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

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{anyhow, Context, Result};
use chrono::DateTime;
use rusqlite::{Connection, OpenFlags};
use serde_json::Value;

use crate::{ActivityChunk, NativeSourceSession, SourceProfile};

use super::jsonl::truncate_title;
use super::shell_edits::shell_edit_targets;

const ORGII_SOURCE_ID: &str = "orgii";
const ORGII_SQLITE_PARSER_VERSION: &str = "orgii-sqlite-v2";
const ORGII_DB_FILE: &str = "sessions.db";
const DEFAULT_LIMIT: usize = 50;

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    let db_path = orgii_db_path(profile)?;
    let connection = open_orgii_db(&db_path)?;
    let scan_limit = limit.unwrap_or(DEFAULT_LIMIT);
    let app_id = profile
        .app_id
        .clone()
        .unwrap_or_else(|| ORGII_SOURCE_ID.to_string());

    let mut statement = connection
        .prepare(
            "SELECT session_id, name, model, project_path, code_repo_path, workspace_path,
                    worktree_branch, user_input, created_at, updated_at
             FROM agent_sessions
             ORDER BY updated_at DESC
             LIMIT ?1",
        )
        .context("failed to prepare ORGII agent_sessions query")?;
    let rows = statement
        .query_map([scan_limit as i64], |row| {
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
        })
        .context("failed to query ORGII agent_sessions")?;

    let db_path_string = db_path.display().to_string();
    let mut sessions = Vec::new();
    for row in rows {
        let row = row?;
        let impact = session_edit_impact(&connection, &row.session_id)?;
        sessions.push(session_from_row(&row, &db_path_string, &app_id, impact));
    }
    Ok(sessions)
}

pub(super) fn format_chunks(
    _external_session_id: &str,
    _source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    // Chunk-level transcript rendering is not implemented for ORGII yet; the
    // metadata path (touched_files for blame) is what unblocks recall. Returning
    // an empty chunk list keeps callers working without claiming false content.
    Ok(Vec::new())
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
        .map(|value| truncate_title(value.to_string()));
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

/// Aggregates the set of files a session's events touched, from structured edit
/// tools (`edit_file`/`write_file`/`apply_patch`) and shell write commands.
fn session_edit_impact(connection: &Connection, session_id: &str) -> Result<EditImpact> {
    let mut impact = EditImpact::default();
    let mut statement = connection
        .prepare(
            "SELECT function_name, args_json FROM events
             WHERE session_id = ?1
               AND function_name IN
                   ('edit_file','write_file','apply_patch','edit_file_by_replace',
                    'run_shell','run_command_line')",
        )
        .context("failed to prepare ORGII events query")?;
    let rows = statement
        .query_map([session_id], |row| {
            Ok((
                row.get::<_, Option<String>>(0)?.unwrap_or_default(),
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
            ))
        })
        .context("failed to query ORGII events")?;
    for row in rows {
        let (function_name, args_json) = row?;
        let args: Value = serde_json::from_str(&args_json).unwrap_or(Value::Null);
        match function_name.as_str() {
            "edit_file" | "write_file" | "edit_file_by_replace" => {
                if let Some(path) = args
                    .get("file_path")
                    .or_else(|| args.get("path"))
                    .or_else(|| args.get("target_file"))
                    .and_then(Value::as_str)
                    .filter(|path| !path.is_empty())
                {
                    impact.touched_files.insert(path.to_string());
                }
            }
            "apply_patch" => {
                let patch = args
                    .get("patch")
                    .or_else(|| args.get("input"))
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                for file in shell_edit_targets(patch) {
                    impact.touched_files.insert(file);
                }
            }
            "run_shell" | "run_command_line" => {
                if let Some(command) = args
                    .get("command")
                    .or_else(|| args.get("cmd"))
                    .and_then(Value::as_str)
                {
                    for file in shell_edit_targets(command) {
                        impact.touched_files.insert(file);
                    }
                }
            }
            _ => {}
        }
    }
    Ok(impact)
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

        let sessions = list_sessions(&profile(path), Some(10)).expect("list orgii sessions");

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
}
