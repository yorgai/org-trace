//! Minimal MCP (Model Context Protocol) server over stdio.
//!
//! MCP is JSON-RPC 2.0 framed one message per line on stdin/stdout. This module
//! implements just enough of it — `initialize`, `tools/list`, `tools/call`, and
//! the `notifications/initialized` ack — to expose Brick's cross-tool memory as
//! agent-callable tools. All tool logic reuses the same `build_*` functions the
//! CLI uses, so MCP and CLI never drift.
//!
//! Protocol invariant: stdout carries ONLY JSON-RPC; every diagnostic goes to
//! stderr. A stray println on stdout corrupts the framing and breaks the client.

use std::io::{BufRead, Write};
use std::str::FromStr;

use anyhow::Result;
use brick_core::{
    capture_diff, discover_repo_root, explain_from_events, merge_source_steps_into,
    resolve_direct_anchor, resolve_file_anchor, resolve_file_line_anchor,
    resolve_file_range_anchor, source_sessions_to_steps, CausalChain, DiffCaptureRequest,
    LocalStore, MetadataDb, SourceFileSessionBlameQuery, SourceProfileStore, DEFAULT_EXPLAIN_DEPTH,
    MAX_EXPLAIN_DEPTH,
};
use brick_protocol::{
    ActorRef, ActorType, ArtifactCreatedPayload, ArtifactFileRefRecordedPayload, ArtifactId,
    ArtifactKind, CausalLinkedPayload, CausalRelation, ConfidenceLevel, DiffTarget, FileRefId,
    MissionCreatedPayload, MissionId, MissionStatus, MissionUpdatedPayload, ProjectId, SessionId,
    TraceEvent,
};
use serde_json::{json, Value};

use crate::history::build_live_broadcast;

/// MCP protocol revision this server speaks.
const PROTOCOL_VERSION: &str = "2024-11-05";

/// Runs the stdio JSON-RPC loop until stdin closes. Never returns an error to the
/// caller for per-request failures — those become JSON-RPC error responses; only
/// a fatal stdout write failure propagates.
///
/// `planning` selects the tool surface. The default (false) is the minimal
/// coding-agent surface — just `explain` (read WHY) and `link` (write a causal
/// edge). With `planning=true` the planning tools (`mission`, `mission_list`,
/// `show_mission`, `artifact_add`, `artifact_attach`) are added; this is the
/// surface meant for a dedicated planning custom agent, not the main agent.
pub fn serve(profiles: &SourceProfileStore, store: &LocalStore, planning: bool) -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();

    for line in stdin.lock().lines() {
        let line = match line {
            Ok(line) => line,
            Err(error) => {
                eprintln!("brick mcp-serve: stdin read error: {error}");
                break;
            }
        };
        if line.trim().is_empty() {
            continue;
        }
        let request: Value = match serde_json::from_str(&line) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("brick mcp-serve: invalid JSON: {error}");
                write_message(&mut out, &parse_error())?;
                continue;
            }
        };

        // Notifications have no `id` and expect no response.
        let id = request.get("id").cloned();
        let method = request.get("method").and_then(Value::as_str).unwrap_or("");

        if id.is_none() {
            // e.g. notifications/initialized — acknowledge by doing nothing.
            continue;
        }
        let id = id.unwrap();

        let response = match method {
            "initialize" => success(id, initialize_result(planning)),
            "tools/list" => success(id, tools_list_result(planning)),
            "tools/call" => match handle_tool_call(profiles, store, planning, &request) {
                Ok(result) => success(id, result),
                Err(error) => tool_error(id, &error.to_string()),
            },
            "ping" => success(id, json!({})),
            other => method_not_found(id, other),
        };
        write_message(&mut out, &response)?;
    }
    Ok(())
}

fn write_message(out: &mut impl Write, message: &Value) -> Result<()> {
    writeln!(out, "{}", serde_json::to_string(message)?)?;
    out.flush()?;
    Ok(())
}

fn initialize_result(planning: bool) -> Value {
    let instructions = if planning {
        "Brick planning surface (for a dedicated planning agent). Turn a request \
into a tracked goal with mission action='create'; as work moves, mission \
action='update' its status; list in-flight work with mission_list; inspect one \
with show_mission; log deliverables with artifact_add and back them with \
artifact_attach."
    } else {
        "Brick is the causal memory of this codebase — it answers WHY code looks \
the way it does, across every AI tool that touched it. \
When you locate a file or code you are about to change, call `explain` on it \
BEFORE drawing conclusions from the code alone: it returns who changed it, why \
(the rationale that can't be read from the code), what was derived from it, and \
who is editing it right now. `explain` is your FIRST step into existing code — \
prefer it over grep and `git log`, which are only a fallback when Brick has no \
record. \
After you make a non-trivial change, call `link` to record WHY in one line so \
the next agent (or you, in three months) can recover your reasoning."
    };
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "brick",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": instructions
    })
}

/// The `explain` tool schema — the single read entry point.
fn explain_tool() -> Value {
    json!({
        "name": "explain",
        "description": "Explain WHY a piece of code looks the way it does, across \
every AI tool that touched this repo. Your FIRST step when you locate existing \
code and before you draw conclusions from the code alone — prefer it over grep \
and `git log`, which are only a fallback when Brick has no record. Returns a \
causal chain: who changed the anchor, WHEN, WHY (the rationale note that cannot \
be read from the code), what was derived from / triggered by it, each step's \
confidence (explicit > observed > inferred), a transcript pointer per step, and \
a `live` field warning if another session is editing the same file right now. \
This subsumes line-level blame (WHO) into the WHY answer. Anchor can be a \
`path:line` (e.g. `/abs/workspace/src/auth.rs:42`), a `path:start-end` line \
range to explain a whole block at once (e.g. `/abs/workspace/src/auth.rs:10-20`), \
a whole-file `path`, an `artifact_*` id, a `mission_*` id, or an event id. Prefer \
an ABSOLUTE path — the server may run from a different working directory than \
your workspace, and an absolute anchor always resolves the right repo.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "anchor": {
                    "type": "string",
                    "description": "What to explain: a `path:line`, a `path:start-end` \
line range (e.g. `/abs/ws/src/auth.rs:10-20`, to get every change that touched \
that block), a whole-file `path` (use an ABSOLUTE path, e.g. \
`/abs/workspace/src/auth.rs:42`, so it resolves regardless of the server's \
working directory), an artifact id, a mission id, or an event id."
                },
                "depth": {
                    "type": "integer",
                    "description": "How many causal hops to walk back (default 3, max 8)."
                }
            },
            "required": ["anchor"]
        }
    })
}

