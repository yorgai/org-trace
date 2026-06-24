//! `explain`: walk the causal graph backward from an anchor to answer WHY a
//! piece of code looks the way it does — not just WHO last touched it (that is
//! `blame`, a single hop). This is the read side of Brick's causal continuity
//! layer.
//!
//! Two ideas keep this honest:
//!
//! 1. **Edges are built at index time, chains are walked at query time.** The
//!    adjacency tables (`TraceIndex.causes` / `effects`) materialize the *edges*;
//!    a *chain* is relative to an anchor + depth, so we BFS it here on demand
//!    rather than pre-materializing every possible chain (which would explode).
//! 2. **The graph is the goal; the timeline is only a degraded fallback.**
//!    Explicit `causal.linked` edges (`explicit`/`observed`) are the real causal
//!    graph. When an anchor event has no edges at all, we fall back to a shallow
//!    same-session time-ordered guess and label every such step `inferred` — a
//!    timeline, clearly marked as a guess, never dressed up as causality.

use std::collections::{BTreeMap, HashSet, VecDeque};

use serde::{Deserialize, Serialize};

use brick_protocol::{EventType, SourceSessionObservedPayload, TraceEvent};

use crate::blame::blame_file;
use crate::store::LocalStore;
use crate::{CausalEdge, FileSessionBlameRow, TraceIndex};

/// Default backward traversal depth, and the hard cap callers cannot exceed.
pub const DEFAULT_EXPLAIN_DEPTH: usize = 3;
pub const MAX_EXPLAIN_DEPTH: usize = 8;

/// Synthetic event type for a CTP step derived from a metadata-db source session
/// (file-level provenance) rather than a real recorded trace event.
pub const EVENT_TYPE_SOURCE_SESSION: &str = "source.session";

/// How an anchor string was resolved to one or more starting events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnchorKind {
    Event,
    Artifact,
    Mission,
    FileLine,
    File,
}

/// The resolved anchor: what the caller asked about and which events it mapped to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExplainAnchor {
    pub kind: AnchorKind,
    pub input: String,
    pub resolved_events: Vec<String>,
    /// For a `file:line` anchor, the blame confidence that pinned the line to an
    /// event (`commit` / `working` / `unattributed`). `None` for direct anchors.
    pub blame_confidence: Option<String>,
}

/// A pointer the caller can use to fetch the full transcript of a step's session.
/// The core only knows the source app + app session id; turning that into a file
/// path or sqlite ref + excerpt is the CLI/MCP layer's job (it has the filesystem).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TranscriptPointer {
    pub source: Option<String>,
    pub session_ref: Option<String>,
    pub session_id: Option<String>,
    /// A ready-to-run command that dumps THIS session's full trajectory — the
    /// deep-dive pointer. `note` is only the turn's closing narration (an
    /// `observed` summary, often not the root cause); when an agent needs the
    /// real WHY it should run this to read the original session end-to-end.
    /// `None` until the CLI layer resolves the source's on-disk location.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_session: Option<String>,
}

/// One step in a causal chain: an event, who/when produced it, why (the rationale
/// note + the relation to the step it caused), and how confident the attribution is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalStep {
    pub event_id: String,
    pub event_type: String,
    /// Short human title for this step ("modified auth.rs", a session/artifact
    /// title), best-effort from the event metadata.
    pub title: Option<String>,
    pub actor_type: Option<String>,
    pub actor_id: Option<String>,
    pub session_id: Option<String>,
    pub mission_id: Option<String>,
    /// Human-readable mission title resolved from `mission_id`, so the agent sees
    /// the WHY ("Harden token refresh") instead of an opaque id it discards.
    pub mission_title: Option<String>,
    pub occurred_at: String,
    /// Relation of THIS step to the step it caused (the one nearer the anchor).
    /// `None` for the anchor/root steps themselves.
    pub relation: Option<String>,
    /// The WHY: a standalone rationale recorded on this event, when present.
    pub note: Option<String>,
    /// `explicit` (asserted) / `observed` (hook-captured) / `inferred` (fallback).
    pub confidence: String,
    pub transcript: Option<TranscriptPointer>,
    /// Distance from the anchor (0 = anchor itself).
    pub depth: usize,
}

/// A forward effect of the anchor: something derived from / triggered by it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardEffect {
    pub event_id: String,
    pub event_type: String,
    pub title: Option<String>,
    pub relation_to_anchor: Option<String>,
    pub session_id: Option<String>,
}

/// The full result of an `explain` query.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalChain {
    pub anchor: ExplainAnchor,
    #[serde(rename = "causal_chain")]
    pub steps: Vec<CausalStep>,
    pub forward: Vec<ForwardEffect>,
    /// True when traversal hit the depth cap and stopped before exhausting causes.
    pub truncated: bool,
}

/// Builds CTP steps from metadata-db source-session rows (file-level provenance).
///
/// This is the read-time half of "one db, one explain": when the file anchor has
/// no recorded trace events, the metadata db's `source_sessions` (what codex /
/// claude / … touched, already indexed) become the chain. Each row → a step with
/// `confidence="observed"` and a transcript pointer; the WHY (`note`) is left
/// `None` for the CLI/MCP layer to fill from the turn-final assistant message.
///
/// Pure (no I/O) so it unit-tests without a db. `start_depth` lets the caller
/// place these after any real-event steps.
pub fn source_sessions_to_steps(
    rows: &[FileSessionBlameRow],
    start_depth: usize,
) -> Vec<CausalStep> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut steps = Vec::new();
    for row in rows {
        let external = row.external_session_id.clone().unwrap_or_default();
        let source = row
            .app_id
            .clone()
            .or_else(|| row.source_id.clone())
            .unwrap_or_default();
        // Stable synthetic id so repeated rows / re-queries dedupe idempotently.
        let event_id = format!("source-session:{source}:{external}");
        if !seen.insert(event_id.clone()) {
            continue;
        }
        let source_path = row
            .source_pointer
            .as_ref()
            .and_then(|pointer| pointer.get("source_path"))
            .and_then(|value| value.as_str())
            .map(ToOwned::to_owned);
        let file_name = row
            .file_path
            .rsplit('/')
            .next()
            .unwrap_or(row.file_path.as_str());
        // `what` is the session's human title — the same string the user sees in
        // their tool's history. The anchor file is already known from the query,
        // so we don't append a redundant "— touched <file>" suffix. Fall back to a
        // minimal phrasing only when no title was indexed.
        let title = match row
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(title) => title.to_string(),
            None => format!("touched {file_name} in {source} session"),
        };
        steps.push(CausalStep {
            event_id,
            event_type: EVENT_TYPE_SOURCE_SESSION.to_string(),
            title: Some(title),
            actor_type: row.actor_type.clone(),
            actor_id: row.actor_id.clone().or_else(|| Some(source.clone())),
            session_id: (!external.is_empty()).then(|| external.clone()),
            mission_id: None,
            mission_title: None,
            occurred_at: row.last_seen_at.clone(),
            relation: None,
            note: None,
            confidence: "observed".to_string(),
            transcript: Some(TranscriptPointer {
                source: (!source.is_empty()).then(|| source.clone()),
                session_ref: source_path,
                session_id: (!external.is_empty()).then(|| external.clone()),
                read_session: None,
            }),
            depth: start_depth + steps.len(),
        });
    }
    steps
}

