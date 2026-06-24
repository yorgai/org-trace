//! ORGII native history source provider.
//!
//! ORGII persists agent sessions in `<home>/.orgii/sessions.db`, a SQLite store
//! with an `agent_sessions` metadata table and an `events` table carrying one
//! row per tool call / message. File edits show up as `events.function_name`
//! values (`edit_file`, `write_file`, `apply_patch`) with the target path in
//! `args_json`, or as shell writes inside `run_shell` / `run_command_line`
//! commands. This provider reads that store read-only and projects each session
//! into the shared `NativeSourceSession` shape, populating `touched_files` so
//! `file-session-blame` works against ORGII history.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::{
    assistant_message_chunk, tool_call_chunk, user_message_chunk, ActivityChunk, ImportedToolCall,
    NativeSourceSession, SourceProfile,
};

use super::jsonl::normalize_title;
use super::shell_edits::shell_edit_targets;

const ORGII_SOURCE_ID: &str = "orgii";
const ORGII_SQLITE_PARSER_VERSION: &str = "orgii-sqlite-v2";
const ORGII_PROVIDER_SLUG: &str = "orgii";
const ORGII_DB_FILE: &str = "sessions.db";

/// Lists ORGII sessions, newest first. When `since` is set (an RFC3339 string),
/// only sessions whose `updated_at >= since` are returned — the incremental path
/// that keeps refresh cheap after the first full scan. Touched-file impact is
/// computed for the whole returned batch in ONE aggregate query (no per-session
/// N+1 scan over the multi-GB `events` table).
pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<&str>,
) -> Result<Vec<NativeSourceSession>> {
    let db_path = orgii_db_path(profile)?;
    let connection = open_orgii_db(&db_path)?;
    let app_id = profile
        .app_id
        .clone()
        .unwrap_or_else(|| ORGII_SOURCE_ID.to_string());

    // `>=` (not `>`) on the watermark so a session sharing the boundary
    // `updated_at` is never silently skipped; per-session fingerprint skip in the
    // refresh layer dedupes the at-most-one re-scan that costs.
    let sql = if since.is_some() {
        "SELECT session_id, name, model, project_path, code_repo_path, workspace_path,
                worktree_branch, user_input, created_at, updated_at
         FROM agent_sessions
         WHERE updated_at >= ?1
         ORDER BY updated_at DESC"
    } else {
        "SELECT session_id, name, model, project_path, code_repo_path, workspace_path,
                worktree_branch, user_input, created_at, updated_at
         FROM agent_sessions
         ORDER BY updated_at DESC"
    };
    let mut statement = connection
        .prepare(sql)
        .context("failed to prepare ORGII agent_sessions query")?;
    let map_row = |row: &rusqlite::Row| {
        Ok(OrgiiSessionRow {
            session_id: row.get(0)?,
            name: row.get::<_, Option<String>>(1)?,
            model: row.get::<_, Option<String>>(2)?,
            project_path: row.get::<_, Option<String>>(3)?,
            code_repo_path: row.get::<_, Option<String>>(4)?,
            workspace_path: row.get::<_, Option<String>>(5)?,
            branch: row.get::<_, Option<String>>(6)?,
            user_input: row.get::<_, Option<String>>(7)?,
            created_at: row.get::<_, Option<String>>(8)?,
            updated_at: row.get::<_, Option<String>>(9)?,
        })
    };
    let mut session_rows: Vec<OrgiiSessionRow> = if let Some(since) = since {
        statement
            .query_map(rusqlite::params![since], map_row)
            .context("failed to query ORGII agent_sessions")?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        statement
            .query_map([], map_row)
            .context("failed to query ORGII agent_sessions")?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    if let Some(limit) = limit {
        session_rows.truncate(limit);
    }

    let session_ids: Vec<&str> = session_rows.iter().map(|r| r.session_id.as_str()).collect();
    let mut impacts = batch_edit_impact(&connection, &session_ids)?;

    let db_path_string = db_path.display().to_string();
    let sessions = session_rows
        .iter()
        .map(|row| {
            let impact = impacts.remove(&row.session_id).unwrap_or_default();
            session_from_row(row, &db_path_string, &app_id, impact)
        })
        .collect();
    Ok(sessions)
}

struct OrgiiSessionRow {
    session_id: String,
    name: Option<String>,
    model: Option<String>,
    project_path: Option<String>,
    code_repo_path: Option<String>,
    workspace_path: Option<String>,
    branch: Option<String>,
    user_input: Option<String>,
    created_at: Option<String>,
    updated_at: Option<String>,
}

#[derive(Default)]
struct EditImpact {
    touched_files: BTreeSet<String>,
}

fn session_from_row(
    row: &OrgiiSessionRow,
    db_path: &str,
    app_id: &str,
    impact: EditImpact,
) -> NativeSourceSession {
    let repo_path = row
        .code_repo_path
        .clone()
        .or_else(|| row.project_path.clone())
        .or_else(|| row.workspace_path.clone());
    let title = row
        .name
        .as_deref()
        .filter(|name| !name.is_empty())
        .or(row.user_input.as_deref())
        .map(|value| normalize_title(value.to_string()));
    let touched_files: Vec<String> = impact.touched_files.into_iter().collect();
    let files_changed = (!touched_files.is_empty()).then_some(touched_files.len() as u64);

    NativeSourceSession {
        external_session_id: row.session_id.clone(),
        source_app_id: app_id.to_string(),
        title,
        path: PathBuf::from(db_path),
        size_bytes: 0,
        modified_at: parse_time(row.updated_at.as_deref()),
        session_created_at: parse_time(row.created_at.as_deref()),
        session_updated_at: parse_time(row.updated_at.as_deref()),
        model: row.model.clone(),
        input_tokens: None,
        output_tokens: None,
        repo_path: repo_path.clone().map(PathBuf::from),
        branch: row.branch.clone(),
        files_changed,
        lines_added: None,
        lines_removed: None,
        touched_files,
        parser_version: ORGII_SQLITE_PARSER_VERSION.to_string(),
        listable: true,
        metadata_json: None,
        cwd: repo_path.map(PathBuf::from),
        liveness: crate::Liveness::Unknown,
        last_activity: None,
    }
}

/// Accumulates one event's edit impact into `touched_files`, shared by the batch
/// and single-session paths.
fn accumulate_edit(function_name: &str, args_json: &str, touched_files: &mut BTreeSet<String>) {
    let args: Value = serde_json::from_str(args_json).unwrap_or(Value::Null);
    match function_name {
        "edit_file" | "write_file" | "edit_file_by_replace" => {
            if let Some(path) = args
                .get("file_path")
                .or_else(|| args.get("path"))
                .or_else(|| args.get("target_file"))
                .and_then(Value::as_str)
                .filter(|path| !path.is_empty())
            {
                touched_files.insert(path.to_string());
            }
        }
        "apply_patch" => {
            let patch = args
                .get("patch")
                .or_else(|| args.get("input"))
                .and_then(Value::as_str)
                .unwrap_or_default();
            for file in shell_edit_targets(patch) {
                touched_files.insert(file);
            }
        }
        "run_shell" | "run_command_line" => {
            if let Some(command) = args
                .get("command")
                .or_else(|| args.get("cmd"))
                .and_then(Value::as_str)
            {
                for file in shell_edit_targets(command) {
                    touched_files.insert(file);
                }
            }
        }
        _ => {}
    }
}

/// Computes touched-file impact for a whole batch of sessions in ONE query,
/// grouping by `session_id` in Rust. Replaces the previous per-session N+1 scan
/// that ran one `events` query per session over the multi-GB table. Returns an
/// empty map for an empty batch (no query issued).
fn batch_edit_impact(
    connection: &Connection,
    session_ids: &[&str],
) -> Result<HashMap<String, EditImpact>> {
    let mut impacts: HashMap<String, EditImpact> = HashMap::new();
    if session_ids.is_empty() {
        return Ok(impacts);
    }
    // Bind each id as a parameter (`IN (?,?,…)`) — avoids string interpolation
    // and keeps SQLite's prepared-statement plan reusable.
    let placeholders = std::iter::repeat_n("?", session_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT session_id, function_name, args_json FROM events
         WHERE function_name IN
               ('edit_file','write_file','apply_patch','edit_file_by_replace',
                'run_shell','run_command_line')
           AND session_id IN ({placeholders})"
    );
    let mut statement = connection
        .prepare(&sql)
        .context("failed to prepare ORGII batch events query")?;
    let params = rusqlite::params_from_iter(session_ids.iter());
    let rows = statement
        .query_map(params, |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
            ))
        })
        .context("failed to query ORGII batch events")?;
    for row in rows {
        let (session_id, function_name, args_json) = row?;
        let impact = impacts.entry(session_id).or_default();
        accumulate_edit(&function_name, &args_json, &mut impact.touched_files);
    }
    Ok(impacts)
}