/// The `link` tool schema — the single write entry point for causal edges.
fn link_tool() -> Value {
    json!({
        "name": "link",
        "description": "Record WHY you just made a change, so the next agent can \
recover your reasoning with `explain`. Call this RIGHT AFTER a non-trivial edit \
and BEFORE you commit — `link` binds your reason to a real change event, which it \
gets by capturing your still-uncommitted work, so committing first leaves nothing \
to bind to. Every `link` has an effect (the change) and a WHY (`note`), plus an \
optional `cause`. Common shapes: (1) a rationale — omit `effect` and give a \
`note`; Brick captures your uncommitted diff and binds the note to exactly those \
files; (2) a causal edge — also set `cause` to the anchor that prompted the \
change (an artifact, mission, or event id) and pick a `relation`; (3) \
implementing a planned work item — set `cause` to its `mission_…` id with \
relation='derived_from' so the planning record connects to the real code. If you \
made several unrelated edits, `link` after each one so each reason binds to the \
right files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "effect": {
                    "type": "string",
                    "description": "The change you are explaining. USUALLY OMIT \
THIS — when omitted, Brick captures your current uncommitted edits and binds the \
reason to exactly those files (so call `link` before committing). Only pass \
`effect` to point at a change Brick has ALREADY recorded: an event id, or a \
`path`/`path:line` (prefer an ABSOLUTE path) that resolves through blame to an \
existing change event. An `effect` that resolves to nothing is an error — it does \
NOT create a free-floating note; omit it and let Brick capture the diff instead."
                },
                "cause": {
                    "type": "string",
                    "description": "Optional anchor that caused/motivated this \
change: a `path`, `path:line` (prefer an ABSOLUTE path), artifact, mission, or \
event id. If you are implementing a planned work item, pass that `mission_…` id \
here (with relation='derived_from') so the planning record links to the actual \
code — do NOT just mention the mission in `note`, that leaves the graph \
disconnected. Omit for a standalone rationale."
                },
                "relation": {
                    "type": "string",
                    "description": "How the effect relates to the cause. Use 'rationale' \
for a standalone reason (no cause).",
                    "enum": ["triggered_by", "derived_from", "supersedes", "responds_to", "rationale"]
                },
                "note": {
                    "type": "string",
                    "description": "One line: WHY. Required when there is no cause."
                },
                "session": {
                    "type": "string",
                    "description": "Optional: the id of the coding session you are \
working in (your tool's session/conversation id). Brick records it on the \
change so a later `explain` can hand the next agent a transcript pointer back \
to this session — the original context behind the change. Omit if you don't \
have one."
                }
            },
            "required": []
        }
    })
}

fn tools_list_result(planning: bool) -> Value {
    if planning {
        return json!({ "tools": planning_tools() });
    }
    json!({ "tools": [ explain_tool(), link_tool() ] })
}

/// Planning tools, exposed only on the planning surface (a dedicated planning
/// agent), never on the main coding-agent surface.
fn planning_tools() -> Vec<Value> {
    vec![
        json!({
            "name": "mission_list",
            "description": "List missions (work items / goals) Brick is tracking, \
newest first. Use to see what's in flight before starting work or to pick up an \
unfinished task. Optionally filter by status or project.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Filter by status: planned | active | blocked | completed | archived."
                    },
                    "project": { "type": "string", "description": "Filter to one project id." },
                    "limit": { "type": "integer", "description": "Max missions (default 50)." }
                }
            }
        }),
        json!({
            "name": "show_mission",
            "description": "Show one mission in detail: status, description, and the \
sessions and artifacts linked to it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "mission": { "type": "string", "description": "The mission id to show." }
                },
                "required": ["mission"]
            }
        }),
        json!({
            "name": "mission",
            "description": "Create or update a mission (work item / goal). \
action='create' opens a new work item under a project; action='update' changes \
its title/description/status as work progresses.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "'create' a new mission or 'update' an existing one.",
                        "enum": ["create", "update"]
                    },
                    "mission": { "type": "string", "description": "Mission id — REQUIRED for update." },
                    "project": {
                        "type": "string",
                        "description": "Project id — REQUIRED for create."
                    },
                    "title": { "type": "string", "description": "Short imperative goal title. Required for create." },
                    "description": { "type": "string", "description": "Optional longer description." },
                    "status": {
                        "type": "string",
                        "description": "planned | active | blocked | completed | archived.",
                        "enum": ["planned", "active", "blocked", "completed", "archived"]
                    },
                    "session_id": { "type": "string", "description": "Your session id (optional)." },
                    "source": { "type": "string", "description": "Your tool/app id (optional)." }
                },
                "required": ["action"]
            }
        }),
        json!({
            "name": "artifact_add",
            "description": "Record a deliverable (a PR, design doc, decision, test \
result) and link it to a mission. Call after finishing a meaningful piece of work.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "What the artifact is." },
                    "kind": {
                        "type": "string",
                        "description": "decision | file_ref | patch | review | test_result | acceptance | note.",
                        "enum": ["decision", "file_ref", "patch", "review", "test_result", "acceptance", "note"]
                    },
                    "body": { "type": "string", "description": "Optional details / link / summary." },
                    "mission": { "type": "string", "description": "Mission id to link this artifact to." },
                    "session_id": { "type": "string", "description": "Your session id (optional)." },
                    "source": { "type": "string", "description": "Your tool/app id (optional)." }
                },
                "required": ["title"]
            }
        }),
        json!({
            "name": "artifact_attach",
            "description": "Attach a file-path piece of evidence to an artifact — the \
concrete file(s) backing a deliverable. Call after artifact_add.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "artifact": { "type": "string", "description": "Artifact id (from artifact_add)." },
                    "path": { "type": "string", "description": "File path the artifact touched." },
                    "session_id": { "type": "string", "description": "Your session id (optional)." },
                    "source": { "type": "string", "description": "Your tool/app id (optional)." }
                },
                "required": ["artifact", "path"]
            }
        }),
    ]
}

/// Tools retired from the MCP surface and folded elsewhere. A `tools/call` for
/// one of these returns an actionable error pointing at the replacement, rather
/// than a bare "unknown tool", so agents with the old names baked into memory
/// files / MCP configs get a clear migration message for one transition cycle.
fn retired_tool_hint(name: &str) -> Option<&'static str> {
    let hint = match name {
        // Memory/query tools — folded into `explain` (which returns WHO + WHY).
        "log_file" | "recall_file" | "blame" | "blame_file" | "log_line" | "blame_history"
        | "search" | "explore_memory" | "search_sessions" | "show_session" | "read_session" => {
            "retired: use `explain` with a `path:line`, artifact, mission, or event \
anchor — it returns who changed it, why, and a transcript pointer."
        }
        // Coordination tools — folded into `explain`'s `live` field.
        "sessions" | "live_sessions" | "claim" | "announce_work" | "claims"
        | "list_announcements" | "status" | "current_context" => {
            "retired: live coordination is now the `live` field of an `explain` \
response; there is no separate coordination tool."
        }
        // Planning tools — moved to the planning surface (`brick mcp-serve
        // --planning`), used by a dedicated planning agent, not the main agent.
        "mission" | "manage_mission" | "mission_list" | "list_missions" | "show_mission"
        | "artifact_add" | "record_artifact" | "artifact_attach" | "attach_evidence" => {
            "moved: planning tools live on the planning surface (a dedicated \
planning agent via `brick mcp-serve --planning`), not the main coding surface."
        }
        _ => return None,
    };
    Some(hint)
}

