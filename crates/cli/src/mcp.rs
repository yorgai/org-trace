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

use std::collections::HashSet;
use std::io::{BufRead, Write};
use std::str::FromStr;

use anyhow::Result;
use brick_core::{
    blame_file, blame_line_range_history, discover_repo_root, list_source_sessions,
    AnnouncementStore, ClaimValidity, LocalStore, NewAnnouncement, SourceProfileStore,
};
use brick_protocol::{
    ActorRef, ActorType, ArtifactCreatedPayload, ArtifactFileRefRecordedPayload, ArtifactId,
    ArtifactKind, FileRefId, MissionCreatedPayload, MissionId, MissionStatus,
    MissionUpdatedPayload, ProjectId, SessionId, TraceEvent,
};
use chrono::Duration;
use serde_json::{json, Value};

use crate::history::{
    build_chunks_response, build_live_broadcast, collect_live_sessions, live_session_row,
    read_profile,
};
use crate::metadata::{build_query_response, build_recall_response};

/// MCP protocol revision this server speaks.
const PROTOCOL_VERSION: &str = "2024-11-05";
/// Default source scope for tools: search every indexed tool's history.
const DEFAULT_SOURCE: &str = crate::defaults::SOURCE_ALL;
/// Default recall/query result cap, kept small so tool output stays triage-sized.
const DEFAULT_LIMIT: usize = crate::defaults::RESULT_LIMIT;
/// Default per-field truncation for `show_session`, matching the CLI default.
const DEFAULT_MAX_FIELD_BYTES: usize = crate::defaults::MAX_FIELD_BYTES;