/// Renders an ORGII session's transcript into shared activity chunks so the CTP
/// layer can recover an `observed` rationale (the turn-final assistant message).
/// ORGII stores no `user` event rows — the user's prompt lives only in
/// `agent_sessions.user_input` — so we inject that single prompt as the opening
/// user chunk, then stream `assistant` / `tool_call` events in time order.
pub(super) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    let path = source_path
        .ok_or_else(|| anyhow!("ORGII source path missing for session: {external_session_id}"))?;
    let connection = open_orgii_db(path)?;

    let mut chunks = Vec::new();
    let mut sequence = 0_usize;

    // Opening user prompt from agent_sessions.user_input (ORGII has no user events).
    let user_input: Option<(Option<String>, Option<String>)> = connection
        .query_row(
            "SELECT user_input, created_at FROM agent_sessions WHERE session_id = ?1",
            [external_session_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .ok();
    if let Some((Some(message), created_at)) = user_input {
        if !message.trim().is_empty() {
            chunks.push(user_message_chunk(
                external_session_id,
                ORGII_PROVIDER_SLUG,
                sequence,
                created_at.as_deref().unwrap_or_default(),
                message.trim(),
            ));
            sequence += 1;
        }
    }

    let mut statement = connection
        .prepare(
            "SELECT event_type, function_name, args_json, content, created_at
             FROM events
             WHERE session_id = ?1
               AND event_type IN ('assistant','tool_call')
             ORDER BY created_at, history_sequence",
        )
        .context("failed to prepare ORGII transcript query")?;
    let rows = statement
        .query_map([external_session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, Option<String>>(4)?.unwrap_or_default(),
            ))
        })
        .context("failed to query ORGII transcript")?;
    for row in rows {
        let (event_type, function_name, args_json, content, created_at) = row?;
        match event_type.as_str() {
            "assistant" => {
                let message = strip_assistant_prefix(&content);
                if message.is_empty() {
                    continue;
                }
                chunks.push(assistant_message_chunk(
                    external_session_id,
                    ORGII_PROVIDER_SLUG,
                    sequence,
                    &created_at,
                    message,
                ));
                sequence += 1;
            }
            "tool_call" => {
                let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);
                let call = ImportedToolCall {
                    call_id: format!("{external_session_id}-{sequence}"),
                    raw_name: function_name.clone(),
                    canonical_name: function_name.clone(),
                    args,
                    created_at: created_at.clone(),
                };
                chunks.push(tool_call_chunk(
                    external_session_id,
                    ORGII_PROVIDER_SLUG,
                    sequence,
                    &call,
                    "",
                ));
                sequence += 1;
            }
            _ => {}
        }
    }
    Ok(chunks)
}