/// Maps a retired planning tool name onto its current name for the planning
/// surface. Unknown names pass through unchanged.
fn canonical_planning_name(name: &str) -> &str {
    match name {
        "list_missions" => "mission_list",
        "manage_mission" => "mission",
        "record_artifact" => "artifact_add",
        "attach_evidence" => "artifact_attach",
        other => other,
    }
}

fn handle_tool_call(
    profiles: &SourceProfileStore,
    store: &LocalStore,
    planning: bool,
    request: &Value,
) -> Result<Value> {
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let raw_name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let payload = if planning {
        dispatch_planning(store, canonical_planning_name(raw_name), &args)?
    } else {
        match raw_name {
            "explain" => explain_tool_call(profiles, store, &args)?,
            "link" => link_tool_call(store, &args)?,
            other => {
                // A retired name gets an actionable migration hint; a truly
                // unknown name gets the generic error.
                if let Some(hint) = retired_tool_hint(other) {
                    json!({ "error": "tool_retired", "tool": other, "hint": hint })
                } else {
                    return Err(anyhow::anyhow!("unknown tool: {other}"));
                }
            }
        }
    };

    // MCP tool results wrap content blocks; we hand back the JSON as text so the
    // agent gets the full structured payload it can parse.
    Ok(json!({
        "content": [
            { "type": "text", "text": serde_json::to_string_pretty(&payload)? }
        ]
    }))
}

/// One db, one explain: merge the metadata db's indexed `source_sessions` (what
/// codex/claude/… touched) WITH whatever JSONL causal steps the chain already
/// has, into one deduped, time-ordered timeline. A file's history is commonly
/// interleaved — some changes `link`ed, some only seen by an external tool — so
/// this is a true merge, not a fill-if-empty fallback (which dropped every
/// un-`link`ed change adjacent to a `link`ed one). Dedup is by `session_id`;
/// see `merge_source_steps_into`.
///
/// Shared by the MCP `explain` tool and the CLI `explain` command so both behave
/// identically. Returns `Some(n)` for a file:line anchor: `n` indexed sessions
/// touched the file, so the caller surfaces a hint to retry with a whole-file
/// anchor (file:line never pulls file-level index data into the chain, which
/// would fake line precision).
pub(crate) fn merge_index_sessions_into_chain(
    chain: &mut CausalChain,
    repo_root: &std::path::Path,
    anchored_path: Option<&str>,
    is_file_line: bool,
    depth: usize,
) -> Option<usize> {
    let rel_path = anchored_path?;
    let repo_root = repo_root.to_path_buf();
    let abs_path = if std::path::Path::new(rel_path).is_absolute() {
        rel_path.to_string()
    } else {
        repo_root.join(rel_path).display().to_string()
    };
    let db = MetadataDb::open_global().ok()?;
    let query = SourceFileSessionBlameQuery {
        file_path: abs_path,
        source_id: None,
        repo_path: Some(repo_root.clone()),
        limit: depth.clamp(DEFAULT_EXPLAIN_DEPTH, MAX_EXPLAIN_DEPTH),
    };
    let rows = db.query_source_file_session_blame(&query).ok()?;
    // Strict same-repo filter: source_sessions is a global table, so never let
    // another repo's session bleed in. Canonicalize both sides so a symlinked root
    // (macOS `/var`→`/private/var`) still matches while a genuinely different repo
    // is excluded.
    let want = std::fs::canonicalize(&repo_root).unwrap_or_else(|_| repo_root.clone());
    let same_repo: Vec<_> = rows
        .into_iter()
        .filter(|row| {
            row.source_pointer
                .as_ref()
                .and_then(|p| p.get("repo_path"))
                .and_then(|v| v.as_str())
                .map(|rp| {
                    let have =
                        std::fs::canonicalize(rp).unwrap_or_else(|_| std::path::PathBuf::from(rp));
                    have == want
                })
                .unwrap_or(false)
        })
        .collect();
    // file:line never merges: source_sessions are file-level, so folding them into
    // a line anchor would fake line precision. Report the count so the caller can
    // hint "re-run with a whole-file anchor", but leave the chain untouched.
    if is_file_line {
        return (!same_repo.is_empty()).then_some(same_repo.len());
    }
    // Whole-file anchor: merge the indexed source sessions WITH whatever JSONL
    // steps the chain already has, into one deduped, time-ordered timeline. This
    // is the fix for interleaved history (link, no-link, link, …) where the old
    // fill-if-empty fallback silently dropped the un-linked changes.
    let source_steps = source_sessions_to_steps(&same_repo, 0);
    if !source_steps.is_empty() {
        merge_source_steps_into(&mut chain.steps, source_steps);
        chain.anchor.resolved_events = chain.steps.iter().map(|s| s.event_id.clone()).collect();
    }
    None
}