/// Merges metadata source-session steps into an existing JSONL causal chain,
/// producing one unified timeline instead of an either/or fallback.
///
/// Why this exists: a file's history is commonly *interleaved* — some changes
/// were `link`ed into the JSONL ledger (the authoritative half) and some were
/// only seen by an external tool's session db (the indexed half). A naive
/// "use JSONL if non-empty, else metadata" drops every un-`link`ed change that
/// happens to sit next to a `link`ed one. This merges both.
///
/// Dedup key is `session_id`: a session that was BOTH `link`ed and indexed
/// appears in both sources, so any `source_steps` whose `session_id` already
/// appears among `chain_steps` is dropped (the JSONL version carries more — an
/// explicit note, mission, relation). When a JSONL step has NO `session_id`
/// (the `link` call omitted `session`), we deliberately do NOT fuzzy-match:
/// keeping a possible duplicate is correct, silently dropping a real change is
/// not. Such pairs are still distinguishable by `confidence` (explicit vs
/// observed).
///
/// Ordering note: JSONL `occurred_at` is the moment the change happened, but a
/// source-session step's `occurred_at` is `last_seen_at` — when the indexer last
/// scanned it, NOT the change moment. Mixing them is acceptable (external
/// sessions only ever carry coarse time) but means a source step can sort later
/// than its true position. The sort is stable, so for equal timestamps the
/// JSONL step (inserted first) stays ahead of the metadata step.
pub fn merge_source_steps_into(chain_steps: &mut Vec<CausalStep>, source_steps: Vec<CausalStep>) {
    let linked_session_ids: HashSet<String> = chain_steps
        .iter()
        .filter_map(|step| step.session_id.clone())
        .collect();
    for step in source_steps {
        let already_linked = step
            .session_id
            .as_ref()
            .is_some_and(|id| linked_session_ids.contains(id));
        if already_linked {
            continue;
        }
        chain_steps.push(step);
    }
    // One unified timeline: stable-sort by occurred_at, then renumber depth so it
    // stays a contiguous 0..N distance-from-anchor sequence after the merge.
    chain_steps.sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at));
    for (depth, step) in chain_steps.iter_mut().enumerate() {
        step.depth = depth;
    }
}

/// Resolves a direct (non-file) anchor string to its starting event-ids.
///
/// - an `event_id` (a raw UUID) resolves to itself if present in the stream;
/// - an `artifact_*` id resolves to the events carrying that artifact;
/// - a `mission_*` id resolves to the events carrying that mission.
pub fn resolve_direct_anchor(events: &[TraceEvent], input: &str) -> ExplainAnchor {
    let trimmed = input.trim();

    if let Some(stripped) = trimmed.strip_prefix("artifact_") {
        let _ = stripped;
        let resolved = events
            .iter()
            .filter(|event| {
                event
                    .artifact_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == trimmed)
            })
            .map(|event| event.event_id.to_string())
            .collect();
        return ExplainAnchor {
            kind: AnchorKind::Artifact,
            input: trimmed.to_string(),
            resolved_events: resolved,
            blame_confidence: None,
        };
    }

    if let Some(stripped) = trimmed.strip_prefix("mission_") {
        let _ = stripped;
        let resolved = events
            .iter()
            .filter(|event| {
                event
                    .mission_id
                    .as_ref()
                    .is_some_and(|id| id.as_str() == trimmed)
            })
            .map(|event| event.event_id.to_string())
            .collect();
        return ExplainAnchor {
            kind: AnchorKind::Mission,
            input: trimmed.to_string(),
            resolved_events: resolved,
            blame_confidence: None,
        };
    }

    // Otherwise treat it as an event-id, resolving to itself if it exists.
    let resolved = events
        .iter()
        .filter(|event| event.event_id.to_string() == trimmed)
        .map(|event| event.event_id.to_string())
        .collect();
    ExplainAnchor {
        kind: AnchorKind::Event,
        input: trimmed.to_string(),
        resolved_events: resolved,
        blame_confidence: None,
    }
}

/// Resolves a whole-file anchor (a path with no `:line`) to the events that
/// changed that file, newest first. Agents very often ask about a file without a
/// specific line ("why does auth.rs look like this"), so this is git-free and
/// matches `diff.captured` events by path suffix rather than treating the path as
/// an opaque id (which wrongly reported "no record").
pub fn resolve_file_anchor(events: &[TraceEvent], path: &str) -> ExplainAnchor {
    let rel = path.trim().trim_start_matches("./");
    let mut matches: Vec<&TraceEvent> = events
        .iter()
        .filter(|event| match event.event_type {
            EventType::DiffCaptured => diff_event_touches(event, rel),
            EventType::SourceSessionObserved => source_session_event_touches(event, rel),
            _ => false,
        })
        .collect();
    matches.sort_by_key(|event| std::cmp::Reverse(event.occurred_at));
    let resolved = matches
        .iter()
        .map(|event| event.event_id.to_string())
        .collect();
    ExplainAnchor {
        kind: AnchorKind::File,
        input: path.to_string(),
        resolved_events: resolved,
        blame_confidence: None,
    }
}