/// ORGII assistant `content` rows are stored with a literal `assistant ` prefix;
/// strip it so the recovered rationale reads naturally.
fn strip_assistant_prefix(content: &str) -> &str {
    content.strip_prefix("assistant ").unwrap_or(content).trim()
}

fn parse_time(value: Option<&str>) -> Option<SystemTime> {
    let value = value?;
    DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| SystemTime::from(dt.with_timezone(&chrono::Utc)))
}

fn orgii_db_path(profile: &SourceProfile) -> Result<PathBuf> {
    if let Some(path) = &profile.session_db_path {
        return Ok(path.clone());
    }
    if let Some(root) = &profile.evidence_root {
        let candidate = root.join(ORGII_DB_FILE);
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "orgii source requires session_db_path or an evidence_root containing sessions.db"
    ))
}

fn open_orgii_db(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open ORGII DB at {}", path.display()))
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::*;
    use brick_protocol::ActorType;

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "brick-orgii-{name}-{}.db",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn profile(db_path: PathBuf) -> SourceProfile {
        SourceProfile {
            name: ORGII_SOURCE_ID.to_string(),
            app_id: Some(ORGII_SOURCE_ID.to_string()),
            actor_id: None,
            actor_type: Some(ActorType::Agent),
            store_root: None,
            session_db_path: Some(db_path),
            session_log_path: None,
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        }
    }

    fn seed_db(path: &PathBuf) {
        let connection = Connection::open(path).expect("open orgii test db");
        connection
            .execute_batch(
                "CREATE TABLE agent_sessions (
                    session_id TEXT PRIMARY KEY,
                    name TEXT,
                    model TEXT,
                    project_path TEXT,
                    code_repo_path TEXT,
                    workspace_path TEXT,
                    worktree_branch TEXT,
                    user_input TEXT,
                    created_at TEXT,
                    updated_at TEXT
                 );
                 CREATE TABLE events (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    event_type TEXT NOT NULL DEFAULT '',
                    function_name TEXT,
                    args_json TEXT NOT NULL DEFAULT '{}'
                 );",
            )
            .expect("create orgii schema");
        connection
            .execute(
                "INSERT INTO agent_sessions
                    (session_id, name, model, code_repo_path, worktree_branch, user_input, created_at, updated_at)
                 VALUES ('s1', 'Fix the bug', 'claude-opus', '/repo', 'main', 'fix it',
                         '2026-06-18T01:00:00+00:00', '2026-06-18T02:00:00+00:00')",
                [],
            )
            .expect("insert session");
        let events = [
            (
                "edit_file",
                r#"{"file_path":"/repo/src/lib.rs","old_string":"a","new_string":"b"}"#,
            ),
            (
                "write_file",
                r#"{"file_path":"/repo/README.md","content":"x"}"#,
            ),
            ("run_shell", r#"{"command":"echo done > /repo/notes.txt"}"#),
            ("read_file", r#"{"file_path":"/repo/ignored.rs"}"#),
        ];
        for (index, (function_name, args)) in events.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO events (id, session_id, function_name, args_json)
                     VALUES (?1, 's1', ?2, ?3)",
                    (format!("e{index}"), function_name, args),
                )
                .expect("insert event");
        }
    }

    #[test]
    fn lists_orgii_sessions_with_touched_files() {
        let path = temp_db("list");
        seed_db(&path);

        let sessions = list_sessions(&profile(path), Some(10), None).expect("list orgii sessions");

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.external_session_id, "s1");
        assert_eq!(session.title.as_deref(), Some("Fix the bug"));
        assert_eq!(session.model.as_deref(), Some("claude-opus"));
        assert_eq!(session.repo_path.as_deref(), Some(Path::new("/repo")));
        assert_eq!(session.branch.as_deref(), Some("main"));
        // edit_file + write_file + run_shell redirect contribute; read_file does not.
        assert_eq!(
            session.touched_files,
            vec![
                "/repo/README.md".to_string(),
                "/repo/notes.txt".to_string(),
                "/repo/src/lib.rs".to_string(),
            ]
        );
        assert_eq!(session.files_changed, Some(3));
        assert_eq!(session.parser_version, ORGII_SQLITE_PARSER_VERSION);
    }

    /// Seeds a db with two sessions at different `updated_at` plus an assistant +
    /// tool_call transcript on `s1`, for the incremental + chunk-rendering tests.
    fn seed_transcript_db(path: &PathBuf) {
        let connection = Connection::open(path).expect("open orgii test db");
        connection
            .execute_batch(
                "CREATE TABLE agent_sessions (
                    session_id TEXT PRIMARY KEY,
                    name TEXT, model TEXT, project_path TEXT, code_repo_path TEXT,
                    workspace_path TEXT, worktree_branch TEXT, user_input TEXT,
                    created_at TEXT, updated_at TEXT
                 );
                 CREATE TABLE events (
                    id TEXT PRIMARY KEY,
                    session_id TEXT NOT NULL,
                    event_type TEXT NOT NULL DEFAULT '',
                    function_name TEXT,
                    args_json TEXT NOT NULL DEFAULT '{}',
                    content TEXT NOT NULL DEFAULT '',
                    created_at TEXT,
                    history_sequence INTEGER
                 );",
            )
            .expect("create schema");
        connection
            .execute(
                "INSERT INTO agent_sessions
                    (session_id, name, code_repo_path, user_input, created_at, updated_at)
                 VALUES ('s1', 'Old session', '/repo', 'do the old thing',
                         '2026-06-18T01:00:00+00:00', '2026-06-18T02:00:00+00:00')",
                [],
            )
            .expect("insert s1");
        connection
            .execute(
                "INSERT INTO agent_sessions
                    (session_id, name, code_repo_path, user_input, created_at, updated_at)
                 VALUES ('s2', 'New session', '/repo', 'do the new thing',
                         '2026-06-20T01:00:00+00:00', '2026-06-20T02:00:00+00:00')",
                [],
            )
            .expect("insert s2");
        // s1 transcript: user_input (injected) + assistant + tool_call.
        connection
            .execute(
                "INSERT INTO events (id, session_id, event_type, function_name, content, created_at, history_sequence)
                 VALUES ('a1', 's1', 'assistant', NULL, 'assistant I fixed the bug by serializing the refresh.', '2026-06-18T01:30:00+00:00', 1)",
                [],
            )
            .expect("insert assistant");
        connection
            .execute(
                "INSERT INTO events (id, session_id, event_type, function_name, args_json, content, created_at, history_sequence)
                 VALUES ('t1', 's1', 'tool_call', 'edit_file', '{\"file_path\":\"/repo/src/lib.rs\"}', '', '2026-06-18T01:20:00+00:00', 2)",
                [],
            )
            .expect("insert tool_call");
    }

    #[test]
    fn since_filters_to_newer_sessions_only() {
        let path = temp_db("since");
        seed_transcript_db(&path);
        // Full scan sees both.
        let all = list_sessions(&profile(path.clone()), Some(10), None).expect("full scan");
        assert_eq!(all.len(), 2);
        // Incremental from after s1's updated_at sees only s2.
        let incr = list_sessions(&profile(path), Some(10), Some("2026-06-19T00:00:00+00:00"))
            .expect("incremental scan");
        assert_eq!(incr.len(), 1);
        assert_eq!(incr[0].external_session_id, "s2");
    }

    #[test]
    fn format_chunks_renders_user_assistant_tool() {
        let path = temp_db("chunks");
        seed_transcript_db(&path);
        let chunks = format_chunks("s1", Some(&path)).expect("render chunks");
        // user_input + assistant + tool_call = 3 chunks.
        assert_eq!(chunks.len(), 3);
        // The turn-final assistant message must be recoverable, prefix stripped.
        let final_msg = crate::select_turn_final_message(&chunks, "2026-06-18T02:00:00+00:00");
        assert_eq!(
            final_msg.as_deref(),
            Some("I fixed the bug by serializing the refresh.")
        );
    }
}