/// `explain` dispatch: resolve the anchor (file:line via blame, or a direct id),
/// walk the causal graph, then enrich with transcript pointers and the `live`
/// coordination field.
fn explain_tool_call(
    profiles: &SourceProfileStore,
    store: &LocalStore,
    args: &Value,
) -> Result<Value> {
    let anchor_input = str_arg(args, "anchor")?;
    let depth = usize_arg(args, "depth").unwrap_or(DEFAULT_EXPLAIN_DEPTH);

    // Resolve the store from the anchor so a server spawned with an unrelated
    // cwd (the universal `cwd=/` MCP-client behavior) still reads the right
    // repo. No repo for this anchor → a clean no-record response, never a crash.
    let Some(store) = store_for_anchor(store, &anchor_input) else {
        return Ok(json!({
            "anchor": { "input": anchor_input, "kind": "unknown", "resolved_events": [] },
            "causal_chain": [],
            "forward": [],
            "truncated": false,
            "note": "No Brick repo resolved for this anchor (the MCP server was \
likely started outside a git repo, e.g. cwd=/). Pass an absolute path anchor, \
or set the server's working directory to the workspace. Falling back to git/grep \
is fine here."
        }));
    };
    let store = &store;

    // Zero-config freshness: refresh this repo's source index before reading, so
    // a change the agent just made is visible without the user ever running a
    // CLI refresh. Best-effort + throttled — never blocks or fails the read.
    crate::history::refresh_repo_sources_best_effort(store.repo_root());

    let events = store.read_all_events()?;
    let index = store.load_or_rebuild_index()?;

    // file:line and file:start-end anchors need git + the working tree; direct
    // ids do not.
    let (anchor, anchored_path, is_file_line) = if let Some((path, start, end)) =
        parse_file_range(&anchor_input)
    {
        let repo_root = store.repo_root().to_path_buf();
        let rel_path = normalize_repo_relative(&repo_root, &path);
        let anchor = resolve_file_range_anchor(store, &repo_root, &rel_path, start, end)?;
        (anchor, Some(rel_path), true)
    } else if let Some((path, line)) = parse_file_line(&anchor_input) {
        let repo_root = store.repo_root().to_path_buf();
        let rel_path = normalize_repo_relative(&repo_root, &path);
        let anchor = resolve_file_line_anchor(store, &repo_root, &rel_path, line)?;
        (anchor, Some(rel_path), true)
    } else if looks_like_path(&anchor_input) {
        // A whole-file anchor (no `:line`) — agents very often ask about a file,
        // not a line. Match the file's change events directly instead of treating
        // the path as an opaque id (which wrongly reported "no record").
        let rel_path = normalize_repo_relative(store.repo_root(), &anchor_input);
        (resolve_file_anchor(&events, &rel_path), Some(rel_path), false)
    } else {
        (resolve_direct_anchor(&events, &anchor_input), None, false)
    };

    let mut chain = explain_from_events(&index, &events, anchor, depth.min(MAX_EXPLAIN_DEPTH));
    // One db, one explain: when a WHOLE-FILE anchor has no recorded trace events,
    // the metadata db's indexed `source_sessions` ARE the chain. Shared with the
    // CLI `explain` command so both entry points behave identically.
    let index_session_hint = merge_index_sessions_into_chain(
        &mut chain,
        store.repo_root(),
        anchored_path.as_deref(),
        is_file_line,
        depth,
    );
    let value = finalize_explain_chain(
        chain,
        store,
        Some(profiles),
        anchored_path.as_deref(),
        index_session_hint,
    )?;
    Ok(value)
}

/// Finalizes a resolved causal chain into the JSON `explain` response, applying
/// the enrichments that make CLI and MCP answer identically: transcript pointers,
/// observed-rationale recovery, the `live` cross-session field, and the
/// no-record note. Shared by `explain_tool_call` (MCP) and `handle_explain`
/// (CLI) so the same db yields the same answer from either entry point.
///
/// `cwd_profiles` is the profile store built from the server's process cwd, used
/// only as a fallback for `live` when the anchor repo has no profiles of its own;
/// the CLI passes `None` (cwd already is the repo).
pub(crate) fn finalize_explain_chain(
    mut chain: CausalChain,
    store: &LocalStore,
    cwd_profiles: Option<&SourceProfileStore>,
    anchored_path: Option<&str>,
    index_session_hint: Option<usize>,
) -> Result<Value> {
    // Resolve transcript pointers from the SAME repo the anchor resolved to, not
    // the server's process cwd (which is `/` for every MCP client) — mirrors the
    // `live` profile resolution below.
    let anchor_profiles = SourceProfileStore::new(store.repo_root().to_path_buf());
    enrich_transcripts(&anchor_profiles, cwd_profiles, &mut chain);
    // For steps that have a resolved transcript but no asserted (`explicit`) note,
    // recover the turn's final assistant message as an `observed` rationale, so
    // ingested history isn't left with WHO/WHEN but zero WHY. Never overrides an
    // explicit `link` note.
    enrich_observed_rationale(&mut chain);

    let mut value = serde_json::to_value(&chain)?;
    // `live` field: if another running session is touching the anchored file
    // right now, surface it so the agent avoids a cross-session edit conflict.
    // This is what replaced the standalone `sessions`/`claims` coordination tools.
    if let Some(path) = anchored_path {
        // Source profiles live under `<repo>/.brick/sources`. Rebuild the profile
        // store from the SAME repo the anchor resolved to, and only fall back to
        // the cwd-derived profiles when that store has no profiles of its own.
        // This makes `live` work for the default agent path (absolute anchor +
        // cwd=/), exactly like explain's store resolution.
        let live_profiles = match anchor_profiles.list_profiles() {
            Ok(found) if !found.is_empty() => found,
            _ => cwd_profiles
                .and_then(|p| p.list_profiles().ok())
                .unwrap_or_default(),
        };
        if !live_profiles.is_empty() {
            if let Some(broadcast) = build_live_broadcast(&live_profiles, path, None) {
                if let Value::Object(map) = &mut value {
                    map.insert("live".to_string(), serde_json::to_value(broadcast)?);
                }
            }
        }
    }

    if chain_is_empty(&chain) {
        if let Value::Object(map) = &mut value {
            let note = match index_session_hint {
                Some(count) => format!(
                    "No line-level record for this anchor. But {count} indexed session(s) \
touched this file — re-run `explain` with a whole-file anchor (drop the `:line`) \
to see who changed it and why."
                ),
                None => "No Brick record for this anchor yet. Brick only records causal \
edges for changes made while it was installed; fall back to git/grep here. As \
more changes flow through Brick, explain gets richer."
                    .to_string(),
            };
            map.insert("note".to_string(), json!(note));
        }
    }

    Ok(value)
}