/// Whether a `diff.captured` event's `file_changes` touch `rel` (suffix match,
/// tolerant of repo-relative vs absolute differences).
fn diff_event_touches(event: &TraceEvent, rel: &str) -> bool {
    event
        .payload
        .get("file_changes")
        .and_then(|value| value.as_array())
        .map(|changes| {
            changes.iter().any(|change| {
                change
                    .get("path")
                    .and_then(|p| p.as_str())
                    .map(|path| path == rel || path.ends_with(rel) || rel.ends_with(path))
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

fn source_session_event_touches(event: &TraceEvent, rel: &str) -> bool {
    let Ok(payload) = serde_json::from_value::<SourceSessionObservedPayload>(event.payload.clone())
    else {
        return false;
    };
    payload
        .touched_files
        .iter()
        .any(|path| path == rel || path.ends_with(&format!("/{rel}")) || rel.ends_with(path))
}

/// Resolves a `file:line` anchor to the event that produced that line, reusing
/// line-level `blame` (line → commit → patch-id → event, drift-aware). This is
/// the git-dependent branch kept separate from the pure graph traversal so the
/// latter stays unit-testable without a repo.
pub fn resolve_file_line_anchor(
    store: &LocalStore,
    repo_root: &std::path::Path,
    rel_path: &str,
    line: u64,
) -> anyhow::Result<ExplainAnchor> {
    let lines = blame_file(store, repo_root, rel_path)?;
    let hit = lines.iter().find(|blame| blame.line_no == line);
    let (resolved, confidence) = match hit {
        Some(blame) => {
            let events = blame
                .source_event_id
                .clone()
                .into_iter()
                .collect::<Vec<_>>();
            let confidence = serde_json::to_value(blame.confidence)
                .ok()
                .and_then(|value| value.as_str().map(str::to_string));
            (events, confidence)
        }
        None => (Vec::new(), None),
    };
    Ok(ExplainAnchor {
        kind: AnchorKind::FileLine,
        input: format!("{rel_path}:{line}"),
        resolved_events: resolved,
        blame_confidence: confidence,
    })
}

/// Resolves a `path:start-end` line-RANGE anchor: unions the blame events of
/// every line in `[line_start, line_end]`, so an agent can ask "why does this
/// block (lines 10-20) look like this" and get every change that touched it.
/// Deduplicates events while preserving first-seen order (top-of-range first).
/// `blame_confidence` is the strongest confidence seen across the range.
pub fn resolve_file_range_anchor(
    store: &LocalStore,
    repo_root: &std::path::Path,
    rel_path: &str,
    line_start: u64,
    line_end: u64,
) -> anyhow::Result<ExplainAnchor> {
    let (lo, hi) = if line_start <= line_end {
        (line_start, line_end)
    } else {
        (line_end, line_start)
    };
    let lines = blame_file(store, repo_root, rel_path)?;
    let mut resolved: Vec<String> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut best_rank = 0u8;
    let mut best_confidence: Option<String> = None;
    for blame in lines.iter().filter(|b| b.line_no >= lo && b.line_no <= hi) {
        if let Some(event_id) = blame.source_event_id.clone() {
            if seen.insert(event_id.clone()) {
                resolved.push(event_id);
            }
        }
        let conf = serde_json::to_value(blame.confidence)
            .ok()
            .and_then(|value| value.as_str().map(str::to_string));
        let rank = match conf.as_deref() {
            Some("commit") => 3,
            Some("working") => 2,
            Some(_) => 1,
            None => 0,
        };
        if rank > best_rank {
            best_rank = rank;
            best_confidence = conf;
        }
    }
    Ok(ExplainAnchor {
        kind: AnchorKind::FileLine,
        input: format!("{rel_path}:{lo}-{hi}"),
        resolved_events: resolved,
        blame_confidence: best_confidence,
    })
}

/// Walks the causal graph backward from `anchor`'s resolved events, returning the
/// chain of steps (newest/anchor first) plus the anchor's forward effects.
pub fn explain_from_events(
    index: &TraceIndex,
    events: &[TraceEvent],
    anchor: ExplainAnchor,
    depth: usize,
) -> CausalChain {
    let depth = depth.min(MAX_EXPLAIN_DEPTH);
    let by_id: BTreeMap<String, &TraceEvent> = events
        .iter()
        .map(|event| (event.event_id.to_string(), event))
        .collect();
    let mission_titles = mission_title_index(events);

    let mut steps = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut truncated = false;

    // BFS backward. Each queue item carries the event, the relation by which it
    // caused its parent (None for roots), and its depth from the anchor.
    let mut queue: VecDeque<(String, Option<String>, usize)> = VecDeque::new();
    for root in &anchor.resolved_events {
        queue.push_back((root.clone(), None, 0));
    }

    while let Some((event_id, relation, step_depth)) = queue.pop_front() {
        if !visited.insert(event_id.clone()) {
            continue;
        }

        let edges = index.causes.get(&event_id);
        let (rationale_note, rationale_conf) = rationale_of(edges);
        let step = build_step(
            by_id.get(&event_id).copied(),
            &event_id,
            relation,
            rationale_note,
            rationale_conf,
            step_depth,
            &mission_titles,
        );
        steps.push(step);

        if step_depth >= depth {
            if edges.map(|edges| edges.iter().any(|e| e.cause_event.is_some())) == Some(true) {
                truncated = true;
            }
            continue;
        }

        if let Some(edges) = edges {
            for edge in edges {
                if let Some(cause) = &edge.cause_event {
                    if !visited.contains(cause) {
                        queue.push_back((cause.clone(), Some(relation_name(edge)), step_depth + 1));
                    }
                }
            }
        }
    }

    // If the anchor event has no causal edges at all, degrade to a shallow
    // same-session timeline guess — clearly labelled `inferred`.
    if steps.len() == 1
        && !index
            .causes
            .contains_key(&anchor.resolved_events.first().cloned().unwrap_or_default())
    {
        if let Some(anchor_event) = anchor
            .resolved_events
            .first()
            .and_then(|id| by_id.get(id).copied())
        {
            for inferred in
                inferred_same_session_steps(events, anchor_event, depth, &mission_titles)
            {
                if visited.insert(inferred.event_id.clone()) {
                    steps.push(inferred);
                }
            }
        }
    }

    let forward = forward_effects(index, &by_id, &anchor.resolved_events);

    CausalChain {
        anchor,
        steps,
        forward,
        truncated,
    }
}

/// Extracts the standalone rationale (cause-less edge) note + confidence for an
/// effect event, if one was recorded. Multiple rationales collapse to the first.
fn rationale_of(edges: Option<&Vec<CausalEdge>>) -> (Option<String>, Option<String>) {
    let Some(edges) = edges else {
        return (None, None);
    };
    for edge in edges {
        if edge.cause_event.is_none() {
            return (edge.note.clone(), Some(edge.confidence.clone()));
        }
    }
    // A cross-event edge can also carry a note explaining the link.
    for edge in edges {
        if edge.note.is_some() {
            return (edge.note.clone(), Some(edge.confidence.clone()));
        }
    }
    (None, edges.first().map(|edge| edge.confidence.clone()))
}

fn relation_name(edge: &CausalEdge) -> String {
    serde_json::to_value(edge.relation)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "triggered_by".to_string())
}

fn build_step(
    event: Option<&TraceEvent>,
    event_id: &str,
    relation: Option<String>,
    note: Option<String>,
    rationale_conf: Option<String>,
    depth: usize,
    mission_titles: &BTreeMap<String, String>,
) -> CausalStep {
    match event {
        Some(event) if event.event_type == EventType::SourceSessionObserved => {
            build_source_session_step(event, event_id, relation, note, rationale_conf, depth)
        }
        Some(event) => CausalStep {
            event_id: event_id.to_string(),
            event_type: event_type_wire(event.event_type).to_string(),
            title: describe_event(event),
            actor_type: Some(actor_type_wire(event).to_string()),
            actor_id: Some(event.actor.actor_id.clone()),
            session_id: event.session_id.as_ref().map(ToString::to_string),
            mission_id: event.mission_id.as_ref().map(ToString::to_string),
            mission_title: mission_title_for(event, mission_titles),
            occurred_at: event.occurred_at.to_rfc3339(),
            relation,
            note,
            confidence: rationale_conf.unwrap_or_else(|| confidence_wire(event)),
            transcript: transcript_pointer(event),
            depth,
        },
        // Edge references an event-id not present in the stream (e.g. a dangling
        // cause). Report it honestly rather than dropping the link.
        None => CausalStep {
            event_id: event_id.to_string(),
            event_type: "unknown".to_string(),
            title: None,
            actor_type: None,
            actor_id: None,
            session_id: None,
            mission_id: None,
            mission_title: None,
            occurred_at: String::new(),
            relation,
            note,
            confidence: rationale_conf.unwrap_or_else(|| "unknown".to_string()),
            transcript: None,
            depth,
        },
    }
}

fn build_source_session_step(
    event: &TraceEvent,
    event_id: &str,
    relation: Option<String>,
    note: Option<String>,
    rationale_conf: Option<String>,
    depth: usize,
) -> CausalStep {
    let payload =
        serde_json::from_value::<SourceSessionObservedPayload>(event.payload.clone()).ok();
    let source = payload
        .as_ref()
        .map(|payload| payload.source_id.clone())
        .filter(|source| !source.is_empty());
    let external = payload
        .as_ref()
        .map(|payload| payload.external_session_id.clone())
        .filter(|external| !external.is_empty());
    let source_path = payload
        .as_ref()
        .and_then(|payload| payload.source_path.clone());
    CausalStep {
        event_id: event_id.to_string(),
        event_type: event_type_wire(event.event_type).to_string(),
        title: payload.as_ref().and_then(|payload| payload.title.clone()),
        actor_type: Some(actor_type_wire(event).to_string()),
        actor_id: Some(event.actor.actor_id.clone()),
        session_id: external.clone(),
        mission_id: None,
        mission_title: None,
        occurred_at: payload
            .as_ref()
            .and_then(|payload| payload.session_updated_at.clone())
            .unwrap_or_else(|| event.occurred_at.to_rfc3339()),
        relation,
        note,
        confidence: rationale_conf.unwrap_or_else(|| confidence_wire(event)),
        transcript: Some(TranscriptPointer {
            source,
            session_ref: source_path,
            session_id: external,
            read_session: None,
        }),
        depth,
    }
}

/// Builds a `mission_id` → mission title lookup from `mission.created` /
/// `mission.updated` events, so any step carrying a mission gets a human label.
fn mission_title_index(events: &[TraceEvent]) -> BTreeMap<String, String> {
    let mut titles = BTreeMap::new();
    for event in events {
        if matches!(
            event.event_type,
            EventType::MissionCreated | EventType::MissionUpdated
        ) {
            if let Some(mission_id) = event.mission_id.as_ref() {
                if let Some(title) = event
                    .payload
                    .get("title")
                    .and_then(|value| value.as_str())
                    .filter(|title| !title.is_empty())
                {
                    titles.insert(mission_id.to_string(), title.to_string());
                }
            }
        }
    }
    titles
}

/// Resolves the human mission title for an event's `mission_id`, if known.
fn mission_title_for(
    event: &TraceEvent,
    mission_titles: &BTreeMap<String, String>,
) -> Option<String> {
    event
        .mission_id
        .as_ref()
        .and_then(|mission_id| mission_titles.get(&mission_id.to_string()).cloned())
}

/// Shallow fallback: when an anchor event has no causal edges, surface the most
/// recent prior events in the SAME session as `inferred` context. This is a
/// timeline, not a causal chain — every step is marked `inferred`.
fn inferred_same_session_steps(
    events: &[TraceEvent],
    anchor_event: &TraceEvent,
    depth: usize,
    mission_titles: &BTreeMap<String, String>,
) -> Vec<CausalStep> {
    let Some(session) = anchor_event.session_id.as_ref() else {
        return Vec::new();
    };
    let mut prior: Vec<&TraceEvent> = events
        .iter()
        .filter(|event| {
            event.session_id.as_ref() == Some(session)
                && event.event_id != anchor_event.event_id
                && event.occurred_at <= anchor_event.occurred_at
        })
        .collect();
    prior.sort_by_key(|event| std::cmp::Reverse(event.occurred_at));
    prior
        .into_iter()
        .take(depth)
        .enumerate()
        .map(|(offset, event)| CausalStep {
            event_id: event.event_id.to_string(),
            event_type: event_type_wire(event.event_type).to_string(),
            title: describe_event(event),
            actor_type: Some(actor_type_wire(event).to_string()),
            actor_id: Some(event.actor.actor_id.clone()),
            session_id: event.session_id.as_ref().map(ToString::to_string),
            mission_id: event.mission_id.as_ref().map(ToString::to_string),
            mission_title: mission_title_for(event, mission_titles),
            occurred_at: event.occurred_at.to_rfc3339(),
            relation: Some("inferred_prior".to_string()),
            note: None,
            confidence: "inferred".to_string(),
            transcript: transcript_pointer(event),
            depth: offset + 1,
        })
        .collect()
}

fn forward_effects(
    index: &TraceIndex,
    by_id: &BTreeMap<String, &TraceEvent>,
    roots: &[String],
) -> Vec<ForwardEffect> {
    let mut effects = Vec::new();
    let mut seen = HashSet::new();
    for root in roots {
        let Some(downstream) = index.effects.get(root) else {
            continue;
        };
        for effect_id in downstream {
            if !seen.insert(effect_id.clone()) {
                continue;
            }
            let relation = index.causes.get(effect_id).and_then(|edges| {
                edges
                    .iter()
                    .find(|edge| edge.cause_event.as_deref() == Some(root.as_str()))
                    .map(relation_name)
            });
            let event = by_id.get(effect_id).copied();
            effects.push(ForwardEffect {
                event_id: effect_id.clone(),
                event_type: event
                    .map(|event| event_type_wire(event.event_type).to_string())
                    .unwrap_or_else(|| "unknown".to_string()),
                title: event.and_then(describe_event),
                relation_to_anchor: relation,
                session_id: event
                    .and_then(|event| event.session_id.as_ref().map(ToString::to_string)),
            });
        }
    }
    effects
}

fn transcript_pointer(event: &TraceEvent) -> Option<TranscriptPointer> {
    let session_id = event.session_id.as_ref().map(ToString::to_string);
    session_id.as_ref()?;
    Some(TranscriptPointer {
        source: None,
        session_ref: None,
        session_id,
        read_session: None,
    })
}

/// Best-effort one-line description of an event for the `what` field.
fn describe_event(event: &TraceEvent) -> Option<String> {
    match event.event_type {
        EventType::DiffCaptured => {
            let paths: Vec<String> = event
                .payload
                .get("file_changes")
                .and_then(|value| value.as_array())
                .map(|changes| {
                    changes
                        .iter()
                        .filter_map(|change| {
                            change
                                .get("path")
                                .and_then(|p| p.as_str())
                                .map(str::to_string)
                        })
                        .collect()
                })
                .unwrap_or_default();
            if paths.is_empty() {
                Some("captured a diff".to_string())
            } else {
                Some(format!("changed {}", paths.join(", ")))
            }
        }
        EventType::ArtifactCreated | EventType::ArtifactUpdated => event
            .payload
            .get("title")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        EventType::MissionCreated | EventType::MissionUpdated => event
            .payload
            .get("title")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        EventType::SourceSessionObserved => event
            .payload
            .get("title")
            .and_then(|value| value.as_str())
            .map(str::to_string),
        _ => None,
    }
}

fn event_type_wire(event_type: EventType) -> &'static str {
    serde_event_str(event_type).unwrap_or("unknown")
}

fn serde_event_str(event_type: EventType) -> Option<&'static str> {
    match event_type {
        EventType::OrgCreated => Some("org.created"),
        EventType::OrgUpdated => Some("org.updated"),
        EventType::ProjectCreated => Some("project.created"),
        EventType::ProjectUpdated => Some("project.updated"),
        EventType::MissionCreated => Some("mission.created"),
        EventType::MissionUpdated => Some("mission.updated"),
        EventType::SessionStarted => Some("session.started"),
        EventType::SessionLinkedToMission => Some("session.linked_to_mission"),
        EventType::SessionLogUploaded => Some("session.log_uploaded"),
        EventType::ArtifactCreated => Some("artifact.created"),
        EventType::ArtifactUpdated => Some("artifact.updated"),
        EventType::ArtifactLinkedToMission => Some("artifact.linked_to_mission"),
        EventType::ArtifactFileRefRecorded => Some("artifact.file_ref_recorded"),
        EventType::ArtifactAttachmentUploaded => Some("artifact.attachment_uploaded"),
        EventType::ArtifactReviewed => Some("artifact.reviewed"),
        EventType::ArtifactAccepted => Some("artifact.accepted"),
        EventType::RepoContextCaptured => Some("repo_context.captured"),
        EventType::DiffCaptured => Some("diff.captured"),
        EventType::ExternalRefLinked => Some("external_ref.linked"),
        EventType::SourceSessionObserved => Some("source.session_observed"),
        EventType::CausalLinked => Some("causal.linked"),
    }
}

fn actor_type_wire(event: &TraceEvent) -> &'static str {
    serde_json::to_value(event.actor.actor_type)
        .ok()
        .and_then(|value| value.as_str().map(actor_static))
        .unwrap_or("system")
}

fn actor_static(value: &str) -> &'static str {
    match value {
        "human" => "human",
        "agent" => "agent",
        _ => "system",
    }
}

