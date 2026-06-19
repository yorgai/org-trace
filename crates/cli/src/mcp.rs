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
    list_source_sessions, AnnouncementStore, ClaimValidity, LocalStore, NewAnnouncement,
    SourceProfileStore,
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
const DEFAULT_SOURCE: &str = "all";
/// Default recall/query result cap, kept small so tool output stays triage-sized.
const DEFAULT_LIMIT: usize = 10;
/// Default per-field truncation for `read_session`, matching the CLI default.
const DEFAULT_MAX_FIELD_BYTES: usize = 2000;

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
    MEMORY — explore_memory for an open question, recall_file before editing a file, \
    search_sessions to find past work by topic, read_session to page through a \
    session's transcript. \
    PLANNING — at the start of a task call current_context (what am I working on) and \
    list_missions (what's in flight); turn a request into a tracked goal with \
    manage_mission action='create'; as work moves, manage_mission action='update' its \
    status; log deliverables with record_artifact and back them with attach_evidence. \
    COORDINATION — before a non-trivial edit call announce_work so other sessions hold \
    off, and recall_file surfaces any active claims on the file you ask about. \
    A natural flow: current_context → list_missions → manage_mission(create) → work → \
    record_artifact → attach_evidence → announce_work."
    })
}

fn tools_list_result() -> Value {
    json!({
        "tools": [
            {
                "name": "explore_memory",
                "description": "Answer an open question about past AI coding work \
    by searching cross-tool session history and returning a synthesized summary of \
    the most relevant prior sessions (intent, tool, when, transcript pointer). Use \
    this first when you want context but don't have a specific file path.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "question": {
                            "type": "string",
                            "description": "Natural-language question or topic, e.g. \
    'how did we fix the auth token race' or 'pagination work in the CLI'."
                        }
                    },
                    "required": ["question"]
                }
            },
            {
                "name": "recall_file",
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
                "name": "search_sessions",
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
                "name": "read_session",
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
                            "description": "External session id from a search/recall hit."
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
                "name": "live_sessions",
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
                "name": "announce_work",
                "description": "Post a heads-up on the cross-session bulletin board \
    BEFORE you start editing: 'I'm changing X, hold off'. Other sessions calling \
    recall_file on a matching path will see your note and avoid clobbering your \
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
                "name": "list_announcements",
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
                "name": "current_context",
                "description": "Report your current work context: the active org, \
    project, mission (work item), and session Brick has on record. Call this at the \
    START of a task to know what you're working on and where new work should be \
    filed. Pairs with list_missions to see what's in flight.",
                "inputSchema": { "type": "object", "properties": {} }
            },
            {
                "name": "list_missions",
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
                "name": "manage_mission",
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
                "name": "record_artifact",
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
                "name": "attach_evidence",
                "description": "Attach a file-path piece of evidence to an artifact — the \
    concrete file(s) that back up a deliverable, forming an auditable trail. Call \
    after record_artifact to point at the files the work touched.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "artifact": {
                            "type": "string",
                            "description": "Artifact id the evidence belongs to (from record_artifact)."
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

fn handle_tool_call(
    profiles: &SourceProfileStore,
    store: &LocalStore,
    request: &Value,
) -> Result<Value> {
    let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
    let name = params
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing tool name"))?;
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));

    let payload = match name {
        "explore_memory" => {
            let question = str_arg(&args, "question")?;
            let query = build_query_response(profiles, &question, DEFAULT_SOURCE, DEFAULT_LIMIT)?;
            explore_summary(&question, &query)
        }
        "recall_file" => {
            let path = str_arg(&args, "path")?;
            let recall =
                build_recall_response(store, profiles, &path, DEFAULT_SOURCE, DEFAULT_LIMIT)?;
            let mut value = serde_json::to_value(recall)?;
            // Tiered live-awareness: if another running session collides with this
            // file or its work scope, attach a broadcast so the agent can avoid a
            // cross-session edit conflict. Silent when there is no overlap.
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
        "search_sessions" => {
            let query = str_arg(&args, "query")?;
            let response = build_query_response(profiles, &query, DEFAULT_SOURCE, DEFAULT_LIMIT)?;
            serde_json::to_value(response)?
        }
        "read_session" => {
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
        "live_sessions" => {
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
            they recently touched; call read_session to see what one is doing."
            })
        }
        "announce_work" => {
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
            let ttl =
                usize_arg(&args, "ttl_minutes").map(|minutes| Duration::minutes(minutes as i64));
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
                "note": "Heads-up posted. Other sessions calling recall_file on a \
            matching path will see it. It auto-expires; no need to clear it manually."
            })
        }
        "list_announcements" => {
            let announce_store = AnnouncementStore::open_global()?;
            let validity = build_claim_validity(profiles);
            let claims = match args.get("path").and_then(Value::as_str) {
                Some(path) if !path.is_empty() => announce_store.matching_live(path, validity)?,
                _ => announce_store.list_live(validity)?,
            };
            json!({ "count": claims.len(), "announcements": claims })
        }
        "current_context" => {
            // Rebuild (not load) so reads reflect events just written by other MCP
            // calls — the cached index goes stale the moment append_event runs.
            let index = store.rebuild_index()?;
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
                "note": "Use list_missions to see in-flight work, manage_mission to \
            open or update a goal, and record_artifact to log deliverables."
            })
        }
        "list_missions" => {
            let index = store.rebuild_index()?;
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
            let index = store.rebuild_index()?;
            let item = index
                .missions
                .get(&mission)
                .ok_or_else(|| anyhow::anyhow!("mission not found: {mission}"))?;
            serde_json::to_value(item)?
        }
        "manage_mission" => {
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
                    record_artifact, and update its status with manage_mission action='update'."
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
                            "manage_mission update needs at least one of project, title, description, or status"
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
                        "unknown manage_mission action: {other} (expected 'create' or 'update')"
                    ))
                }
            }
        }
        "record_artifact" => {
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
                    artifact_kind: artifact_kind_from_str(args.get("kind").and_then(Value::as_str)),
                    title,
                    body: opt_str_arg(&args, "body"),
                    repo_context_id: None,
                },
            )?;
            store.append_event(&event)?;
            json!({
                "recorded": true,
                "artifact_id": artifact_id.to_string(),
                "note": "Deliverable logged. Attach the backing files with attach_evidence."
            })
        }
        "attach_evidence" => {
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
    };

    // MCP tool results wrap content blocks; we hand back the JSON as text so the
    // agent gets the full structured payload it can parse.
    Ok(json!({
        "content": [
            { "type": "text", "text": serde_json::to_string_pretty(&payload)? }
        ]
    }))
}

/// Synthesizes a compact, agent-ready summary from a query result — this is the
/// "coarse-grained, agent-like" tool: it does the search-and-condense work inside
/// Brick so the caller gets conclusions, not raw rows.
fn explore_summary(question: &str, query: &crate::metadata::MetadataQueryResponse) -> Value {
    let findings: Vec<Value> = query
        .matches
        .iter()
        .take(5)
        .map(|m| {
            json!({
                "tool": m.source_id,
                "intent": m.intent,
                "when": m.last_seen_at,
                "repo": m.repo_path,
                "branch": m.branch,
                "read_session_hint": m.recall_chunks_hint,
            })
        })
        .collect();
    json!({
        "question": question,
        "summary": query.summary,
        "match_count": query.match_count,
        "top_findings": findings,
        "next_step": "Call read_session with a finding's source + session_id to read its transcript."
    })
}

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