/// `link` dispatch: write a `causal.linked` event. Supports a standalone
/// rationale (note only) or a cross-event edge (cause anchor + relation).
fn link_tool_call(store: &LocalStore, args: &Value) -> Result<Value> {
    // Resolve the store from the effect anchor (or cause) so a server spawned
    // with `cwd=/` still writes into the agent's actual repo, not `/.brick`.
    let anchor_hint = opt_str_arg(args, "effect")
        .or_else(|| opt_str_arg(args, "cause"))
        .unwrap_or_default();
    let resolved_store = store_for_anchor(store, &anchor_hint);
    let store = resolved_store.as_ref().unwrap_or(store);

    // Keep this repo's source index fresh on write too, so a follow-up `explain`
    // (or cause-anchor resolution here) sees the agent's latest sessions.
    // Best-effort + throttled — never blocks or fails the write.
    crate::history::refresh_repo_sources_best_effort(store.repo_root());

    let events = store.read_all_events()?;

    // Track whether we synthesized a diff so the response can tell the agent
    // which files the rationale was bound to (otherwise it silently binds to
    // whatever it could resolve, which used to be an unrelated stale diff).
    let mut captured_files: Vec<String> = Vec::new();

    // A `link` edge always binds to ONE real change event — either an `effect`
    // anchor that resolves to an existing event, or a fresh `diff.captured` taken
    // from the agent's uncommitted work. There is no path/repo pseudo-anchor: a
    // rationale with nothing to bind to is a hard error with actionable guidance,
    // not a silently-mis-bound note. The fix for "nothing to capture" lives in
    // the workflow (call `link` before committing), not in a runtime fallback.
    let effect_event = match opt_str_arg(args, "effect") {
        // An explicit anchor must resolve to a real event. A `path`/`path:line`
        // resolves through blame to the change event that last touched it.
        Some(anchor) => resolve_anchor_to_event(store, &events, &anchor)?.ok_or_else(|| {
            anyhow::anyhow!(
                "effect anchor '{anchor}' does not resolve to a Brick change event. \
`link` records WHY against a change Brick already saw. Either omit `effect` and \
call `link` BEFORE you commit (so the uncommitted diff is captured), or pass an \
`effect` that names an existing event/commit Brick has indexed."
            )
        })?,
        // No explicit effect: capture the agent's current uncommitted work and
        // bind the rationale to exactly those files. A clean tree means there is
        // nothing to attach a reason to — a hard error, never a stale mis-bind.
        None => match capture_working_diff_event(store, args, &mut captured_files)? {
            Some(event_id) => event_id,
            None => {
                return Err(anyhow::anyhow!(
                    "no `effect` given and no uncommitted changes to capture. `link` binds a \
reason to a real change, so call it BEFORE committing (while your edits are still \
in the working tree), or pass an `effect` that names an existing event/commit."
                ));
            }
        },
    };

    let cause_anchor = opt_str_arg(args, "cause");
    let cause_events = match &cause_anchor {
        Some(anchor) => resolve_anchor_to_event(store, &events, anchor)?
            .into_iter()
            .collect(),
        None => Vec::new(),
    };

    let note = opt_str_arg(args, "note");
    let relation = parse_relation(args.get("relation").and_then(Value::as_str), &cause_events)?;

    // Invariant mirror: a standalone edge needs a note.
    if cause_events.is_empty() && note.is_none() {
        return Err(anyhow::anyhow!(
            "link needs either a cause anchor or a note explaining the change"
        ));
    }

    // Confidence is `explicit` — the agent asserted this edge directly.
    let event = TraceEvent::causal_linked(
        mcp_actor(args),
        ConfidenceLevel::Explicit,
        CausalLinkedPayload {
            effect_event,
            cause_events: cause_events.clone(),
            relation,
            note: note.clone(),
            repo_context_id: None,
        },
    )
    .map_err(|err| anyhow::anyhow!("invalid causal edge: {err}"))?;
    store.append_event(&event)?;

    Ok(json!({
        "linked": true,
        "effect_event": effect_event.to_string(),
        "cause_events": cause_events.iter().map(ToString::to_string).collect::<Vec<_>>(),
        "relation": relation_wire(relation),
        "note": note,
        "captured_files": captured_files,
        "note_hint": "Recorded. The next agent can recover this with `explain`."
    }))
}

/// When `link` is called with no `effect`, the agent has just edited code with
/// its own tools (which leave no Brick event). Capture everything the agent
/// changed since the last commit — BOTH unstaged (`Working`) and staged
/// (`Staged`) changes — so the rationale binds to the files actually touched,
/// and return the new `diff.captured` event id. Returns `None` when there are no
/// changes at all. Merging both targets matters: an agent (or its harness) that
/// `git add`s its work before calling `link` would otherwise capture nothing and
/// the reason would mis-bind to a stale prior diff.
fn capture_working_diff_event(
    store: &LocalStore,
    args: &Value,
    captured_files: &mut Vec<String>,
) -> Result<Option<uuid::Uuid>> {
    let cwd = std::env::current_dir()?;
    let Ok(repo_root) = discover_repo_root(&cwd) else {
        return Ok(None);
    };
    let mut payload = capture_diff(
        &repo_root,
        DiffCaptureRequest {
            target: DiffTarget::Working,
            base_commit: None,
            head_commit: None,
            repo_context_id: None,
        },
    )?;
    // Fold in staged changes the working-tree diff doesn't see, deduping by path
    // so a file that is both staged and further edited isn't listed twice.
    let staged = capture_diff(
        &repo_root,
        DiffCaptureRequest {
            target: DiffTarget::Staged,
            base_commit: None,
            head_commit: None,
            repo_context_id: None,
        },
    )?;
    for change in staged.file_changes {
        if !payload
            .file_changes
            .iter()
            .any(|existing| existing.path == change.path)
        {
            payload.file_changes.push(change);
        }
    }
    if payload.file_changes.is_empty() {
        return Ok(None);
    }
    captured_files.extend(payload.file_changes.iter().map(|change| change.path.clone()));
    let event = TraceEvent::diff_captured(
        mcp_actor(args),
        ArtifactId::new(),
        link_session_id(args),
        None,
        payload,
    )?;
    let event_id = event.event_id;
    store.append_event(&event)?;
    Ok(Some(event_id))
}

/// Planning-surface dispatch (mission / artifact tools).
fn dispatch_planning(store: &LocalStore, name: &str, args: &Value) -> Result<Value> {
    // Planning records (missions / artifacts) have no path anchor, so they can't
    // resolve a repo from their arguments the way explain/link do. When the
    // server was spawned outside a git repo (the universal `cwd=/` MCP-client
    // case) the cwd-derived store points at an unwritable root and every
    // append_event/init crashes with "failed to create provenance queue
    // directory". Fall back to a BRICK_HOME-rooted store, which is always
    // writable and is the natural home for cross-repo planning state anyway.
    let fallback = planning_store_fallback(store);
    let store = fallback.as_ref().unwrap_or(store);

    let payload = match name {
        "mission_list" => {
            let index = store.load_or_rebuild_index()?;
            let status_filter = args
                .get("status")
                .and_then(Value::as_str)
                .map(|raw| raw.trim().to_lowercase());
            let project_filter = args
                .get("project")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty());
            let limit = usize_arg(args, "limit").unwrap_or(50);
            let mut missions: Vec<_> = index
                .missions
                .values()
                .filter(|mission| match &status_filter {
                    Some(status) => mission_status_str(mission.status) == status,
                    None => true,
                })
                .filter(|mission| match project_filter {
                    Some(project) => mission.project_id.as_deref() == Some(project),
                    None => true,
                })
                .collect();
            missions.sort_by_key(|mission| std::cmp::Reverse(mission.last_event_at));
            missions.truncate(limit);
            json!({ "count": missions.len(), "missions": missions })
        }
        "show_mission" => {
            let mission = str_arg(args, "mission")?;
            let index = store.load_or_rebuild_index()?;
            let item = index
                .missions
                .get(&mission)
                .ok_or_else(|| anyhow::anyhow!("mission not found: {mission}"))?;
            serde_json::to_value(item)?
        }
        "mission" => dispatch_mission(store, args)?,
        "artifact_add" => {
            let title = str_arg(args, "title")?;
            let actor = mcp_actor(args);
            let mission_id = match args.get("mission").and_then(Value::as_str) {
                Some(mission) if !mission.is_empty() => Some(
                    MissionId::from_str(mission)
                        .map_err(|err| anyhow::anyhow!("invalid mission id: {err}"))?,
                ),
                _ => None,
            };
            let session_id = match args.get("session_id").and_then(Value::as_str) {
                Some(session) if !session.is_empty() => Some(
                    SessionId::from_str(session)
                        .map_err(|err| anyhow::anyhow!("invalid session id: {err}"))?,
                ),
                _ => None,
            };
            let artifact_id = ArtifactId::new();
            let event = TraceEvent::artifact_created(
                actor,
                artifact_id.clone(),
                mission_id,
                session_id,
                ArtifactCreatedPayload {
                    artifact_kind: artifact_kind_from_str(args.get("kind").and_then(Value::as_str)),
                    title,
                    body: opt_str_arg(args, "body"),
                    repo_context_id: None,
                },
            )?;
            store.append_event(&event)?;
            json!({
                "recorded": true,
                "artifact_id": artifact_id.to_string(),
                "note": "Deliverable logged. Attach the backing files with artifact_attach."
            })
        }
        "artifact_attach" => {
            let artifact = str_arg(args, "artifact")?;
            let path = str_arg(args, "path")?;
            let actor = mcp_actor(args);
            let artifact_id = ArtifactId::from_str(&artifact)
                .map_err(|err| anyhow::anyhow!("invalid artifact id: {err}"))?;
            let session_id = match args.get("session_id").and_then(Value::as_str) {
                Some(session) if !session.is_empty() => Some(
                    SessionId::from_str(session)
                        .map_err(|err| anyhow::anyhow!("invalid session id: {err}"))?,
                ),
                _ => None,
            };
            let event = TraceEvent::artifact_file_ref_recorded(
                actor,
                artifact_id.clone(),
                session_id,
                ArtifactFileRefRecordedPayload {
                    file_ref_id: FileRefId::new(),
                    path,
                    repo_context_id: None,
                },
            )?;
            store.append_event(&event)?;
            json!({ "attached": true, "artifact_id": artifact_id.to_string() })
        }
        other => return Err(anyhow::anyhow!("unknown planning tool: {other}")),
    };
    Ok(payload)
}