fn confidence_wire(event: &TraceEvent) -> String {
    serde_json::to_value(event.confidence)
        .ok()
        .and_then(|value| value.as_str().map(str::to_string))
        .unwrap_or_else(|| "unknown".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use brick_protocol::{
        ActorRef, ActorType, CausalLinkedPayload, CausalRelation, ConfidenceLevel,
        DiffCapturedPayload, DiffFileChange, DiffFileChangeKind, DiffTarget, SessionId,
        SourceSessionObservedPayload,
    };
    use uuid::Uuid;

    fn actor() -> ActorRef {
        ActorRef {
            actor_type: ActorType::Agent,
            actor_id: "claude".to_string(),
            display_name: None,
        }
    }

    fn diff_event(session: &SessionId, path: &str) -> TraceEvent {
        let mut event = TraceEvent::diff_captured(
            actor(),
            brick_protocol::ArtifactId::new(),
            Some(session.clone()),
            None,
            DiffCapturedPayload {
                diff_target: DiffTarget::Working,
                base_commit: None,
                head_commit: None,
                patch_id: None,
                summary_hash: "h".to_string(),
                file_changes: vec![DiffFileChange {
                    path: path.to_string(),
                    old_path: None,
                    change_kind: DiffFileChangeKind::Modified,
                    additions: Some(8),
                    deletions: Some(0),
                    hunks: vec![],
                    patch_id: None,
                }],
                repo_context_id: None,
            },
        )
        .expect("diff event");
        event.occurred_at = chrono::Utc::now();
        event
    }

    fn source_session_event(path: &str) -> TraceEvent {
        TraceEvent::source_session_observed(
            actor(),
            SourceSessionObservedPayload {
                source_id: "orgii".to_string(),
                external_session_id: "session-1".to_string(),
                title: Some("Investigate sync design".to_string()),
                source_path: Some("/Users/me/.orgii/sessions.db".to_string()),
                source_uri: Some("file:///Users/me/.orgii/sessions.db".to_string()),
                source_mtime: None,
                source_size: Some(123),
                source_fingerprint: Some("fp".to_string()),
                parser_version: Some("test".to_string()),
                session_created_at: Some("2026-06-23T00:00:00Z".to_string()),
                session_updated_at: Some("2026-06-23T00:01:00Z".to_string()),
                model: Some("test-model".to_string()),
                input_tokens: Some(10),
                output_tokens: Some(20),
                repo_path: Some("/repo".to_string()),
                branch: Some("main".to_string()),
                files_changed: Some(1),
                lines_added: Some(2),
                lines_removed: Some(0),
                touched_files: vec![path.to_string()],
                metadata_json: None,
            },
        )
        .expect("source session event")
    }

    fn causal(
        effect: Uuid,
        causes: Vec<Uuid>,
        relation: CausalRelation,
        note: &str,
        confidence: ConfidenceLevel,
    ) -> TraceEvent {
        TraceEvent::causal_linked(
            actor(),
            confidence,
            CausalLinkedPayload {
                effect_event: effect,
                cause_events: causes,
                relation,
                note: Some(note.to_string()),
                repo_context_id: None,
            },
        )
        .expect("causal edge")
    }

    #[test]
    fn whole_file_anchor_resolves_source_session_events() {
        let event = source_session_event("crates/core/src/explain.rs");
        let event_id = event.event_id.to_string();
        let events = vec![event];
        let index = TraceIndex::build(&events).expect("index");

        let anchor = resolve_file_anchor(&events, "src/explain.rs");
        assert_eq!(anchor.resolved_events, vec![event_id.clone()]);
        let chain = explain_from_events(&index, &events, anchor, DEFAULT_EXPLAIN_DEPTH);

        assert_eq!(chain.steps.len(), 1);
        assert_eq!(chain.steps[0].event_id, event_id);
        assert_eq!(chain.steps[0].event_type, "source.session_observed");
        assert_eq!(
            chain.steps[0].title.as_deref(),
            Some("Investigate sync design")
        );
        assert_eq!(chain.steps[0].session_id.as_deref(), Some("session-1"));
        assert_eq!(
            chain.steps[0]
                .transcript
                .as_ref()
                .and_then(|t| t.source.as_deref()),
            Some("orgii")
        );
    }

    #[test]
    fn linear_chain_walks_back_with_rationale_and_relation() {
        let session = SessionId::new();
        let e2 = diff_event(&session, "auth.rs");
        let e4 = diff_event(&session, "test_auth.rs");
        let e2_id = e2.event_id;
        let e4_id = e4.event_id;

        let rationale = causal(
            e2_id,
            vec![],
            CausalRelation::Rationale,
            "token refresh race",
            ConfidenceLevel::Observed,
        );
        let derived = causal(
            e4_id,
            vec![e2_id],
            CausalRelation::DerivedFrom,
            "covers the race fix",
            ConfidenceLevel::Explicit,
        );
        let events = vec![e2, e4, rationale, derived];
        let index = TraceIndex::build(&events).expect("index");

        let anchor = resolve_direct_anchor(&events, &e4_id.to_string());
        assert_eq!(anchor.kind, AnchorKind::Event);
        let chain = explain_from_events(&index, &events, anchor, DEFAULT_EXPLAIN_DEPTH);

        // root e4 (relation None) then e2 (derived_from).
        assert_eq!(chain.steps.len(), 2);
        assert_eq!(chain.steps[0].event_id, e4_id.to_string());
        assert_eq!(chain.steps[0].relation, None);
        assert_eq!(chain.steps[1].event_id, e2_id.to_string());
        assert_eq!(chain.steps[1].relation.as_deref(), Some("derived_from"));
        assert_eq!(chain.steps[1].note.as_deref(), Some("token refresh race"));
        assert_eq!(chain.steps[1].confidence, "observed");
        assert_eq!(chain.steps[1].title.as_deref(), Some("changed auth.rs"));
        assert!(!chain.truncated);
    }

    #[test]
    fn forward_effects_surface_downstream_derivations() {
        let session = SessionId::new();
        let e2 = diff_event(&session, "auth.rs");
        let e4 = diff_event(&session, "test_auth.rs");
        let e2_id = e2.event_id;
        let e4_id = e4.event_id;
        let derived = causal(
            e4_id,
            vec![e2_id],
            CausalRelation::DerivedFrom,
            "covers fix",
            ConfidenceLevel::Explicit,
        );
        let events = vec![e2, e4, derived];
        let index = TraceIndex::build(&events).expect("index");

        let anchor = resolve_direct_anchor(&events, &e2_id.to_string());
        let chain = explain_from_events(&index, &events, anchor, DEFAULT_EXPLAIN_DEPTH);
        assert_eq!(chain.forward.len(), 1);
        assert_eq!(chain.forward[0].event_id, e4_id.to_string());
        assert_eq!(
            chain.forward[0].relation_to_anchor.as_deref(),
            Some("derived_from")
        );
    }

    #[test]
    fn cycle_does_not_loop_forever() {
        let session = SessionId::new();
        let a = diff_event(&session, "a.rs");
        let b = diff_event(&session, "b.rs");
        let a_id = a.event_id;
        let b_id = b.event_id;
        // a caused by b, and b caused by a (degenerate cycle).
        let ab = causal(
            a_id,
            vec![b_id],
            CausalRelation::TriggeredBy,
            "x",
            ConfidenceLevel::Explicit,
        );
        let ba = causal(
            b_id,
            vec![a_id],
            CausalRelation::TriggeredBy,
            "y",
            ConfidenceLevel::Explicit,
        );
        let events = vec![a, b, ab, ba];
        let index = TraceIndex::build(&events).expect("index");

        let anchor = resolve_direct_anchor(&events, &a_id.to_string());
        let chain = explain_from_events(&index, &events, anchor, MAX_EXPLAIN_DEPTH);
        // Visited set bounds it: exactly the two distinct events.
        assert_eq!(chain.steps.len(), 2);
    }

    #[test]
    fn depth_cap_truncates() {
        let session = SessionId::new();
        let a = diff_event(&session, "a.rs");
        let b = diff_event(&session, "b.rs");
        let c = diff_event(&session, "c.rs");
        let (a_id, b_id, c_id) = (a.event_id, b.event_id, c.event_id);
        let ab = causal(
            a_id,
            vec![b_id],
            CausalRelation::DerivedFrom,
            "x",
            ConfidenceLevel::Explicit,
        );
        let bc = causal(
            b_id,
            vec![c_id],
            CausalRelation::DerivedFrom,
            "y",
            ConfidenceLevel::Explicit,
        );
        let events = vec![a, b, c, ab, bc];
        let index = TraceIndex::build(&events).expect("index");

        let anchor = resolve_direct_anchor(&events, &a_id.to_string());
        let chain = explain_from_events(&index, &events, anchor, 1);
        // depth 1: anchor a (depth 0) + b (depth 1); c is beyond the cap.
        assert_eq!(chain.steps.len(), 2);
        assert!(chain.truncated);
    }

    #[test]
    fn no_edges_falls_back_to_inferred_timeline() {
        let session = SessionId::new();
        let mut e1 = diff_event(&session, "early.rs");
        e1.occurred_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        let e2 = diff_event(&session, "anchor.rs");
        let e2_id = e2.event_id;
        // No causal edges at all.
        let events = vec![e1, e2];
        let index = TraceIndex::build(&events).expect("index");

        let anchor = resolve_direct_anchor(&events, &e2_id.to_string());
        let chain = explain_from_events(&index, &events, anchor, DEFAULT_EXPLAIN_DEPTH);
        // anchor + one inferred prior step, clearly labelled.
        assert!(chain.steps.len() >= 2);
        assert!(chain.steps.iter().any(|step| step.confidence == "inferred"));
    }

    #[test]
    fn artifact_anchor_resolves_to_its_events() {
        let session = SessionId::new();
        let e2 = diff_event(&session, "auth.rs");
        let artifact_id = e2.artifact_id.clone().expect("artifact id");
        let events = vec![e2];
        let anchor = resolve_direct_anchor(&events, artifact_id.as_str());
        assert_eq!(anchor.kind, AnchorKind::Artifact);
        assert_eq!(anchor.resolved_events.len(), 1);
    }

    /// Regression: an agent very often anchors on a whole file (no `:line`), e.g.
    /// `explain src/auth.rs`. That must resolve to the file's change events, not
    /// be treated as an opaque id reporting "no record". Newest diff comes first.
    #[test]
    fn whole_file_anchor_resolves_to_file_change_events_newest_first() {
        let session = SessionId::new();
        let mut older = diff_event(&session, "src/auth.rs");
        older.occurred_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        let newer = diff_event(&session, "src/auth.rs");
        let newer_id = newer.event_id.to_string();
        let unrelated = diff_event(&session, "src/other.rs");
        let events = vec![older, newer, unrelated];

        let anchor = resolve_file_anchor(&events, "src/auth.rs");
        assert_eq!(anchor.kind, AnchorKind::File);
        assert_eq!(anchor.resolved_events.len(), 2, "both auth.rs diffs match");
        assert_eq!(
            anchor.resolved_events[0], newer_id,
            "newest diff resolves first"
        );

        // And it walks into a populated chain rather than an empty one.
        let index = TraceIndex::build(&events).expect("index");
        let chain = explain_from_events(&index, &events, anchor, DEFAULT_EXPLAIN_DEPTH);
        assert!(!chain.steps.is_empty(), "whole-file anchor yields a chain");
        assert_eq!(chain.steps[0].actor_id.as_deref(), Some("claude"));
    }

    /// Regression: a step carrying a `mission_id` must expose the human
    /// `mission_title` so the agent sees the WHY instead of an opaque id it
    /// discards (which made it wrongly fall back to git).
    #[test]
    fn step_resolves_mission_title_from_mission_event() {
        let session = SessionId::new();
        let project = brick_protocol::ProjectId::new();
        let mission_id = brick_protocol::MissionId::new();
        let mission = TraceEvent::mission_created(
            actor(),
            mission_id.clone(),
            brick_protocol::MissionCreatedPayload {
                project_id: project,
                title: "Harden token refresh".to_string(),
                description: None,
                status: brick_protocol::MissionStatus::Active,
                repo_context_id: None,
            },
        )
        .expect("mission event");

        // A diff carrying that mission.
        let mut diff = TraceEvent::diff_captured(
            actor(),
            brick_protocol::ArtifactId::new(),
            Some(session.clone()),
            Some(mission_id.clone()),
            DiffCapturedPayload {
                diff_target: DiffTarget::Working,
                base_commit: None,
                head_commit: None,
                patch_id: None,
                summary_hash: "h".to_string(),
                file_changes: vec![DiffFileChange {
                    path: "src/auth.rs".to_string(),
                    old_path: None,
                    change_kind: DiffFileChangeKind::Modified,
                    additions: Some(1),
                    deletions: Some(0),
                    hunks: vec![],
                    patch_id: None,
                }],
                repo_context_id: None,
            },
        )
        .expect("diff event");
        diff.occurred_at = chrono::Utc::now();
        let diff_id = diff.event_id.to_string();

        let events = vec![mission, diff];
        let index = TraceIndex::build(&events).expect("index");
        let anchor = resolve_direct_anchor(&events, &diff_id);
        let chain = explain_from_events(&index, &events, anchor, DEFAULT_EXPLAIN_DEPTH);

        let step = chain
            .steps
            .iter()
            .find(|step| step.event_id == diff_id)
            .expect("diff step");
        assert_eq!(
            step.mission_title.as_deref(),
            Some("Harden token refresh"),
            "mission title must be resolved for the agent"
        );
    }

    fn blame_row(source: &str, external: &str, file: &str, repo: &str) -> FileSessionBlameRow {
        FileSessionBlameRow {
            file_path: file.to_string(),
            session_id: None,
            external_session_id: Some(external.to_string()),
            source_id: Some(source.to_string()),
            app_id: Some(source.to_string()),
            actor_id: Some("agent-x".to_string()),
            actor_type: Some("agent".to_string()),
            evidence_kind: crate::FileSessionBlameEvidenceKind::SourceMetadata,
            last_seen_at: "2026-06-20T10:00:00+00:00".to_string(),
            title: None,
            lines_added: Some(3),
            lines_removed: Some(1),
            files_changed: Some(1),
            confidence: Some("metadata_only".to_string()),
            source_pointer: Some(serde_json::json!({
                "source_path": format!("/sessions/{external}.jsonl"),
                "repo_path": repo,
            })),
        }
    }

    #[test]
    fn source_sessions_to_steps_maps_rows_to_observed_steps() {
        let rows = vec![blame_row(
            "codex_app",
            "sess-1",
            "/repo/src/merge.rs",
            "/repo",
        )];
        let steps = source_sessions_to_steps(&rows, 0);
        assert_eq!(steps.len(), 1);
        let step = &steps[0];
        assert_eq!(step.event_type, EVENT_TYPE_SOURCE_SESSION);
        assert_eq!(step.confidence, "observed");
        assert_eq!(
            step.title.as_deref(),
            Some("touched merge.rs in codex_app session")
        );
        assert_eq!(step.session_id.as_deref(), Some("sess-1"));
        assert_eq!(step.note, None, "WHY is filled later from the transcript");
        let transcript = step.transcript.as_ref().expect("transcript pointer");
        assert_eq!(transcript.source.as_deref(), Some("codex_app"));
        assert_eq!(
            transcript.session_ref.as_deref(),
            Some("/sessions/sess-1.jsonl")
        );
        assert_eq!(transcript.session_id.as_deref(), Some("sess-1"));
    }

    #[test]
    fn source_sessions_to_steps_dedupes_by_synthetic_id_and_honors_start_depth() {
        let rows = vec![
            blame_row("codex_app", "dup", "/repo/a.rs", "/repo"),
            blame_row("codex_app", "dup", "/repo/a.rs", "/repo"), // same source+external
            blame_row("claude_code", "other", "/repo/a.rs", "/repo"),
        ];
        let steps = source_sessions_to_steps(&rows, 5);
        assert_eq!(steps.len(), 2, "duplicate synthetic ids collapse");
        assert_eq!(steps[0].depth, 5);
        assert_eq!(steps[1].depth, 6);
    }

    #[test]
    fn source_sessions_to_steps_empty_input_is_empty() {
        assert!(source_sessions_to_steps(&[], 0).is_empty());
    }

    #[test]
    fn source_sessions_to_steps_what_uses_title_when_present() {
        let mut row = blame_row("orgii", "s1", "/repo/src/types.ts", "/repo");
        row.title = Some("Cache git status lookups".to_string());
        let steps = source_sessions_to_steps(&[row], 0);
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].title.as_deref(),
            Some("Cache git status lookups"),
            "a session with a title must get the title verbatim as its what"
        );
    }

    #[test]
    fn source_sessions_to_steps_what_falls_back_without_title() {
        // blame_row leaves title None → generic phrasing.
        let row = blame_row("orgii", "s2", "/repo/src/types.ts", "/repo");
        let steps = source_sessions_to_steps(&[row], 0);
        assert_eq!(steps.len(), 1);
        assert_eq!(
            steps[0].title.as_deref(),
            Some("touched types.ts in orgii session"),
            "no title → fall back to the generic phrasing"
        );
    }

    /// Builds a minimal CausalStep for merge tests.
    fn step(
        event_id: &str,
        session_id: Option<&str>,
        occurred_at: &str,
        confidence: &str,
    ) -> CausalStep {
        CausalStep {
            event_id: event_id.to_string(),
            event_type: "test".to_string(),
            title: None,
            actor_type: None,
            actor_id: None,
            session_id: session_id.map(ToOwned::to_owned),
            mission_id: None,
            mission_title: None,
            occurred_at: occurred_at.to_string(),
            relation: None,
            note: None,
            confidence: confidence.to_string(),
            transcript: None,
            depth: 0,
        }
    }

    #[test]
    fn merge_interleaves_linked_and_source_steps_by_time() {
        // The exact bug the merge fixes: change 1 (source only) → change 2
        // (linked) → change 3 (source only). The old fill-if-empty fallback
        // dropped 1 and 3 because the chain already had the linked step 2.
        let mut chain = vec![step(
            "link-2",
            Some("s2"),
            "2026-06-22T10:02:00Z",
            "explicit",
        )];
        let source = vec![
            step(
                "source-session:codex:s1",
                Some("s1"),
                "2026-06-22T10:01:00Z",
                "observed",
            ),
            step(
                "source-session:codex:s3",
                Some("s3"),
                "2026-06-22T10:03:00Z",
                "observed",
            ),
        ];
        merge_source_steps_into(&mut chain, source);
        let order: Vec<&str> = chain.iter().map(|s| s.event_id.as_str()).collect();
        assert_eq!(
            order,
            vec![
                "source-session:codex:s1",
                "link-2",
                "source-session:codex:s3"
            ],
            "all three changes must appear, time-ordered"
        );
        // depth renumbered contiguously 0..N.
        assert_eq!(
            chain.iter().map(|s| s.depth).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn merge_dedups_source_step_already_linked_by_session_id() {
        // The same session was BOTH linked and indexed — keep the richer linked
        // version, drop the source duplicate.
        let mut chain = vec![step(
            "link-a",
            Some("sA"),
            "2026-06-22T10:00:00Z",
            "explicit",
        )];
        let source = vec![step(
            "source-session:codex:sA",
            Some("sA"),
            "2026-06-22T10:00:00Z",
            "observed",
        )];
        merge_source_steps_into(&mut chain, source);
        assert_eq!(chain.len(), 1, "duplicate session must be deduped");
        assert_eq!(chain[0].event_id, "link-a");
    }

    #[test]
    fn merge_keeps_both_when_linked_step_has_no_session_id() {
        // link without a `session` arg → no session_id → no fuzzy dedup; keep
        // both rather than risk dropping a real change.
        let mut chain = vec![step("link-x", None, "2026-06-22T10:00:00Z", "explicit")];
        let source = vec![step(
            "source-session:codex:sX",
            Some("sX"),
            "2026-06-22T10:00:30Z",
            "observed",
        )];
        merge_source_steps_into(&mut chain, source);
        assert_eq!(chain.len(), 2, "no session_id means no dedup — keep both");
    }
}
