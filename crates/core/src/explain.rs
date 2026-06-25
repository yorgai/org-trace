//! `explain`: a file-history timeline. Given an anchor (a file, a `file:line`,
//! or a direct event/artifact/mission id), return the sessions/changes that
//! touched it in reverse-chronological order — newest first (depth 0), older
//! changes pushed further out. This answers "why does this code look the way it
//! does" by showing WHO changed it, WHEN, under which mission, with a transcript
//! pointer to read the full session — not via a separately-recorded causal graph.
//!
//! The anchor resolvers (`resolve_file_anchor`, blame-backed `file:line`, direct
//! id) already produce the matching events newest-first; `explain_from_events`
//! just turns each into a timeline step and caps the count at `depth`.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};

use brick_protocol::{EventType, SourceSessionObservedPayload, TraceEvent};

use crate::blame::blame_file;
use crate::store::LocalStore;
use crate::FileSessionBlameRow;

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

/// One step in a file-history timeline: an event/session, who/when produced it,
/// the mission it belonged to, and a transcript pointer to read the full session.
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
    /// Left `None` by the core timeline builder. The CLI/MCP enrichment layer
    /// may later fill it with a transcript-inferred relation when the session's
    /// turn-final narration named a cause entity that actually exists in the
    /// ledger (see `infer_session_rationale`); an unresolvable reference is
    /// dropped rather than fabricated.
    pub relation: Option<String>,
    /// The WHY: a turn-final rationale recovered from the session transcript by
    /// the CLI/MCP layer, when present.
    pub note: Option<String>,
    /// `observed` (transcript-captured) / `inferred` (heuristic) / a blame
    /// confidence for line anchors.
    pub confidence: String,
    pub transcript: Option<TranscriptPointer>,
    /// Distance from the anchor in the timeline (0 = newest / anchor itself).
    pub depth: usize,
}

/// The full result of an `explain` query: a newest-first timeline of the
/// sessions/changes that touched the anchor.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalChain {
    pub anchor: ExplainAnchor,
    #[serde(rename = "causal_chain")]
    pub steps: Vec<CausalStep>,
    /// True when the timeline hit the depth cap and older changes were omitted.
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

/// Whether a recorded `path` refers to the same file as the anchor `rel`,
/// tolerant of repo-relative-vs-absolute differences but WITHOUT the
/// false-positive bare-suffix trap.
///
/// Matching is path-component aware: `rel` matches `path` when they are equal,
/// or when one is a trailing *component* sequence of the other (i.e. the
/// boundary is a `/`). So anchor `lib.rs` matches recorded `src/lib.rs` and
/// `core/src/lib.rs`, but `auth.rs` does NOT match `oauth.rs`, and `lib.rs`
/// does not spuriously collide via a raw `ends_with`. Both sides are compared
/// after trimming a leading `./`.
fn path_matches_rel(path: &str, rel: &str) -> bool {
    let path = path.strip_prefix("./").unwrap_or(path);
    let rel = rel.strip_prefix("./").unwrap_or(rel);
    if path == rel {
        return true;
    }
    // `rel` is a trailing component-suffix of `path` (anchor shorter), or vice
    // versa (recorded path shorter) — always anchored at a `/` boundary.
    path.ends_with(&format!("/{rel}")) || rel.ends_with(&format!("/{path}"))
}

/// Whether a `diff.captured` event's `file_changes` touch `rel`.
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
                    .map(|path| path_matches_rel(path, rel))
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
        .any(|path| path_matches_rel(path, rel))
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

/// Builds the file-history timeline for `anchor`: each resolved event becomes a
/// step, newest first (the resolvers already sort newest-first), capped at
/// `depth`. `truncated` is set when older changes were dropped at the cap.
pub fn explain_from_events(
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
    let mut seen: HashSet<String> = HashSet::new();
    let mut truncated = false;

    // The anchor's resolved events are already newest-first. Cap at depth+1 (the
    // anchor itself is depth 0, then `depth` older changes); anything beyond is a
    // truncation.
    let limit = depth.saturating_add(1);
    for event_id in &anchor.resolved_events {
        if !seen.insert(event_id.clone()) {
            continue;
        }
        if steps.len() >= limit {
            truncated = true;
            break;
        }
        let step = build_step(
            by_id.get(event_id).copied(),
            event_id,
            steps.len(),
            &mission_titles,
        );
        steps.push(step);
    }

    CausalChain {
        anchor,
        steps,
        truncated,
    }
}