/// Runs the stdio JSON-RPC loop until stdin closes. Never returns an error to the
/// caller for per-request failures — those become JSON-RPC error responses; only
/// a fatal stdout write failure propagates.
pub fn serve(profiles: &SourceProfileStore, store: &LocalStore) -> Result<()> {
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
            "initialize" => success(id, initialize_result()),
            "tools/list" => success(id, tools_list_result()),
            "tools/call" => match handle_tool_call(profiles, store, &request) {
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

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": {
            "name": "brick",
            "version": env!("CARGO_PKG_VERSION"),
        },
        "instructions": "Brick gives any agent a shared work surface across tools: \
    memory, planning, and coordination. \
    MEMORY — search for an open question or to find past work by topic, log_file before \
    editing a file to see who changed it and why, show_session to page through a \
    session's transcript. \
    PLANNING — at the start of a task call status (what am I working on) and \
    mission_list (what's in flight); turn a request into a tracked goal with \
    mission action='create'; as work moves, mission action='update' its \
    status; log deliverables with artifact_add and back them with artifact_attach. \
    COORDINATION — before a non-trivial edit call claim so other sessions hold \
    off, and log_file surfaces any active claims on the file you ask about. \
    A natural flow: status → mission_list → mission(create) → work → \
    artifact_add → artifact_attach → claim."
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "log_file",
                "description": "Recall who previously changed a file and why, across \
    every coding tool on this machine. Returns a one-line summary plus per-session \
    intent and change size. Call before editing a file.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Repo-relative or absolute file path."
                        }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "blame",
                "description": "Owner provenance for a file: returns the lines \
    attributable to an AI agent in THIS machine's local session history (which \
    agent / session / mission produced them), anchored to git commits and the \
    append-only event log. This is deliberately NOT whole-file authorship — lines \
    changed by others, edited by hand, or never captured are reported as a \
    skipped count, not guessed. Use it to find which lines are AI-owned and by \
    whom before changing them. Optionally restrict to a line range, or pass \
    include_unattributed=true to get every line verbatim.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Repo-relative file path to blame."
                        },
                        "line_start": {
                            "type": "integer",
                            "description": "Optional 1-based first line to include."
                        },
                        "line_end": {
                            "type": "integer",
                            "description": "Optional 1-based last line to include."
                        },
                        "include_unattributed": {
                            "type": "boolean",
                            "description": "Return every line verbatim (including \
    non-owned ones) instead of only AI-owned lines plus a skipped count. Default false."
                        }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "log_line",
                "description": "Full change history of a line range: every commit \
    that touched lines [line_start, line_end] of a file (newest first), each \
    tagged with the AI session that produced it when this machine's local history \
    can attribute it. Unlike blame (which only resolves the single LAST \
    commit per line), this lists ALL the sessions that ever changed this code. \
    Commits not in local records (others' commits, hand edits, or \
    squash/rebase-rewritten history) are listed with attributed=false, not \
    guessed. Use it to trace how a specific block of code evolved across sessions.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Repo-relative file path."
                        },
                        "line_start": {
                            "type": "integer",
                            "description": "1-based first line of the range (required)."
                        },
                        "line_end": {
                            "type": "integer",
                            "description": "1-based last line of the range (required)."
                        }
                    },
                    "required": ["path", "line_start", "line_end"]
                }
            },
            {
                "name": "search",
                "description": "Free-text search over session metadata (title, \
    intent, touched files, repo, branch) to find past sessions by topic. Returns \
    matches newest-first, each with a transcript pointer.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "query": {
                            "type": "string",
                            "description": "Keywords to match against session metadata."
                        }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "show_session",
                "description": "Page through one session's full transcript chunks. \
    Supports offset/limit pagination and per-field truncation so large tool outputs \
    don't overflow context; set max_field_bytes to 0 to fetch one chunk untruncated.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "source": {
                            "type": "string",
                            "description": "Source id (e.g. claude_code, codex_app, cursor_ide, orgii)."
                        },
                        "session_id": {
                            "type": "string",
                            "description": "External session id from a search hit."
                        },
                        "offset": { "type": "integer", "description": "Chunk offset (default 0)." },
                        "limit": { "type": "integer", "description": "Max chunks (default 50)." },
                        "max_field_bytes": {
                            "type": "integer",
                            "description": "Truncate string values over this many bytes; 0 disables (default 2000)."
                        }
                    },
                    "required": ["source", "session_id"]
                }
            },
            {
                "name": "sessions",
                "description": "List AI coding sessions that appear to be running \
    RIGHT NOW across every tool on this machine (Claude Code, Codex, Cursor, …). \
    Each result includes its work scope (repo or working dir), the file(s) it \
    recently touched, and what it is doing. Use this to coordinate with other \
    in-flight sessions and avoid editing files someone else is actively changing.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "description": "Optional path prefix; only sessions whose \
    work scope is at or under this path are returned. Omit for all live sessions."
                        }
                    }
                }
            },
            {
                "name": "claim",
                "description": "Post a heads-up on the cross-session bulletin board \
    BEFORE you start editing: 'I'm changing X, hold off'. Other sessions calling \
    log_file on a matching path will see your note and avoid clobbering your \
    work. The claim auto-expires (default 4h) so you don't have to remember to \
    clear it. Call this when you begin a non-trivial change to a file or area.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "scope": {
                            "type": "string",
                            "description": "File path or glob you are claiming, e.g. \
    'crates/core/src/auth.rs' or 'crates/cli/src/**/*.rs'. A bare filename like \
    'auth.rs' matches that file anywhere."
                        },
                        "message": {
                            "type": "string",
                            "description": "One line: what you're doing and any warning, \
    e.g. 'refactoring token validation, please hold off ~1h'."
                        },
                        "session_id": {
                            "type": "string",
                            "description": "Your session id, so others can tell who to \
    coordinate with and the claim clears when your session ends. Optional."
                        },
                        "source": {
                            "type": "string",
                            "description": "Your tool/app id (e.g. claude_code). Optional."
                        },
                        "ttl_minutes": {
                            "type": "integer",
                            "description": "Minutes until the claim auto-expires (default 240)."
                        }
                    },
                    "required": ["scope", "message"]
                }
            },
            {
                "name": "claims",
                "description": "List active bulletin-board claims (other sessions' \
    'I'm working on X' notes). With `path`, only claims covering that path. Call \
    before editing to check nobody has claimed the area you're about to touch.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Only show claims whose scope covers this path. \
    Omit to list every active claim."
                        }
                    }
                }
            },
            {
                "name": "status",
                "description": "Report your current work context: the active org, \
    project, mission (work item), and session Brick has on record. Call this at the \
    START of a task to know what you're working on and where new work should be \
    filed. Pairs with mission_list to see what's in flight.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "mission_list",
                "description": "List missions (work items / goals) Brick is tracking, \
    newest first. Use this to see 'what is in flight' before starting work, to find \
    an existing mission to attach output to, or to pick up an unfinished task. \
    Optionally filter by status or project.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "description": "Filter by status: planned | active | blocked \
    | completed | archived. Omit for all."
                        },
                        "project": {
                            "type": "string",
                            "description": "Filter to one project id. Omit for all projects."
                        },
                        "limit": {
                            "type": "integer",
                            "description": "Max missions to return (default 50)."
                        }
                    }
                }
            },
            {
                "name": "show_mission",
                "description": "Show one mission in detail: status, description, and the \
    sessions and artifacts linked to it. Use to inspect a work item before updating \
    it or recording output against it.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "mission": {
                            "type": "string",
                            "description": "The mission id to show (e.g. msn-…)."
                        }
                    },
                    "required": ["mission"]
                }
            },
            {
                "name": "mission",
                "description": "Create or update a mission (work item / goal) — Brick's \
    planning primitive. action='create' opens a new work item under a project; \
    action='update' changes its title/description/status as work progresses \
    (planned→active→blocked→completed). Use this to turn a user request into a \
    tracked goal, then record artifacts and evidence against it.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "action": {
                            "type": "string",
                            "description": "'create' a new mission or 'update' an existing one.",
                            "enum": ["create", "update"]
                        },
                        "mission": {
                            "type": "string",
                            "description": "Mission id — REQUIRED for action='update'."
                        },
                        "project": {
                            "type": "string",
                            "description": "Project id this mission belongs to — REQUIRED \
    for action='create'. For update, moves the mission to another project."
                        },
                        "title": {
                            "type": "string",
                            "description": "Short imperative goal title, e.g. 'Add OAuth \
    login'. Required for create."
                        },
                        "description": {
                            "type": "string",
                            "description": "Optional longer description / acceptance notes."
                        },
                        "status": {
                            "type": "string",
                            "description": "planned | active | blocked | completed | \
    archived. Defaults to 'planned' on create.",
                            "enum": ["planned", "active", "blocked", "completed", "archived"]
                        },
                        "session_id": { "type": "string", "description": "Your session id (optional)." },
                        "source": { "type": "string", "description": "Your tool/app id (optional)." }
                    },
                    "required": ["action"]
                }
            },
            {
                "name": "artifact_add",
                "description": "Record a deliverable you produced (a PR, a design doc, a \
    decision, a test result) and link it to a mission. This closes the planning \
    loop: a mission states the goal, an artifact is the proof of work. Call after \
    finishing a meaningful piece of work.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "title": {
                            "type": "string",
                            "description": "What the artifact is, e.g. 'PR #42: OAuth login'."
                        },
                        "kind": {
                            "type": "string",
                            "description": "decision | file_ref | patch | review | \
    test_result | acceptance | note. Defaults to 'note'.",
                            "enum": ["decision", "file_ref", "patch", "review", "test_result", "acceptance", "note"]
                        },
                        "body": {
                            "type": "string",
                            "description": "Optional details / link / summary."
                        },
                        "mission": {
                            "type": "string",
                            "description": "Mission id to link this artifact to (optional \
    but recommended so the work item shows its outputs)."
                        },
                        "session_id": { "type": "string", "description": "Your session id (optional)." },
                        "source": { "type": "string", "description": "Your tool/app id (optional)." }
                    },
                    "required": ["title"]
                }
            },
            {
                "name": "artifact_attach",
                "description": "Attach a file-path piece of evidence to an artifact — the \
    concrete file(s) that back up a deliverable, forming an auditable trail. Call \
    after artifact_add to point at the files the work touched.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "artifact": {
                            "type": "string",
                            "description": "Artifact id the evidence belongs to (from artifact_add)."
                        },
                        "path": {
                            "type": "string",
                            "description": "File path the artifact represents or touched."
                        },
                        "session_id": { "type": "string", "description": "Your session id (optional)." },
                        "source": { "type": "string", "description": "Your tool/app id (optional)." }
                    },
                    "required": ["artifact", "path"]
                }
            }
        ]
    })
}