fn dispatch_mission(store: &LocalStore, args: &Value) -> Result<Value> {
    let action = str_arg(args, "action")?;
    let actor = mcp_actor(args);
    match action.as_str() {
        "create" => {
            let project = str_arg(args, "project")?;
            let title = str_arg(args, "title")?;
            let project_id = ProjectId::from_str(&project)
                .map_err(|err| anyhow::anyhow!("invalid project id: {err}"))?;
            let mission_id = MissionId::new();
            let event = TraceEvent::mission_created(
                actor,
                mission_id.clone(),
                MissionCreatedPayload {
                    project_id,
                    title,
                    description: opt_str_arg(args, "description"),
                    status: mission_status_from_str(args.get("status").and_then(Value::as_str))?,
                    repo_context_id: None,
                },
            )?;
            store.append_event(&event)?;
            Ok(json!({
                "created": true,
                "mission_id": mission_id.to_string(),
                "note": "Mission opened. Record deliverables with artifact_add, and \
update its status with mission action='update'."
            }))
        }
        "update" => {
            let mission = str_arg(args, "mission")?;
            let mission_id = MissionId::from_str(&mission)
                .map_err(|err| anyhow::anyhow!("invalid mission id: {err}"))?;
            let project_id = match args.get("project").and_then(Value::as_str) {
                Some(project) if !project.is_empty() => Some(
                    ProjectId::from_str(project)
                        .map_err(|err| anyhow::anyhow!("invalid project id: {err}"))?,
                ),
                _ => None,
            };
            let title = opt_str_arg(args, "title");
            let description = opt_str_arg(args, "description");
            let status = match args.get("status").and_then(Value::as_str) {
                Some(raw) if !raw.is_empty() => Some(parse_mission_status(raw)?),
                _ => None,
            };
            if project_id.is_none() && title.is_none() && description.is_none() && status.is_none() {
                return Err(anyhow::anyhow!(
                    "mission update needs at least one of project, title, description, or status"
                ));
            }
            let event = TraceEvent::mission_updated(
                actor,
                mission_id.clone(),
                MissionUpdatedPayload {
                    project_id,
                    title,
                    description,
                    status,
                    repo_context_id: None,
                },
            )?;
            store.append_event(&event)?;
            Ok(json!({ "updated": true, "mission_id": mission_id.to_string() }))
        }
        other => Err(anyhow::anyhow!(
            "unknown mission action: {other} (expected 'create' or 'update')"
        )),
    }
}

/// Returns a required non-empty string argument, or an error.
fn str_arg(args: &Value, key: &str) -> Result<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("missing required string argument: {key}"))
}

fn usize_arg(args: &Value, key: &str) -> Option<usize> {
    args.get(key)
        .and_then(Value::as_u64)
        .map(|value| value as usize)
}

/// Parses a `path:line` anchor into `(path, line)`. Returns `None` when the input
/// is not of that shape (e.g. it's a bare id) — note a Windows-style `C:\...` has
/// no trailing integer, so it won't false-match.
fn parse_file_line(input: &str) -> Option<(String, u64)> {
    let (path, line) = input.rsplit_once(':')?;
    let line: u64 = line.trim().parse().ok()?;
    if path.is_empty() {
        return None;
    }
    Some((path.to_string(), line))
}

/// Parses a `path:start-end` line-RANGE anchor (e.g. `src/auth.rs:10-20`).
/// Returns `None` for a bare path, a single `path:line` (handled by
/// [`parse_file_line`]), or a malformed range. Tolerates `end < start`.
fn parse_file_range(input: &str) -> Option<(String, u64, u64)> {
    let (path, span) = input.rsplit_once(':')?;
    if path.is_empty() {
        return None;
    }
    let (start, end) = span.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    Some((path.to_string(), start, end))
}

/// Heuristic: does the anchor look like a file path rather than an id? Brick ids
/// are `artifact_*` / `mission_*` / `session_*` prefixes or bare UUIDs; anything
/// with a path separator or a file extension is a whole-file anchor.
fn looks_like_path(input: &str) -> bool {
    let s = input.trim();
    if s.is_empty() {
        return false;
    }
    if s.starts_with("artifact_")
        || s.starts_with("mission_")
        || s.starts_with("session_")
        || s.starts_with("org_")
        || s.starts_with("project_")
    {
        return false;
    }
    if uuid::Uuid::parse_str(s).is_ok() {
        return false;
    }
    s.contains('/') || s.contains('.')
}