fn build_step(
    event: Option<&TraceEvent>,
    event_id: &str,
    depth: usize,
    mission_titles: &BTreeMap<String, String>,
) -> CausalStep {
    match event {
        Some(event) if event.event_type == EventType::SourceSessionObserved => {
            build_source_session_step(event, event_id, depth)
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
            relation: None,
            note: None,
            confidence: confidence_wire(event),
            transcript: transcript_pointer(event),
            depth,
        },
        // Anchor resolved to an event-id not present in the stream (e.g. a stale
        // blame pointer). Report it honestly rather than dropping the step.
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
            relation: None,
            note: None,
            confidence: "unknown".to_string(),
            transcript: None,
            depth,
        },
    }
}

fn build_source_session_step(event: &TraceEvent, event_id: &str, depth: usize) -> CausalStep {
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
        relation: None,
        note: None,
        confidence: confidence_wire(event),
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
        ActorRef, ActorType, DiffCapturedPayload, DiffFileChange, DiffFileChangeKind, DiffTarget,
        SessionId, SourceSessionObservedPayload,
    };

    fn actor() -> ActorRef {
        ActorRef {
            actor_type: ActorType::Agent,
            actor_id: "claude".to_string(),
            display_name: None,
        }
    }

    #[test]
    fn path_matches_rel_is_component_aware_no_false_positives() {
        // Equal and ./-normalized.
        assert!(path_matches_rel("src/lib.rs", "src/lib.rs"));
        assert!(path_matches_rel("./src/lib.rs", "src/lib.rs"));
        // Anchor is a trailing component-suffix of the recorded path.
        assert!(path_matches_rel("crates/core/src/lib.rs", "lib.rs"));
        assert!(path_matches_rel("crates/core/src/lib.rs", "src/lib.rs"));
        // Recorded path is the shorter side (absolute-vs-relative tolerance).
        assert!(path_matches_rel("lib.rs", "crates/core/src/lib.rs"));
        // NOT a bare-suffix false positive: oauth.rs must not match auth.rs.
        assert!(!path_matches_rel("src/oauth.rs", "auth.rs"));
        // Different dirs, same basename, neither a component-suffix of the
        // other → no collision.
        assert!(!path_matches_rel("tests/lib.rs", "src/lib.rs"));
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
                name: None,
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
                normalized_chunks: Vec::new(),
            },
        )
        .expect("source session event")
    }

    #[test]
    fn whole_file_anchor_resolves_source_session_events() {
        let event = source_session_event("crates/core/src/explain.rs");
        let event_id = event.event_id.to_string();
        let events = vec![event];

        let anchor = resolve_file_anchor(&events, "src/explain.rs");
        assert_eq!(anchor.resolved_events, vec![event_id.clone()]);
        let chain = explain_from_events(&events, anchor, DEFAULT_EXPLAIN_DEPTH);

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

    /// The timeline lists every event the anchor resolved to, newest first.
    #[test]
    fn timeline_lists_resolved_events_newest_first() {
        let session = SessionId::new();
        let mut older = diff_event(&session, "auth.rs");
        older.occurred_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        let newer = diff_event(&session, "auth.rs");
        let (older_id, newer_id) = (older.event_id.to_string(), newer.event_id.to_string());
        let events = vec![older, newer];

        let anchor = resolve_file_anchor(&events, "auth.rs");
        let chain = explain_from_events(&events, anchor, DEFAULT_EXPLAIN_DEPTH);

        // newest first (depth 0), older next.
        assert_eq!(chain.steps.len(), 2);
        assert_eq!(chain.steps[0].event_id, newer_id);
        assert_eq!(chain.steps[0].depth, 0);
        assert_eq!(chain.steps[1].event_id, older_id);
        assert_eq!(chain.steps[1].depth, 1);
        assert_eq!(chain.steps[0].relation, None);
        assert_eq!(chain.steps[0].title.as_deref(), Some("changed auth.rs"));
        assert!(!chain.truncated);
    }

    #[test]
    fn depth_cap_truncates() {
        let session = SessionId::new();
        let mut a = diff_event(&session, "auth.rs");
        let mut b = diff_event(&session, "auth.rs");
        let c = diff_event(&session, "auth.rs");
        a.occurred_at = chrono::Utc::now() - chrono::Duration::seconds(120);
        b.occurred_at = chrono::Utc::now() - chrono::Duration::seconds(60);
        let events = vec![a, b, c];

        let anchor = resolve_file_anchor(&events, "auth.rs");
        let chain = explain_from_events(&events, anchor, 1);
        // depth 1: newest (depth 0) + one older (depth 1); the third is truncated.
        assert_eq!(chain.steps.len(), 2);
        assert!(chain.truncated);
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
        let chain = explain_from_events(&events, anchor, DEFAULT_EXPLAIN_DEPTH);
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
        let anchor = resolve_direct_anchor(&events, &diff_id);
        let chain = explain_from_events(&events, anchor, DEFAULT_EXPLAIN_DEPTH);

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