/// Tools that require a Brick account (soft login gate). Line-level blame and
/// planning/artifact tools are gated; file-level recall and live-awareness are
/// free. Defined unconditionally so the tool set is identical across builds —
/// only the *enforcement* (`login_required_error`) is feature-gated.
fn tool_requires_login(name: &str) -> bool {
    matches!(
        name,
        "blame" | "log_line" | "mission" | "artifact_add" | "artifact_attach"
    )
}

/// Returns a structured `login_required` error payload when a gated tool is
/// called without a valid login, or `None` to allow the call.
///
/// In the proprietary `sync` build this consults `brick_sync::identity`. In the
/// open-source build there is no login concept, so it always allows (returns
/// `None`) — the gate is a no-op and the tool runs unguarded.
#[cfg(feature = "sync")]
fn login_required_error(name: &str) -> Option<Value> {
    if brick_sync::is_logged_in() {
        return None;
    }
    Some(json!({
        "error": "login_required",
        "tool": name,
        "hint": "This tool needs a Brick account. Run `brick login` first.",
    }))
}

#[cfg(not(feature = "sync"))]
fn login_required_error(_name: &str) -> Option<Value> {
    None
}

/// Maps a retired tool name onto its current Git-aligned name. Unknown names
/// pass through unchanged. Kept for one transition cycle so agents with the old
/// names baked into memory files / MCP configs keep working.
fn canonical_tool_name(name: &str) -> &str {
    match name {
        "recall_file" => "log_file",
        "blame_file" => "blame",
        "blame_history" => "log_line",
        "explore_memory" | "search_sessions" => "search",
        "read_session" => "show_session",
        "live_sessions" => "sessions",
        "current_context" => "status",
        "announce_work" => "claim",
        "list_announcements" => "claims",
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
    request: &Value,
) -> Result<Value> {
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let raw_name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
    // Old tool names are accepted for one transition cycle and mapped onto the
    // current Git-aligned names so already-installed agent memory / MCP configs
    // keep working. Everything below operates on the canonical (new) name.
    let name = canonical_tool_name(raw_name);
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    // Soft login gate: line-level blame and planning tools require a Brick
    // account. Free tools (file-level recall + live-awareness) are never gated.
    // The gate is only compiled into the proprietary `sync` build; the
    // open-source binary has no login concept and never blocks. See
    // `brick_sync::identity` for why this is a registration hook, not a security
    // boundary. The denial flows through the same content wrapper below so the
    // client still receives a well-formed tool result.
    let gate_denial = if tool_requires_login(name) {
        login_required_error(name)
    } else {
        None
    };
    let payload = if let Some(denial) = gate_denial {
        denial
    } else {
        match name {
            "search" => {
                let query = args
                    .get("query")
                    .or_else(|| args.get("question"))
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("missing required string argument: query"))?
                    .to_string();
                let response =
                    build_query_response(profiles, &query, DEFAULT_SOURCE, DEFAULT_LIMIT)?;
                serde_json::to_value(response)?
            }
            "log_file" => {
                let path = str_arg(&args, "path")?;
                let recall =
                    build_recall_response(store, profiles, &path, DEFAULT_SOURCE, DEFAULT_LIMIT)?;
                let mut value = serde_json::to_value(recall)?;
                // MCP-ONLY enrichment (not produced by the CLI `metadata recall`):
                // the MCP surface adds real-time coordination signals on top of the
                // shared recall payload. Tiered live-awareness — if another running
                // session collides with this file or its work scope, attach a
                // broadcast so the agent can avoid a cross-session edit conflict.
                // Silent when there is no overlap. This asymmetry is deliberate: it
                // is a coordination capability that only makes sense for a live agent,
                // so it lives on the MCP path, not in the CLI's batch output.
                if let Ok(all_profiles) = profiles.list_profiles() {
                    if let Some(broadcast) = build_live_broadcast(&all_profiles, &path, None) {
                        if let Value::Object(map) = &mut value {
                            map.insert(
                                "live_broadcast".to_string(),
                                serde_json::to_value(broadcast)?,
                            );
                        }
                    }
                }
                // Bulletin-board claims: another session may have explicitly asked
                // others to hold off this file/area. Surface those notes directly,
                // dropping any whose owning session has already ended.
                if let Ok(announce_store) = AnnouncementStore::open_global() {
                    let validity = build_claim_validity(profiles);
                    if let Ok(claims) = announce_store.matching_live(&path, validity) {
                        if !claims.is_empty() {
                            if let Value::Object(map) = &mut value {
                                map.insert(
                                    "active_claims".to_string(),
                                    json!({
                                        "count": claims.len(),
                                        "message": "Another session posted a heads-up covering \
                                    this path. Review before editing.",
                                        "claims": claims,
                                    }),
                                );
                            }
                        }
                    }
                }
                value
            }
            "blame" => {
                let path = str_arg(&args, "path")?;
                let line_start = usize_arg(&args, "line_start");
                let line_end = usize_arg(&args, "line_end");
                let include_unattributed = args
                    .get("include_unattributed")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                blame_response(store, &path, line_start, line_end, include_unattributed)?
            }
            "log_line" => {
                let path = str_arg(&args, "path")?;
                let line_start = usize_arg(&args, "line_start")
                    .ok_or_else(|| anyhow::anyhow!("log_line requires line_start"))?;
                let line_end = usize_arg(&args, "line_end")
                    .ok_or_else(|| anyhow::anyhow!("log_line requires line_end"))?;
                blame_history_response(store, &path, line_start as u64, line_end as u64)?
            }
            "show_session" => {
                let source = str_arg(&args, "source")?;
                let session_id = str_arg(&args, "session_id")?;
                let offset = usize_arg(&args, "offset").unwrap_or(0);
                let limit = usize_arg(&args, "limit").unwrap_or(50);
                let max_field_bytes =
                    usize_arg(&args, "max_field_bytes").unwrap_or(DEFAULT_MAX_FIELD_BYTES);
                let profile = read_profile(profiles, &source)?;
                let response = build_chunks_response(
                    &profile,
                    &source,
                    &session_id,
                    limit,
                    offset,
                    max_field_bytes,
                )?;
                serde_json::to_value(response)?
            }
            "sessions" => {
                let scope = args.get("scope").and_then(Value::as_str);
                let all_profiles = profiles.list_profiles()?;
                let live = collect_live_sessions(&all_profiles, 0, 50);
                let rows: Vec<Value> = live
                    .iter()
                    .filter(|session| match scope {
                        Some(prefix) => brick_core::work_scope(session)
                            .map(|path| path.starts_with(prefix))
                            .unwrap_or(false),
                        None => true,
                    })
                    .map(|session| serde_json::to_value(live_session_row(session)))
                    .collect::<std::result::Result<_, _>>()?;
                json!({
                    "count": rows.len(),
                    "sessions": rows,
                    "note": "These sessions appear to be running now. Avoid editing files \
                they recently touched; call show_session to see what one is doing."
                })
            }
            "claim" => {
                let scope = str_arg(&args, "scope")?;
                let message = str_arg(&args, "message")?;
                let session_id = args
                    .get("session_id")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("mcp-session")
                    .to_string();
                let source_id = args
                    .get("source")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .unwrap_or("mcp")
                    .to_string();
                let ttl = usize_arg(&args, "ttl_minutes")
                    .map(|minutes| Duration::minutes(minutes as i64));
                let work_dir = std::env::current_dir()
                    .ok()
                    .map(|path| path.display().to_string());
                let announce_store = AnnouncementStore::open_global()?;
                let announcement = announce_store.publish(NewAnnouncement {
                    source_id,
                    session_id,
                    scope,
                    message,
                    work_dir,
                    ttl,
                })?;
                json!({
                    "published": announcement,
                    "note": "Heads-up posted. Other sessions calling log_file on a \
                matching path will see it. It auto-expires; no need to clear it manually."
                })
            }
            "claims" => {
                let announce_store = AnnouncementStore::open_global()?;
                let validity = build_claim_validity(profiles);
                let claims = match args.get("path").and_then(Value::as_str) {
                    Some(path) if !path.is_empty() => {
                        announce_store.matching_live(path, validity)?
                    }
                    _ => announce_store.list_live(validity)?,
                };
                json!({ "count": claims.len(), "announcements": claims })
            }
            "status" => {
                // Load-or-rebuild: the staleness check rebuilds only when the queue
                // grew since the cache was written (e.g. events just appended by
                // other MCP calls), so back-to-back reads in one flow don't each pay
                // a full rebuild + disk rewrite.
                let index = store.load_or_rebuild_index()?;
                let context = store.read_current_context().ok().flatten();
                let current_mission = context
                    .as_ref()
                    .and_then(|ctx| ctx.mission_id.as_ref())
                    .and_then(|id| index.missions.get(id.as_str()));
                json!({
                    "current": context,
                    "current_mission": current_mission,
                    "counts": {
                        "orgs": index.orgs.len(),
                        "projects": index.projects.len(),
                        "missions": index.missions.len(),
                        "sessions": index.sessions.len(),
                        "artifacts": index.artifacts.len(),
                    },
                    "note": "Use mission_list to see in-flight work, mission to \
                open or update a goal, and artifact_add to log deliverables."
                })
            }
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
                let limit = usize_arg(&args, "limit").unwrap_or(50);
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
                // Newest activity first so "what's in flight" surfaces at the top.
                missions.sort_by_key(|mission| std::cmp::Reverse(mission.last_event_at));
                missions.truncate(limit);
                json!({ "count": missions.len(), "missions": missions })
            }
            "show_mission" => {
                let mission = str_arg(&args, "mission")?;
                let index = store.load_or_rebuild_index()?;
                let item = index
                    .missions
                    .get(&mission)
                    .ok_or_else(|| anyhow::anyhow!("mission not found: {mission}"))?;
                serde_json::to_value(item)?
            }
            "mission" => {
                let action = str_arg(&args, "action")?;
                let actor = mcp_actor(&args);
                match action.as_str() {
                    "create" => {
                        let project = str_arg(&args, "project")?;
                        let title = str_arg(&args, "title")?;
                        let project_id = ProjectId::from_str(&project)
                            .map_err(|err| anyhow::anyhow!("invalid project id: {err}"))?;
                        let mission_id = MissionId::new();
                        let event = TraceEvent::mission_created(
                            actor,
                            mission_id.clone(),
                            MissionCreatedPayload {
                                project_id,
                                title,
                                description: opt_str_arg(&args, "description"),
                                status: mission_status_from_str(
                                    args.get("status").and_then(Value::as_str),
                                )?,
                                repo_context_id: None,
                            },
                        )?;
                        store.append_event(&event)?;
                        json!({
                            "created": true,
                            "mission_id": mission_id.to_string(),
                            "note": "Mission opened. Record deliverables against it with \
                        artifact_add, and update its status with mission action='update'."
                        })
                    }
                    "update" => {
                        let mission = str_arg(&args, "mission")?;
                        let mission_id = MissionId::from_str(&mission)
                            .map_err(|err| anyhow::anyhow!("invalid mission id: {err}"))?;
                        let project_id = match args.get("project").and_then(Value::as_str) {
                            Some(project) if !project.is_empty() => Some(
                                ProjectId::from_str(project)
                                    .map_err(|err| anyhow::anyhow!("invalid project id: {err}"))?,
                            ),
                            _ => None,
                        };
                        let title = opt_str_arg(&args, "title");
                        let description = opt_str_arg(&args, "description");
                        let status = match args.get("status").and_then(Value::as_str) {
                            Some(raw) if !raw.is_empty() => Some(parse_mission_status(raw)?),
                            _ => None,
                        };
                        if project_id.is_none()
                            && title.is_none()
                            && description.is_none()
                            && status.is_none()
                        {
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
                        json!({ "updated": true, "mission_id": mission_id.to_string() })
                    }
                    other => {
                        return Err(anyhow::anyhow!(
                        "unknown mission action: {other} (expected 'create' or 'update')"
                    ))
                    }
                }
            }
            "artifact_add" => {
                let title = str_arg(&args, "title")?;
                let actor = mcp_actor(&args);
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
                        artifact_kind: artifact_kind_from_str(
                            args.get("kind").and_then(Value::as_str),
                        ),
                        title,
                        body: opt_str_arg(&args, "body"),
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
                let artifact = str_arg(&args, "artifact")?;
                let path = str_arg(&args, "path")?;
                let actor = mcp_actor(&args);
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
            other => return Err(anyhow::anyhow!("unknown tool: {other}")),
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

/// Builds the `blame_file` tool response as **owner provenance**: the lines this
/// machine's local AI session history can attribute to an agent, plus a count of
/// the lines it deliberately does not claim. By default only attributed (owner)
/// lines are returned; the rest collapse into `skipped_lines` so the agent gets a
/// precise, honest answer instead of a wall of `unattributed` noise. Pass
/// `include_unattributed=true` to get every line verbatim (debugging). Resolves
/// the repo root from the current working directory (where `brick mcp-serve` ran).
fn blame_response(
    store: &LocalStore,
    path: &str,
    line_start: Option<usize>,
    line_end: Option<usize>,
    include_unattributed: bool,
) -> Result<Value> {
    let cwd = std::env::current_dir()?;
    let repo_root = discover_repo_root(&cwd)?;
    let rel_path = normalize_repo_relative(&repo_root, path);
    let mut lines = blame_file(store, &repo_root, &rel_path)?;
    if let Some(start) = line_start {
        lines.retain(|line| line.line_no as usize >= start);
    }
    if let Some(end) = line_end {
        lines.retain(|line| line.line_no as usize <= end);
    }
    let total = lines.len();
    let is_owned =
        |line: &brick_core::BlameLine| line.session_id.is_some() || line.actor_id.is_some();
    let attributed = lines.iter().filter(|line| is_owned(line)).count();
    let skipped = total - attributed;

    if include_unattributed {
        return Ok(json!({
            "path": rel_path,
            "mode": "full",
            "line_count": total,
            "owner_lines": attributed,
            "skipped_lines": skipped,
            "lines": lines,
        }));
    }

    let owner_lines: Vec<_> = lines.into_iter().filter(is_owned).collect();
    Ok(json!({
        "path": rel_path,
        "mode": "owner",
        "line_count": total,
        "owner_lines": attributed,
        "skipped_lines": skipped,
        "note": format!(
            "Owner provenance: {attributed} line(s) attributable to an AI agent in this \
    machine's local session history; {skipped} line(s) are not in local records (changed by \
    others, edited by hand, or never captured) and are not guessed. Pass \
    include_unattributed=true to see every line."
        ),
        "lines": owner_lines,
    }))
}

/// Builds the `blame_history` tool response: the FULL change history of a line
/// range — every commit that touched `[line_start, line_end]` (newest first),
/// each tagged with the owner session that captured it when attributable. This is
/// the "all the sessions that ever changed this code" view, distinct from
/// `blame_file`, which only resolves the single last commit per line. Commits
/// Brick cannot attribute are listed with `attributed=false`, not guessed.
fn blame_history_response(
    store: &LocalStore,
    path: &str,
    line_start: u64,
    line_end: u64,
) -> Result<Value> {
    let cwd = std::env::current_dir()?;
    let repo_root = discover_repo_root(&cwd)?;
    let rel_path = normalize_repo_relative(&repo_root, path);
    let touches = blame_line_range_history(store, &repo_root, &rel_path, line_start, line_end)?;
    let attributed = touches.iter().filter(|touch| touch.attributed).count();
    Ok(json!({
        "path": rel_path,
        "line_start": line_start,
        "line_end": line_end,
        "commit_count": touches.len(),
        "attributed_commits": attributed,
        "note": format!(
            "Full history of lines {line_start}-{line_end}: {} commit(s) touched this range, \
    {attributed} attributable to a local AI session. Commits with attributed=false are not in \
    local records (others' commits, hand edits, or squash/rebase-rewritten history) and are not \
    guessed.",
            touches.len()
        ),
        "history": touches,
    }))
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

/// Builds the `(source_id, session_id) -> ClaimValidity` judge used to retire
/// bulletin-board claims whose owning session has ended.
///
/// We enumerate every source's sessions once (liveness filled per session) and
/// fold them into two lookups: the set of sessions seen at all, and the subset
/// currently active. The returned closure is then pure:
/// - active                       -> `Live`   (keep)
/// - seen but not active          -> `Dead`   (retire early)
/// - never seen (CLI / bare `mcp`) -> `Unknown` (keep on TTL alone)
///
/// A source that fails to enumerate is simply absent from both sets, so its
/// claims fall through to `Unknown` — one broken source never mass-retires
/// claims it cannot speak to.
fn build_claim_validity(profiles: &SourceProfileStore) -> impl Fn(&str, &str) -> ClaimValidity {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut live: HashSet<(String, String)> = HashSet::new();
    if let Ok(all_profiles) = profiles.list_profiles() {
        for profile in &all_profiles {
            let Ok(sessions) = list_source_sessions(profile, Some(200)) else {
                continue;
            };
            for session in sessions {
                let key = (
                    session.source_app_id.clone(),
                    session.external_session_id.clone(),
                );
                if brick_core::is_active(&session) {
                    live.insert(key.clone());
                }
                seen.insert(key);
            }
        }
    }
    move |source_id: &str, session_id: &str| {
        let key = (source_id.to_string(), session_id.to_string());
        if live.contains(&key) {
            ClaimValidity::Live
        } else if seen.contains(&key) {
            ClaimValidity::Dead
        } else {
            ClaimValidity::Unknown
        }
    }
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