/// Resolves the `LocalStore` to use for an anchor, making the MCP server robust
/// to being spawned with an unrelated process cwd.
///
/// MCP clients (Claude Code, Codex, ORGII, …) routinely spawn a stdio server
/// with `cwd=/` — the agent's workspace is NOT the server's cwd. The default
/// `store` is built from process cwd in `main.rs`, so with `cwd=/` it points at
/// `/.brick` (unwritable) and every read explodes on `init()`.
///
/// When the anchor is an **absolute** path, its own repo root is authoritative —
/// derive the store from there instead of process cwd. Returns `None` when no
/// git repo can be found for the anchor (the honest "Brick has no repo here"
/// case, which the caller renders as a clean no-record result rather than a
/// hard error). For non-path anchors (ids) the default store is used as-is.
fn store_for_anchor(default: &LocalStore, anchor: &str) -> Option<LocalStore> {
    let path_part = parse_file_line(anchor)
        .map(|(p, _)| p)
        .unwrap_or_else(|| anchor.to_string());
    let candidate = std::path::Path::new(&path_part);
    if candidate.is_absolute() {
        // Walk up from the anchor's directory to find its repo root.
        let start = if candidate.is_dir() {
            candidate.to_path_buf()
        } else {
            candidate.parent().unwrap_or(candidate).to_path_buf()
        };
        return discover_repo_root(&start)
            .ok()
            .map(|repo_root| LocalStore::new(repo_root));
    }
    // Relative anchor: fall back to the default (cwd-derived) store only when its
    // repo root is a real git repo — otherwise there is nothing to read.
    if discover_repo_root(default.repo_root()).is_ok() {
        Some(LocalStore::new(default.repo_root().to_path_buf()))
    } else {
        None
    }
}

/// Picks a writable store for the anchorless planning surface (missions /
/// artifacts) when the cwd-derived `default` store is rooted outside a git repo
/// (the `cwd=/` MCP-client case). Returns a `BRICK_HOME`-rooted store — always
/// writable, and the natural home for cross-repo planning state. Returns `None`
/// when the default store is already a real repo (use it as-is) or when no Brick
/// home can be resolved (let the original path surface its own error).
fn planning_store_fallback(default: &LocalStore) -> Option<LocalStore> {
    if discover_repo_root(default.repo_root()).is_ok() {
        return None;
    }
    brick_core::resolve_brick_home()
        .ok()
        .map(LocalStore::new)
}

/// Resolves any anchor (file:line, artifact/mission/event id) to a single event
/// id for `link`. file:line uses blame; ids reuse the direct resolver.
fn resolve_anchor_to_event(
    store: &LocalStore,
    events: &[brick_protocol::TraceEvent],
    anchor: &str,
) -> Result<Option<uuid::Uuid>> {
    if let Some((path, line)) = parse_file_line(anchor) {
        let cwd = std::env::current_dir()?;
        let repo_root = discover_repo_root(&cwd)?;
        let rel_path = normalize_repo_relative(&repo_root, &path);
        let resolved = resolve_file_line_anchor(store, &repo_root, &rel_path, line)?;
        return Ok(resolved
            .resolved_events
            .first()
            .and_then(|id| uuid::Uuid::parse_str(id).ok()));
    }
    if looks_like_path(anchor) {
        let rel_path = std::env::current_dir()
            .ok()
            .and_then(|cwd| discover_repo_root(&cwd).ok())
            .map(|repo_root| normalize_repo_relative(&repo_root, anchor))
            .unwrap_or_else(|| anchor.to_string());
        let resolved = resolve_file_anchor(events, &rel_path);
        return Ok(resolved
            .resolved_events
            .first()
            .and_then(|id| uuid::Uuid::parse_str(id).ok()));
    }
    let resolved = resolve_direct_anchor(events, anchor);
    Ok(resolved
        .resolved_events
        .first()
        .and_then(|id| uuid::Uuid::parse_str(id).ok()))
}

/// Parses the `relation` arg, defaulting to `derived_from` when a cause is given
/// or `rationale` when it is not.
fn parse_relation(raw: Option<&str>, cause_events: &[uuid::Uuid]) -> Result<CausalRelation> {
    match raw.map(|value| value.trim().to_lowercase()).as_deref() {
        Some("triggered_by") => Ok(CausalRelation::TriggeredBy),
        Some("derived_from") => Ok(CausalRelation::DerivedFrom),
        Some("supersedes") => Ok(CausalRelation::Supersedes),
        Some("responds_to") => Ok(CausalRelation::RespondsTo),
        Some("rationale") => Ok(CausalRelation::Rationale),
        Some(other) => Err(anyhow::anyhow!(
            "unknown relation: {other} (triggered_by|derived_from|supersedes|responds_to|rationale)"
        )),
        None => {
            if cause_events.is_empty() {
                Ok(CausalRelation::Rationale)
            } else {
                Ok(CausalRelation::DerivedFrom)
            }
        }
    }
}

fn relation_wire(relation: CausalRelation) -> &'static str {
    match relation {
        CausalRelation::TriggeredBy => "triggered_by",
        CausalRelation::DerivedFrom => "derived_from",
        CausalRelation::Supersedes => "supersedes",
        CausalRelation::RespondsTo => "responds_to",
        CausalRelation::Rationale => "rationale",
    }
}

/// Fills each step's transcript pointer with the concrete on-disk location: a
/// file path for file-backed sources (Claude/Codex/Gemini), or a sqlite ref for
/// db-backed ones (Cursor/ORGII). The core only knows the session id; the CLI
/// layer has the profiles to resolve it.
///
/// Resolution: enumerate every configured source's sessions once, building an
/// `external_session_id → (source_app_id, on-disk path)` map, then stamp each
/// step (and forward effect) whose `session_id` matches. `anchor_profiles` is
/// the repo the anchor resolved to (the right place to look); `cwd_profiles` is
/// the fallback for when the server happens to run inside a repo. A session id
/// that resolves to no known source is left as a bare id pointer — still useful,
/// just not openable.
fn enrich_transcripts(
    anchor_profiles: &SourceProfileStore,
    cwd_profiles: Option<&SourceProfileStore>,
    chain: &mut CausalChain,
) {
    // Cheap exit: if no step or forward effect carries a session id, there is
    // nothing to resolve and we skip the (potentially slow) source scan.
    let needs_resolution = chain
        .steps
        .iter()
        .any(|step| step.transcript.as_ref().and_then(|t| t.session_id.as_ref()).is_some())
        || chain.forward.iter().any(|f| f.session_id.is_some());
    if !needs_resolution {
        return;
    }

    let index = build_transcript_index(anchor_profiles, cwd_profiles);
    if index.is_empty() {
        return;
    }

    for step in &mut chain.steps {
        if let Some(pointer) = step.transcript.as_mut() {
            if let Some(session_id) = pointer.session_id.clone() {
                if let Some((source, session_ref)) = index.get(&session_id) {
                    pointer.source = Some(source.clone());
                    pointer.session_ref = Some(session_ref.clone());
                }
            }
        }
    }
}

