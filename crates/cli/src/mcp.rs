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
    capture_diff, discover_repo_root, explain_from_events, resolve_direct_anchor,
    resolve_file_anchor, resolve_file_line_anchor, AnnouncementStore, CausalChain,
    DiffCaptureRequest, LocalStore, SourceProfileStore, DEFAULT_EXPLAIN_DEPTH, MAX_EXPLAIN_DEPTH,
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
`path:line` (e.g. `crates/core/src/auth.rs:42`), an `artifact_*` id, a \
`mission_*` id, or an event id.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "anchor": {
                    "type": "string",
                    "description": "What to explain: `path:line`, an artifact id, a \
mission id, or an event id."
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
recover your reasoning with `explain`. Call this after a non-trivial edit. Three \
forms: (1) a standalone rationale — just a `note` explaining the change (e.g. \
'token refresh had a concurrency race; serialized it'); (2) a causal edge — set \
`cause` to the anchor that prompted this change (a `path`, `path:line`, \
artifact, mission, or event id) and pick a `relation`; (3) implementing a \
planned work item — set `cause` to its `mission_…` id with \
relation='derived_from' so the planning record connects to the real code. The \
effect is the code you just changed: give its `effect` anchor (a `path` or \
`path:line`), or omit `effect` to auto-capture your current uncommitted changes \
and bind the reason to exactly those files. Tip: if you made several unrelated \
edits, commit (or link) between them so each reason binds to the right files.",
        "inputSchema": {
            "type": "object",
            "properties": {
                "effect": {
                    "type": "string",
                    "description": "Anchor for the change you just made: a file \
`path`, a `path:line`, or an event id. Omit to auto-capture your current \
uncommitted changes (all touched files) and bind the reason to them."
                },
                "cause": {
                    "type": "string",
                    "description": "Optional anchor that caused/motivated this \
change: a `path`, `path:line`, artifact, mission, or event id. If you are \
implementing a planned work item, pass that `mission_…` id here (with \
relation='derived_from') so the planning record links to the actual code — do \
NOT just mention the mission in `note`, that leaves the graph disconnected. \
Omit for a standalone rationale."
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

    let events = store.read_all_events()?;
    let index = store.load_or_rebuild_index()?;

    // file:line anchors need git + the working tree; direct ids do not.
    let (anchor, anchored_path) = if let Some((path, line)) = parse_file_line(&anchor_input) {
        let cwd = std::env::current_dir()?;
        let repo_root = discover_repo_root(&cwd)?;
        let rel_path = normalize_repo_relative(&repo_root, &path);
        let anchor = resolve_file_line_anchor(store, &repo_root, &rel_path, line)?;
        (anchor, Some(rel_path))
    } else if looks_like_path(&anchor_input) {
        // A whole-file anchor (no `:line`) — agents very often ask about a file,
        // not a line. Match the file's change events directly instead of treating
        // the path as an opaque id (which wrongly reported "no record").
        let rel_path = std::env::current_dir()
            .ok()
            .and_then(|cwd| discover_repo_root(&cwd).ok())
            .map(|repo_root| normalize_repo_relative(&repo_root, &anchor_input))
            .unwrap_or_else(|| anchor_input.clone());
        (resolve_file_anchor(&events, &rel_path), Some(rel_path))
    } else {
        (resolve_direct_anchor(&events, &anchor_input), None)
    };

    let mut chain = explain_from_events(&index, &events, anchor, depth.min(MAX_EXPLAIN_DEPTH));
    enrich_transcripts(profiles, &mut chain);

    let mut value = serde_json::to_value(&chain)?;
    // `live` field: if another running session is touching the anchored file
    // right now, surface it so the agent avoids a cross-session edit conflict.
    // This is what replaced the standalone `sessions`/`claims` coordination tools.
    if let Some(path) = anchored_path {
        if let Ok(all_profiles) = profiles.list_profiles() {
            if let Some(broadcast) = build_live_broadcast(&all_profiles, &path, None) {
                if let Value::Object(map) = &mut value {
                    map.insert("live".to_string(), serde_json::to_value(broadcast)?);
                }
            }
        }
        if let Ok(announce_store) = AnnouncementStore::open_global() {
            if let Ok(claims) = announce_store.matching(&path) {
                if !claims.is_empty() {
                    if let Value::Object(map) = &mut value {
                        map.insert("active_claims".to_string(), serde_json::to_value(claims)?);
                    }
                }
            }
        }
    }

    if chain_is_empty(&chain) {
        if let Value::Object(map) = &mut value {
            map.insert(
                "note".to_string(),
                json!("No Brick record for this anchor yet. Brick only records causal \
edges for changes made while it was installed; fall back to git/grep here. As \
more changes flow through Brick, explain gets richer."),
            );
        }
    }

    Ok(value)
}

/// `link` dispatch: write a `causal.linked` event. Supports a standalone
/// rationale (note only) or a cross-event edge (cause anchor + relation).
fn link_tool_call(store: &LocalStore, args: &Value) -> Result<Value> {
    let events = store.read_all_events()?;

    // Track whether we synthesized a diff so the response can tell the agent
    // which files the rationale was bound to (otherwise it silently binds to
    // whatever it could resolve, which used to be an unrelated stale diff).
    let mut captured_files: Vec<String> = Vec::new();

    let effect_event = match opt_str_arg(args, "effect") {
        Some(anchor) => match resolve_anchor_to_event(store, &events, &anchor)? {
            Some(event_id) => event_id,
            // The anchor resolved to nothing. If it's a file path, the agent is
            // pointing at code it JUST edited with its own tools (no Brick event
            // yet) — capture the working diff and bind to that, exactly like the
            // no-effect path, instead of hard-erroring on a perfectly reasonable
            // anchor. Only a non-path anchor (a stale id) is a real error.
            None if looks_like_path(&anchor) => {
                match capture_working_diff_event(store, args, &mut captured_files)? {
                    Some(event_id) => event_id,
                    None => latest_diff_event(&events).ok_or_else(|| {
                        anyhow::anyhow!(
                            "effect anchor '{anchor}' has no Brick event and there are no \
uncommitted changes to capture"
                        )
                    })?,
                }
            }
            None => {
                return Err(anyhow::anyhow!("could not resolve effect anchor: {anchor}"));
            }
        },
        // No explicit effect: the agent just changed code with its own edit tools
        // (which produce no Brick event), so capture the current working diff and
        // bind the rationale to THAT — the files actually touched — instead of
        // guessing at the most recent prior diff (which mis-attributed the note).
        None => match capture_working_diff_event(store, args, &mut captured_files)? {
            Some(event_id) => event_id,
            // Working tree clean (e.g. already committed) — fall back to the most
            // recent captured diff so a follow-up rationale still lands somewhere.
            None => latest_diff_event(&events).ok_or_else(|| {
                anyhow::anyhow!(
                    "no effect given, no uncommitted changes to capture, and no recent diff to bind to"
                )
            })?,
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
        None,
        None,
        payload,
    )?;
    let event_id = event.event_id;
    store.append_event(&event)?;
    Ok(Some(event_id))
}

/// Planning-surface dispatch (mission / artifact tools).
fn dispatch_planning(store: &LocalStore, name: &str, args: &Value) -> Result<Value> {
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

/// The most recent `diff.captured` event, used as the default `link` effect when
/// the agent doesn't name one (it just changed something).
fn latest_diff_event(events: &[brick_protocol::TraceEvent]) -> Option<uuid::Uuid> {
    events
        .iter()
        .filter(|event| event.event_type == brick_protocol::EventType::DiffCaptured)
        .max_by_key(|event| event.occurred_at)
        .map(|event| event.event_id)
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
fn enrich_transcripts(_profiles: &SourceProfileStore, chain: &mut CausalChain) {
    // The core already populated `session_id` on each step's transcript pointer.
    // Resolving session_id → concrete path requires per-source lookups that vary
    // by tool; for now we keep the session_id pointer (the agent can open it via
    // its own tooling) and leave richer path resolution to a follow-up. This
    // function is the seam where that resolution lands.
    let _ = chain;
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
/// the actor is the calling tool: `actor_id`/`source` arg, else `mcp`. This
/// mirrors how `announce_work` attributes claims.
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
