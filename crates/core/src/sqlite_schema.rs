//! SQLite schema creation and reset helpers for the local query cache.
//!
//! This module owns SQL DDL so `sqlite_index` can focus on projecting typed
//! events and graph records into the rebuildable database.

use anyhow::Result;
use rusqlite::Connection;

pub(crate) fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
         CREATE TABLE IF NOT EXISTS events (
             event_id TEXT PRIMARY KEY,
             event_type TEXT NOT NULL,
             schema_version INTEGER NOT NULL,
             payload_schema_version INTEGER NOT NULL,
             occurred_at TEXT NOT NULL,
             recorded_at TEXT NOT NULL,
             actor_id TEXT NOT NULL,
             actor_type TEXT NOT NULL,
             actor_display_name TEXT,
             repo_id TEXT,
             org_id TEXT,
             project_id TEXT,
             mission_id TEXT,
             session_id TEXT,
             artifact_id TEXT,
             repo_context_id TEXT,
             confidence TEXT NOT NULL,
             payload_json TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS orgs (
             org_id TEXT PRIMARY KEY,
             name TEXT,
             description TEXT,
             created_at TEXT NOT NULL,
             last_event_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS projects (
             project_id TEXT PRIMARY KEY,
             org_id TEXT,
             name TEXT,
             description TEXT,
             created_at TEXT NOT NULL,
             last_event_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS missions (
             mission_id TEXT PRIMARY KEY,
             project_id TEXT,
             title TEXT,
             description TEXT,
             status TEXT NOT NULL,
             created_at TEXT NOT NULL,
             last_event_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS sessions (
             session_id TEXT PRIMARY KEY,
             session_name TEXT,
             actor_id TEXT,
             actor_type TEXT,
             app_id TEXT,
             app_session_id TEXT,
             app_session_name TEXT,
             runtime_id TEXT,
             started_at TEXT NOT NULL,
             last_event_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS session_missions (
             session_id TEXT NOT NULL,
             mission_id TEXT NOT NULL,
             PRIMARY KEY (session_id, mission_id)
         );
         CREATE TABLE IF NOT EXISTS artifacts (
             artifact_id TEXT PRIMARY KEY,
             artifact_kind TEXT,
             title TEXT,
             body TEXT,
             created_at TEXT NOT NULL,
             last_event_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS artifact_missions (
             artifact_id TEXT NOT NULL,
             mission_id TEXT NOT NULL,
             PRIMARY KEY (artifact_id, mission_id)
         );
         CREATE TABLE IF NOT EXISTS artifact_sessions (
             artifact_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             PRIMARY KEY (artifact_id, session_id)
         );
         CREATE TABLE IF NOT EXISTS files (path TEXT PRIMARY KEY);
         CREATE TABLE IF NOT EXISTS artifact_files (
             artifact_id TEXT NOT NULL,
             path TEXT NOT NULL,
             PRIMARY KEY (artifact_id, path)
         );
         CREATE TABLE IF NOT EXISTS attachments (
             attachment_id TEXT PRIMARY KEY,
             name TEXT NOT NULL,
             original_path TEXT NOT NULL,
             content_type TEXT,
             sha256 TEXT NOT NULL,
             size_bytes INTEGER NOT NULL,
             storage_uri TEXT NOT NULL,
             external_uri TEXT,
             availability TEXT NOT NULL,
             repo_context_id TEXT,
             uploaded_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS diffs (
             diff_id TEXT PRIMARY KEY,
             artifact_id TEXT NOT NULL,
             session_id TEXT,
             mission_id TEXT,
             diff_target TEXT NOT NULL,
             base_commit TEXT,
             head_commit TEXT,
             patch_id TEXT,
             summary_hash TEXT NOT NULL,
             file_count INTEGER NOT NULL,
             additions INTEGER NOT NULL,
             deletions INTEGER NOT NULL,
             binary_file_count INTEGER NOT NULL,
             repo_context_id TEXT,
             captured_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS diff_files (
             diff_id TEXT NOT NULL,
             path TEXT NOT NULL,
             old_path TEXT,
             change_kind TEXT NOT NULL,
             additions INTEGER,
             deletions INTEGER,
             PRIMARY KEY (diff_id, path, old_path)
         );
         CREATE TABLE IF NOT EXISTS session_logs (
             log_ref_id TEXT PRIMARY KEY,
             session_id TEXT NOT NULL,
             original_path TEXT NOT NULL,
             format TEXT NOT NULL,
             source TEXT NOT NULL,
             sha256 TEXT NOT NULL,
             size_bytes INTEGER NOT NULL,
             storage_uri TEXT NOT NULL,
             local_path TEXT NOT NULL,
             external_uri TEXT,
             availability TEXT NOT NULL,
             repo_context_id TEXT,
             uploaded_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS artifact_attachments (
             artifact_id TEXT NOT NULL,
             attachment_id TEXT NOT NULL,
             PRIMARY KEY (artifact_id, attachment_id)
         );
         CREATE TABLE IF NOT EXISTS file_refs (
             file_ref_id TEXT PRIMARY KEY,
             artifact_id TEXT NOT NULL,
             session_id TEXT,
             repo_context_id TEXT,
             path TEXT NOT NULL,
             recorded_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS repo_contexts (
             repo_context_id TEXT PRIMARY KEY,
             branch TEXT,
             head_commit TEXT,
             dirty INTEGER NOT NULL,
             captured_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS causal_edges (
             source_event_id TEXT NOT NULL,
             effect_event TEXT NOT NULL,
             cause_event TEXT,
             relation TEXT NOT NULL,
             note TEXT,
             confidence TEXT NOT NULL,
             recorded_at TEXT NOT NULL,
             PRIMARY KEY (source_event_id, effect_event, cause_event)
         );
         CREATE INDEX IF NOT EXISTS idx_sessions_source ON sessions(app_id, actor_id, runtime_id, last_event_at);
         CREATE INDEX IF NOT EXISTS idx_session_logs_session ON session_logs(session_id, uploaded_at);
         CREATE INDEX IF NOT EXISTS idx_artifact_sessions_session ON artifact_sessions(session_id);
         CREATE INDEX IF NOT EXISTS idx_artifact_missions_mission ON artifact_missions(mission_id);
         CREATE INDEX IF NOT EXISTS idx_attachments_sha256 ON attachments(sha256);
         CREATE INDEX IF NOT EXISTS idx_causal_edges_effect ON causal_edges(effect_event);
         CREATE INDEX IF NOT EXISTS idx_causal_edges_cause ON causal_edges(cause_event);",
    )?;
    Ok(())
}

pub(crate) fn reset_schema(connection: &Connection) -> Result<()> {
    for table in [
        "metadata",
        "events",
        "orgs",
        "projects",
        "missions",
        "sessions",
        "session_missions",
        "artifacts",
        "artifact_missions",
        "artifact_sessions",
        "files",
        "artifact_files",
        "attachments",
        "diffs",
        "diff_files",
        "session_logs",
        "artifact_attachments",
        "file_refs",
        "repo_contexts",
        "causal_edges",
    ] {
        connection.execute(&format!("DROP TABLE IF EXISTS {table}"), [])?;
    }
    create_schema(connection)
}

pub(crate) fn clear_tables(connection: &Connection) -> Result<()> {
    for table in [
        "metadata",
        "events",
        "orgs",
        "projects",
        "missions",
        "sessions",
        "session_missions",
        "artifacts",
        "artifact_missions",
        "artifact_sessions",
        "files",
        "artifact_files",
        "attachments",
        "diffs",
        "diff_files",
        "session_logs",
        "artifact_attachments",
        "file_refs",
        "repo_contexts",
        "causal_edges",
    ] {
        connection.execute(&format!("DELETE FROM {table}"), [])?;
    }
    Ok(())
}