/// Builds `external_session_id → (source_app_id, on-disk ref)` from the configured
/// sources. Prefers the anchor's repo; only falls back to the cwd-derived store
/// when the anchor repo has no profiles. The on-disk ref is the transcript file
/// path (file sources) or sqlite db path (db sources) — whatever the provider
/// recorded on the session.
fn build_transcript_index(
    anchor_profiles: &SourceProfileStore,
    cwd_profiles: Option<&SourceProfileStore>,
) -> std::collections::BTreeMap<String, (String, String)> {
    let profiles = match anchor_profiles.list_profiles() {
        Ok(found) if !found.is_empty() => found,
        _ => cwd_profiles
            .and_then(|p| p.list_profiles().ok())
            .unwrap_or_default(),
    };
    let mut index = std::collections::BTreeMap::new();
    for profile in profiles {
        let Ok(sessions) = brick_core::list_source_sessions(&profile, None) else {
            continue;
        };
        for session in sessions {
            index
                .entry(session.external_session_id.clone())
                .or_insert_with(|| {
                    (
                        session.source_app_id.clone(),
                        session.path.display().to_string(),
                    )
                });
        }
    }
    index
}

/// Recovers an `observed` rationale for steps that have a resolved transcript but
/// no asserted note: reads the origin session and lifts that turn's final
/// assistant message (see [`brick_core::turn_final_assistant_message`]). This is
/// the read-time half of "ingested history should still have a WHY" — the diff
/// gives WHO/WHEN/what, the turn's closing narration gives the WHY the code can't.
///
/// Invariants:
/// - never touches a step that already has a note (an `explicit` `link` always wins);
/// - sets `confidence = "observed"` so the agent can weigh it accordingly;
/// - is best-effort: a missing/unreadable transcript leaves the step unchanged.
fn enrich_observed_rationale(chain: &mut CausalChain) {
    for step in &mut chain.steps {
        if step.note.is_some() {
            continue;
        }
        let Some(transcript) = step.transcript.as_ref() else {
            continue;
        };
        let (Some(source), Some(session_ref), Some(session_id)) = (
            transcript.source.as_deref(),
            transcript.session_ref.as_deref(),
            transcript.session_id.as_deref(),
        ) else {
            continue;
        };
        let recovered = brick_core::turn_final_assistant_message(
            source,
            session_id,
            Some(std::path::Path::new(session_ref)),
            &step.occurred_at,
        );
        if let Ok(Some(message)) = recovered {
            step.note = Some(message);
            step.confidence = "observed".to_string();
        }
    }
}

/// Whether `explain` found genuinely nothing to say. A chain is empty ONLY when
/// the anchor resolved to no events at all — that is the real "no Brick record"
/// case. A single `diff.captured` step still carries WHO + mission_title + what,
/// which is real provenance; treating "no rationale note yet" as "no record"
/// wrongly pushed agents back to git even when Brick knew who/why-by-mission.
fn chain_is_empty(chain: &CausalChain) -> bool {
    chain.anchor.resolved_events.is_empty() || chain.steps.is_empty()
}

/// Strips a leading `repo_root` prefix so blame always queries a repo-relative
/// path, accepting either absolute or already-relative input.
fn normalize_repo_relative(repo_root: &std::path::Path, path: &str) -> String {
    let candidate = std::path::Path::new(path);
    if let Ok(stripped) = candidate.strip_prefix(repo_root) {
        return stripped.to_string_lossy().into_owned();
    }
    path.trim_start_matches("./").to_string()
}

/// Returns a trimmed non-empty string argument, or None.
fn opt_str_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Builds the actor for an MCP-authored write. MCP has no logged-in human, so
/// the actor is the calling tool: `actor_id`/`source` arg, else `mcp`.
fn mcp_actor(args: &Value) -> ActorRef {
    let actor_id = opt_str_arg(args, "actor_id")
        .or_else(|| opt_str_arg(args, "source"))
        .unwrap_or_else(|| "mcp".to_string());
    ActorRef {
        actor_type: ActorType::Agent,
        actor_id,
        display_name: None,
    }
}

/// The session a `link` write belongs to, if the caller named one. Lets the
/// recorded `diff.captured` carry a `session_id`, which `explain` later turns
/// into a transcript pointer — the seam that lets a curious agent open the
/// original session that produced a change. Accepts `session` or the more
/// explicit `session_id` arg; an empty/blank value resolves to `None`.
fn link_session_id(args: &Value) -> Option<SessionId> {
    opt_str_arg(args, "session")
        .or_else(|| opt_str_arg(args, "session_id"))
        .and_then(|value| SessionId::from_str(&value).ok())
}

/// snake_case wire string for a mission status (matches the serde rename).
fn mission_status_str(status: MissionStatus) -> &'static str {
    match status {
        MissionStatus::Planned => "planned",
        MissionStatus::Active => "active",
        MissionStatus::Blocked => "blocked",
        MissionStatus::Completed => "completed",
        MissionStatus::Archived => "archived",
    }
}

/// Parses a mission status from a wire string, erroring on unknown values.
fn parse_mission_status(raw: &str) -> Result<MissionStatus> {
    match raw.trim().to_lowercase().as_str() {
        "planned" => Ok(MissionStatus::Planned),
        "active" => Ok(MissionStatus::Active),
        "blocked" => Ok(MissionStatus::Blocked),
        "completed" => Ok(MissionStatus::Completed),
        "archived" => Ok(MissionStatus::Archived),
        other => Err(anyhow::anyhow!(
            "unknown mission status: {other} (planned|active|blocked|completed|archived)"
        )),
    }
}

/// Status for a create call: defaults to Planned when omitted.
fn mission_status_from_str(raw: Option<&str>) -> Result<MissionStatus> {
    match raw {
        Some(value) if !value.trim().is_empty() => parse_mission_status(value),
        _ => Ok(MissionStatus::Planned),
    }
}

/// Maps an artifact kind wire string to the enum, defaulting to Note.
fn artifact_kind_from_str(raw: Option<&str>) -> ArtifactKind {
    match raw.map(|value| value.trim().to_lowercase()).as_deref() {
        Some("decision") => ArtifactKind::Decision,
        Some("file_ref") => ArtifactKind::FileRef,
        Some("patch") => ArtifactKind::Patch,
        Some("review") => ArtifactKind::Review,
        Some("test_result") => ArtifactKind::TestResult,
        Some("acceptance") => ArtifactKind::Acceptance,
        _ => ArtifactKind::Note,
    }
}

fn success(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn tool_error(id: Value, message: &str) -> Value {
    // Tool failures are reported as a result with isError=true (MCP convention)
    // so the agent can read the message rather than the call hard-failing.
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": {
            "content": [ { "type": "text", "text": message } ],
            "isError": true
        }
    })
}

fn method_not_found(id: Value, method: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": -32601, "message": format!("method not found: {method}") }
    })
}

fn parse_error() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": null,
        "error": { "code": -32700, "message": "parse error" }
    })
}
