//! Rebuildable SQLite cache for local provenance queries.
//!
//! The database is derived from the append-only JSONL queue and lives under the
//! effective store cache directory. Callers may delete and rebuild it without
//! losing provenance because JSONL remains authoritative.

use std::path::Path;

use anyhow::{Context, Result};
use brick_protocol::{
    ActorType, ArtifactKind, CausalRelation, DiffFileChangeKind, DiffTarget, EventType,
    EvidenceAvailability, MissionStatus, SessionLogFormat, TraceEvent,
};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::json;

use crate::{
    sqlite_schema::{clear_tables, create_schema, reset_schema},
    FileSessionBlameEvidenceKind, FileSessionBlameRow, IndexedArtifact, IndexedAttachment,
    IndexedDiff, IndexedFile, IndexedMission, IndexedOrg, IndexedProject, IndexedRepoContext,
    IndexedSession, IndexedSessionLog, SqliteFileSessionBlameQuery, TraceIndex,
};

/// Current schema version for the rebuildable SQLite cache.
pub const SQLITE_INDEX_SCHEMA_VERSION: u16 = 2;

/// Filename for the SQLite query cache under the effective cache directory.
pub const SQLITE_INDEX_FILE: &str = "brick.sqlite";

/// Metadata summary for the local SQLite cache.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteIndexStatus {
    pub exists: bool,
    pub path: String,
    pub schema_version: Option<u16>,
    pub event_count: usize,
    pub mission_count: usize,
    pub session_count: usize,
    pub artifact_count: usize,
    pub file_count: usize,
    pub session_log_count: usize,
    pub diff_count: usize,
    pub rebuilt_at: Option<DateTime<Utc>>,
}

/// Filters for read-only SQLite session queries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqliteSessionQuery {
    pub app_id: Option<String>,
    pub actor_id: Option<String>,
    pub runtime_id: Option<String>,
    pub limit: usize,
}

/// Filters for read-only SQLite artifact queries.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SqliteArtifactQuery {
    pub session_id: Option<String>,
    pub mission_id: Option<String>,
    pub limit: usize,
}

/// Session row returned by typed SQLite commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteSessionRecord {
    pub session_id: String,
    pub session_name: Option<String>,
    pub actor_id: Option<String>,
    pub actor_type: Option<String>,
    pub app_id: Option<String>,
    pub app_session_id: Option<String>,
    pub app_session_name: Option<String>,
    pub runtime_id: Option<String>,
    pub mission_ids: Vec<String>,
    pub artifact_ids: Vec<String>,
    pub log_ref_ids: Vec<String>,
    pub started_at: String,
    pub last_event_at: String,
}

/// Artifact row returned by typed SQLite commands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteArtifactRecord {
    pub artifact_id: String,
    pub artifact_kind: Option<String>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub mission_ids: Vec<String>,
    pub session_ids: Vec<String>,
    pub file_paths: Vec<String>,
    pub attachments: Vec<SqliteAttachmentRecord>,
    pub diffs: Vec<SqliteDiffRecord>,
    pub created_at: String,
    pub last_event_at: String,
}

/// Diff row returned with typed SQLite artifact queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteDiffRecord {
    pub diff_id: String,
    pub diff_target: String,
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
    pub patch_id: Option<String>,
    pub summary_hash: String,
    pub file_count: usize,
    pub additions: u64,
    pub deletions: u64,
    pub binary_file_count: usize,
    pub repo_context_id: Option<String>,
    pub captured_at: String,
}

/// Attachment row returned with typed SQLite artifact queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteAttachmentRecord {
    pub attachment_id: String,
    pub name: String,
    pub original_path: String,
    pub content_type: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
    pub storage_uri: String,
    pub external_uri: Option<String>,
    pub availability: String,
    pub repo_context_id: Option<String>,
    pub uploaded_at: String,
}

/// Rebuilds a SQLite cache at `path` from queued events and the derived graph index.
pub fn rebuild_sqlite_index(path: &Path, events: &[TraceEvent], index: &TraceIndex) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(|| {
            format!(
                "failed to create SQLite cache directory {}",
                parent.display()
            )
        })?;
    }

    let mut connection = Connection::open(path)
        .with_context(|| format!("failed to open SQLite index at {}", path.display()))?;
    prepare_schema_for_rebuild(&connection)?;
    let transaction = connection
        .transaction()
        .context("failed to start SQLite index rebuild transaction")?;
    clear_tables(&transaction)?;
    insert_metadata(&transaction, events.len())?;
    insert_events(&transaction, events)?;
    insert_index(&transaction, index)?;
    transaction
        .commit()
        .context("failed to commit SQLite index rebuild")?;
    Ok(())
}

/// Reads SQLite cache status without rebuilding a missing database.
pub fn sqlite_index_status(path: &Path) -> Result<SqliteIndexStatus> {
    if !path.exists() {
        return Ok(SqliteIndexStatus {
            exists: false,
            path: path.display().to_string(),
            schema_version: None,
            event_count: 0,
            mission_count: 0,
            session_count: 0,
            artifact_count: 0,
            file_count: 0,
            session_log_count: 0,
            diff_count: 0,
            rebuilt_at: None,
        });
    }

    let connection = Connection::open(path)
        .with_context(|| format!("failed to open SQLite index at {}", path.display()))?;
    let schema_version = metadata_value(&connection, "schema_version")?
        .map(|value| value.parse::<u16>())
        .transpose()
        .context("failed to parse SQLite schema_version metadata")?;
    let rebuilt_at = metadata_value(&connection, "rebuilt_at")?
        .map(|value| DateTime::parse_from_rfc3339(&value).map(|parsed| parsed.with_timezone(&Utc)))
        .transpose()
        .context("failed to parse SQLite rebuilt_at metadata")?;

    Ok(SqliteIndexStatus {
        exists: true,
        path: path.display().to_string(),
        schema_version,
        event_count: count_rows(&connection, "events")?,
        mission_count: count_rows(&connection, "missions")?,
        session_count: count_rows(&connection, "sessions")?,
        artifact_count: count_rows(&connection, "artifacts")?,
        file_count: count_rows(&connection, "files")?,
        session_log_count: count_rows(&connection, "session_logs")?,
        diff_count: count_rows(&connection, "diffs")?,
        rebuilt_at,
    })
}

/// Runs a typed, read-only session query against the SQLite cache.
pub fn query_sqlite_sessions(
    path: &Path,
    query: &SqliteSessionQuery,
) -> Result<Vec<SqliteSessionRecord>> {
    let connection = readonly_connection(path)?;
    let limit = normalized_limit(query.limit);
    let mut statement = connection.prepare(
        "SELECT session_id, session_name, actor_id, actor_type, app_id, app_session_id, \
         app_session_name, runtime_id, started_at, last_event_at \
         FROM sessions \
         WHERE (?1 IS NULL OR app_id = ?1) \
           AND (?2 IS NULL OR actor_id = ?2) \
           AND (?3 IS NULL OR runtime_id = ?3) \
         ORDER BY last_event_at DESC, session_id ASC \
         LIMIT ?4",
    )?;
    let rows = statement.query_map(
        params![query.app_id, query.actor_id, query.runtime_id, limit],
        |row| {
            Ok(SqliteSessionRecord {
                session_id: row.get(0)?,
                session_name: row.get(1)?,
                actor_id: row.get(2)?,
                actor_type: row.get(3)?,
                app_id: row.get(4)?,
                app_session_id: row.get(5)?,
                app_session_name: row.get(6)?,
                runtime_id: row.get(7)?,
                mission_ids: Vec::new(),
                artifact_ids: Vec::new(),
                log_ref_ids: Vec::new(),
                started_at: row.get(8)?,
                last_event_at: row.get(9)?,
            })
        },
    )?;

    let mut records = Vec::new();
    for row in rows {
        let mut record = row.context("failed to read SQLite session row")?;
        record.mission_ids = collect_values(
            &connection,
            "SELECT mission_id FROM session_missions WHERE session_id = ?1 ORDER BY mission_id",
            &record.session_id,
        )?;
        record.artifact_ids = collect_values(
            &connection,
            "SELECT artifact_id FROM artifact_sessions WHERE session_id = ?1 ORDER BY artifact_id",
            &record.session_id,
        )?;
        record.log_ref_ids = collect_values(
            &connection,
            "SELECT log_ref_id FROM session_logs WHERE session_id = ?1 ORDER BY uploaded_at, log_ref_id",
            &record.session_id,
        )?;
        records.push(record);
    }
    Ok(records)
}

/// Runs a typed, read-only file/session attribution query against runtime provenance.
pub fn query_sqlite_file_session_blame(
    path: &Path,
    query: &SqliteFileSessionBlameQuery,
) -> Result<Vec<FileSessionBlameRow>> {
    let connection = readonly_connection(path)?;
    let limit = normalized_limit(query.limit);
    let mut records = runtime_diff_blame_rows(&connection, &query.file_path, limit)?;
    records.extend(runtime_file_ref_blame_rows(
        &connection,
        &query.file_path,
        limit,
    )?);
    records.sort_by(|left, right| {
        right
            .last_seen_at
            .cmp(&left.last_seen_at)
            .then_with(|| left.session_id.cmp(&right.session_id))
            .then_with(|| {
                left.evidence_kind
                    .as_str()
                    .cmp(right.evidence_kind.as_str())
            })
    });
    records.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
    Ok(records)
}

/// Runs a typed, read-only artifact query against the SQLite cache.
pub fn query_sqlite_artifacts(
    path: &Path,
    query: &SqliteArtifactQuery,
) -> Result<Vec<SqliteArtifactRecord>> {
    let connection = readonly_connection(path)?;
    let limit = normalized_limit(query.limit);
    let mut statement = connection.prepare(
        "SELECT DISTINCT artifacts.artifact_id, artifacts.artifact_kind, artifacts.title, \
         artifacts.body, artifacts.created_at, artifacts.last_event_at \
         FROM artifacts \
         LEFT JOIN artifact_sessions ON artifact_sessions.artifact_id = artifacts.artifact_id \
         LEFT JOIN artifact_missions ON artifact_missions.artifact_id = artifacts.artifact_id \
         WHERE (?1 IS NULL OR artifact_sessions.session_id = ?1) \
           AND (?2 IS NULL OR artifact_missions.mission_id = ?2) \
         ORDER BY artifacts.last_event_at DESC, artifacts.artifact_id ASC \
         LIMIT ?3",
    )?;
    let rows = statement.query_map(params![query.session_id, query.mission_id, limit], |row| {
        Ok(SqliteArtifactRecord {
            artifact_id: row.get(0)?,
            artifact_kind: row.get(1)?,
            title: row.get(2)?,
            body: row.get(3)?,
            mission_ids: Vec::new(),
            session_ids: Vec::new(),
            file_paths: Vec::new(),
            attachments: Vec::new(),
            diffs: Vec::new(),
            created_at: row.get(4)?,
            last_event_at: row.get(5)?,
        })
    })?;

    let mut records = Vec::new();
    for row in rows {
        let mut record = row.context("failed to read SQLite artifact row")?;
        record.mission_ids = collect_values(
            &connection,
            "SELECT mission_id FROM artifact_missions WHERE artifact_id = ?1 ORDER BY mission_id",
            &record.artifact_id,
        )?;
        record.session_ids = collect_values(
            &connection,
            "SELECT session_id FROM artifact_sessions WHERE artifact_id = ?1 ORDER BY session_id",
            &record.artifact_id,
        )?;
        record.file_paths = collect_values(
            &connection,
            "SELECT path FROM artifact_files WHERE artifact_id = ?1 ORDER BY path",
            &record.artifact_id,
        )?;
        record.attachments = collect_attachments(&connection, &record.artifact_id)?;
        record.diffs = collect_diffs(&connection, &record.artifact_id)?;
        records.push(record);
    }
    Ok(records)
}

fn runtime_diff_blame_rows(
    connection: &Connection,
    file_path: &str,
    limit: i64,
) -> Result<Vec<FileSessionBlameRow>> {
    let mut statement = connection.prepare(
        "SELECT diff_files.path, diffs.session_id, sessions.app_id, sessions.actor_id, sessions.actor_type,
                diffs.captured_at, diff_files.additions, diff_files.deletions, diffs.file_count,
                diffs.diff_id, diffs.artifact_id, diffs.mission_id, diffs.diff_target, diffs.base_commit,
                diffs.head_commit, diffs.patch_id, diffs.summary_hash, diff_files.change_kind,
                diff_files.old_path, events.confidence
         FROM diff_files
         JOIN diffs ON diffs.diff_id = diff_files.diff_id
         LEFT JOIN sessions ON sessions.session_id = diffs.session_id
         LEFT JOIN events ON events.event_id = diffs.diff_id
         WHERE diff_files.path = ?1
            OR diff_files.old_path = ?1
            OR diff_files.path LIKE ?2 ESCAPE '\\'
            OR diff_files.old_path LIKE ?2 ESCAPE '\\'
         ORDER BY diffs.captured_at DESC, diffs.diff_id ASC
         LIMIT ?3",
    )?;
    let folder_pattern = folder_like_pattern(file_path);
    let rows = statement.query_map(params![file_path, folder_pattern, limit], |row| {
        let additions: Option<i64> = row.get(6)?;
        let deletions: Option<i64> = row.get(7)?;
        let file_count: i64 = row.get(8)?;
        let path_value: String = row.get(0)?;
        let old_path: Option<String> = row.get(18)?;
        Ok(FileSessionBlameRow {
            file_path: if path_value == file_path
                || path_matches_folder_query(&path_value, file_path)
            {
                path_value
            } else {
                old_path.unwrap_or(path_value)
            },
            session_id: row.get(1)?,
            external_session_id: None,
            source_id: None,
            app_id: row.get(2)?,
            actor_id: row.get(3)?,
            actor_type: row.get(4)?,
            evidence_kind: FileSessionBlameEvidenceKind::RuntimeEvent,
            last_seen_at: row.get(5)?,
            title: None,
            lines_added: additions.and_then(|value| u64::try_from(value).ok()),
            lines_removed: deletions.and_then(|value| u64::try_from(value).ok()),
            files_changed: u64::try_from(file_count).ok(),
            confidence: row.get(19)?,
            source_pointer: Some(json!({
                "diff_id": row.get::<_, String>(9)?,
                "artifact_id": row.get::<_, String>(10)?,
                "mission_id": row.get::<_, Option<String>>(11)?,
                "diff_target": row.get::<_, String>(12)?,
                "base_commit": row.get::<_, Option<String>>(13)?,
                "head_commit": row.get::<_, Option<String>>(14)?,
                "patch_id": row.get::<_, Option<String>>(15)?,
                "summary_hash": row.get::<_, String>(16)?,
                "change_kind": row.get::<_, String>(17)?,
                "old_path": row.get::<_, Option<String>>(18)?,
            })),
        })
    })?;
    collect_blame_rows(rows, "failed to read SQLite runtime diff blame row")
}

fn runtime_file_ref_blame_rows(
    connection: &Connection,
    file_path: &str,
    limit: i64,
) -> Result<Vec<FileSessionBlameRow>> {
    let mut statement = connection.prepare(
        "SELECT file_refs.path, file_refs.session_id, sessions.app_id, sessions.actor_id, sessions.actor_type,
                file_refs.recorded_at, file_refs.file_ref_id, file_refs.artifact_id, file_refs.repo_context_id,
                events.confidence
         FROM file_refs
         LEFT JOIN sessions ON sessions.session_id = file_refs.session_id
         LEFT JOIN events ON events.event_id = file_refs.file_ref_id
         LEFT JOIN diffs generated_diff ON generated_diff.diff_id = file_refs.file_ref_id
         WHERE (file_refs.path = ?1 OR file_refs.path LIKE ?2 ESCAPE '\\')
           AND generated_diff.diff_id IS NULL
         ORDER BY file_refs.recorded_at DESC, file_refs.file_ref_id ASC
         LIMIT ?3",
    )?;
    let folder_pattern = folder_like_pattern(file_path);
    let rows = statement.query_map(params![file_path, folder_pattern, limit], |row| {
        Ok(FileSessionBlameRow {
            file_path: row.get(0)?,
            session_id: row.get(1)?,
            external_session_id: None,
            source_id: None,
            app_id: row.get(2)?,
            actor_id: row.get(3)?,
            actor_type: row.get(4)?,
            evidence_kind: FileSessionBlameEvidenceKind::RuntimeEvent,
            last_seen_at: row.get(5)?,
            title: None,
            lines_added: None,
            lines_removed: None,
            files_changed: Some(1),
            confidence: row.get(9)?,
            source_pointer: Some(json!({
                "file_ref_id": row.get::<_, String>(6)?,
                "artifact_id": row.get::<_, String>(7)?,
                "repo_context_id": row.get::<_, Option<String>>(8)?,
            })),
        })
    })?;
    collect_blame_rows(rows, "failed to read SQLite runtime file-ref blame row")
}

fn collect_blame_rows(
    rows: rusqlite::MappedRows<
        '_,
        impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<FileSessionBlameRow>,
    >,
    context: &str,
) -> Result<Vec<FileSessionBlameRow>> {
    let mut records = Vec::new();
    for row in rows {
        records.push(row.context(context.to_string())?);
    }
    Ok(records)
}

fn folder_like_pattern(path: &str) -> String {
    format!("{}/%", escape_like(path.trim_end_matches('/')))
}

fn escape_like(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

fn path_matches_folder_query(file_path: &str, query: &str) -> bool {
    let query = query.trim_end_matches('/');
    !query.is_empty()
        && file_path != query
        && file_path
            .strip_prefix(query)
            .is_some_and(|tail| tail.starts_with('/'))
}

fn prepare_schema_for_rebuild(connection: &Connection) -> Result<()> {
    if !table_exists(connection, "metadata")? {
        return reset_schema(connection);
    }
    let schema_version = metadata_value(connection, "schema_version")?
        .map(|value| value.parse::<u16>())
        .transpose()
        .context("failed to parse SQLite schema_version metadata")?;
    if schema_version == Some(SQLITE_INDEX_SCHEMA_VERSION) {
        create_schema(connection)
    } else {
        reset_schema(connection)
    }
}

fn table_exists(connection: &Connection, table_name: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            params![table_name],
            |row| row.get::<_, bool>(0),
        )
        .context("failed to inspect SQLite schema")
}

fn insert_metadata(connection: &Connection, event_count: usize) -> Result<()> {
    let rebuilt_at = Utc::now().to_rfc3339();
    for (key, value) in [
        ("schema_version", SQLITE_INDEX_SCHEMA_VERSION.to_string()),
        ("rebuilt_at", rebuilt_at),
        ("event_count", event_count.to_string()),
    ] {
        connection.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
    }
    Ok(())
}

fn insert_events(connection: &Connection, events: &[TraceEvent]) -> Result<()> {
    let mut statement = connection.prepare(
        "INSERT INTO events (event_id, event_type, schema_version, payload_schema_version, \
         occurred_at, recorded_at, actor_id, actor_type, actor_display_name, repo_id, org_id, \
         project_id, mission_id, session_id, artifact_id, repo_context_id, confidence, payload_json) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
    )?;
    for event in events {
        let payload_json = serde_json::to_string(&event.payload)
            .context("failed to serialize event payload for SQLite cache")?;
        statement.execute(params![
            event.event_id.to_string(),
            event_type_name(event.event_type),
            i64::from(event.schema_version),
            i64::from(event.payload_schema_version),
            event.occurred_at.to_rfc3339(),
            event.recorded_at.to_rfc3339(),
            event.actor.actor_id,
            actor_type_name(event.actor.actor_type),
            event.actor.display_name,
            event.repo_id,
            event.org_id.as_ref().map(ToString::to_string),
            event.project_id.as_ref().map(ToString::to_string),
            event.mission_id.as_ref().map(ToString::to_string),
            event.session_id.as_ref().map(ToString::to_string),
            event.artifact_id.as_ref().map(ToString::to_string),
            event.repo_context_id.as_ref().map(ToString::to_string),
            format!("{:?}", event.confidence).to_lowercase(),
            payload_json,
        ])?;
    }
    Ok(())
}

fn insert_index(connection: &Connection, index: &TraceIndex) -> Result<()> {
    for org in index.orgs.values() {
        insert_org(connection, org)?;
    }
    for project in index.projects.values() {
        insert_project(connection, project)?;
    }
    for mission in index.missions.values() {
        insert_mission(connection, mission)?;
    }
    for session in index.sessions.values() {
        insert_session(connection, session)?;
    }
    for artifact in index.artifacts.values() {
        insert_artifact(connection, artifact)?;
    }
    for file in index.files.values() {
        insert_file(connection, file)?;
    }
    for attachment in index.attachments.values() {
        insert_attachment(connection, attachment)?;
    }
    for diff in index.diffs.values() {
        insert_diff(connection, diff)?;
    }
    for session_log in index.session_logs.values() {
        insert_session_log(connection, session_log)?;
    }
    for repo_context in index.repo_contexts.values() {
        insert_repo_context(connection, repo_context)?;
    }
    insert_causal_edges(connection, index)?;
    Ok(())
}

/// Projects the causal adjacency tables into `causal_edges`. The `causes` map is
/// authoritative (it holds the relation/note/confidence for every edge including
/// standalone rationales); `effects` is recoverable from it, so we only persist
/// `causes` and rebuild `effects` on load.
fn insert_causal_edges(connection: &Connection, index: &TraceIndex) -> Result<()> {
    for (effect_event, edges) in &index.causes {
        for edge in edges {
            connection.execute(
                "INSERT OR IGNORE INTO causal_edges \
                 (source_event_id, effect_event, cause_event, relation, note, confidence, recorded_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    edge.source_event_id,
                    effect_event,
                    edge.cause_event,
                    causal_relation_name(edge.relation),
                    edge.note,
                    edge.confidence,
                    edge.recorded_at.to_rfc3339(),
                ],
            )?;
        }
    }
    Ok(())
}

fn insert_org(connection: &Connection, org: &IndexedOrg) -> Result<()> {
    connection.execute(
        "INSERT INTO orgs (org_id, name, description, created_at, last_event_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            org.org_id,
            org.name,
            org.description,
            org.created_at.to_rfc3339(),
            org.last_event_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn insert_project(connection: &Connection, project: &IndexedProject) -> Result<()> {
    connection.execute(
        "INSERT INTO projects (project_id, org_id, name, description, created_at, last_event_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            project.project_id,
            project.org_id,
            project.name,
            project.description,
            project.created_at.to_rfc3339(),
            project.last_event_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn insert_mission(connection: &Connection, mission: &IndexedMission) -> Result<()> {
    connection.execute(
        "INSERT INTO missions (mission_id, project_id, title, description, status, created_at, last_event_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            mission.mission_id,
            mission.project_id,
            mission.title,
            mission.description,
            mission_status_name(mission.status),
            mission.created_at.to_rfc3339(),
            mission.last_event_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn insert_session(connection: &Connection, session: &IndexedSession) -> Result<()> {
    connection.execute(
        "INSERT INTO sessions (session_id, session_name, actor_id, actor_type, app_id, \
         app_session_id, app_session_name, runtime_id, started_at, last_event_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
        params![
            session.session_id,
            session.session_name,
            session.actor_id,
            session.actor_type.map(actor_type_name),
            session.source.app_id,
            session.source.app_session_id,
            session.source.app_session_name,
            session.source.runtime_id,
            session.started_at.to_rfc3339(),
            session.last_event_at.to_rfc3339(),
        ],
    )?;
    for mission_id in &session.mission_ids {
        connection.execute(
            "INSERT INTO session_missions (session_id, mission_id) VALUES (?1, ?2)",
            params![session.session_id, mission_id],
        )?;
    }
    Ok(())
}

fn insert_artifact(connection: &Connection, artifact: &IndexedArtifact) -> Result<()> {
    connection.execute(
        "INSERT INTO artifacts (artifact_id, artifact_kind, title, body, created_at, last_event_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            artifact.artifact_id,
            artifact.artifact_kind.map(artifact_kind_name),
            artifact.title,
            artifact.body,
            artifact.created_at.to_rfc3339(),
            artifact.last_event_at.to_rfc3339(),
        ],
    )?;
    for mission_id in &artifact.mission_ids {
        connection.execute(
            "INSERT INTO artifact_missions (artifact_id, mission_id) VALUES (?1, ?2)",
            params![artifact.artifact_id, mission_id],
        )?;
    }
    for session_id in &artifact.session_ids {
        connection.execute(
            "INSERT INTO artifact_sessions (artifact_id, session_id) VALUES (?1, ?2)",
            params![artifact.artifact_id, session_id],
        )?;
    }
    for path in &artifact.file_paths {
        connection.execute(
            "INSERT OR IGNORE INTO artifact_files (artifact_id, path) VALUES (?1, ?2)",
            params![artifact.artifact_id, path],
        )?;
    }
    for attachment_id in &artifact.attachment_ids {
        connection.execute(
            "INSERT OR IGNORE INTO artifact_attachments (artifact_id, attachment_id) VALUES (?1, ?2)",
            params![artifact.artifact_id, attachment_id],
        )?;
    }
    Ok(())
}

fn insert_file(connection: &Connection, file: &IndexedFile) -> Result<()> {
    connection.execute("INSERT INTO files (path) VALUES (?1)", params![file.path])?;
    for file_ref in &file.file_refs {
        connection.execute(
            "INSERT INTO file_refs (file_ref_id, artifact_id, session_id, repo_context_id, path, recorded_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                file_ref.file_ref_id,
                file_ref.artifact_id,
                file_ref.session_id,
                file_ref.repo_context_id,
                file.path,
                file_ref.recorded_at.to_rfc3339(),
            ],
        )?;
        connection.execute(
            "INSERT OR IGNORE INTO artifact_files (artifact_id, path) VALUES (?1, ?2)",
            params![file_ref.artifact_id, file.path],
        )?;
    }
    Ok(())
}

fn insert_attachment(connection: &Connection, attachment: &IndexedAttachment) -> Result<()> {
    connection.execute(
        "INSERT INTO attachments (attachment_id, name, original_path, content_type, sha256, \
         size_bytes, storage_uri, external_uri, availability, repo_context_id, uploaded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            attachment.attachment_id,
            attachment.name,
            attachment.original_path,
            attachment.content_type,
            attachment.sha256,
            i64::try_from(attachment.size_bytes)
                .context("attachment size cannot fit in SQLite integer")?,
            attachment.storage_uri,
            attachment.external_uri,
            evidence_availability_name(attachment.availability),
            attachment.repo_context_id,
            attachment.uploaded_at.to_rfc3339(),
        ],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO artifact_attachments (artifact_id, attachment_id) VALUES (?1, ?2)",
        params![attachment.artifact_id, attachment.attachment_id],
    )?;
    Ok(())
}

fn insert_diff(connection: &Connection, diff: &IndexedDiff) -> Result<()> {
    connection.execute(
        "INSERT INTO diffs (diff_id, artifact_id, session_id, mission_id, diff_target, base_commit, \
         head_commit, patch_id, summary_hash, file_count, additions, deletions, binary_file_count, \
         repo_context_id, captured_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)",
        params![
            diff.diff_id,
            diff.artifact_id,
            diff.session_id,
            diff.mission_id,
            diff_target_name(diff.diff_target),
            diff.base_commit,
            diff.head_commit,
            diff.patch_id,
            diff.summary_hash,
            i64::try_from(diff.file_count).context("diff file count cannot fit in SQLite integer")?,
            i64::try_from(diff.additions).context("diff additions cannot fit in SQLite integer")?,
            i64::try_from(diff.deletions).context("diff deletions cannot fit in SQLite integer")?,
            i64::try_from(diff.binary_file_count)
                .context("diff binary file count cannot fit in SQLite integer")?,
            diff.repo_context_id,
            diff.captured_at.to_rfc3339(),
        ],
    )?;
    for change in &diff.file_changes {
        connection.execute(
            "INSERT INTO diff_files (diff_id, path, old_path, change_kind, additions, deletions) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                diff.diff_id,
                change.path,
                change.old_path,
                diff_file_change_kind_name(change.change_kind),
                change
                    .additions
                    .map(|value| i64::try_from(value).unwrap_or(i64::MAX)),
                change
                    .deletions
                    .map(|value| i64::try_from(value).unwrap_or(i64::MAX)),
            ],
        )?;
    }
    Ok(())
}

fn insert_session_log(connection: &Connection, session_log: &IndexedSessionLog) -> Result<()> {
    connection.execute(
        "INSERT INTO session_logs (log_ref_id, session_id, original_path, format, source, sha256, \
         size_bytes, storage_uri, local_path, external_uri, availability, repo_context_id, uploaded_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
        params![
            session_log.log_ref_id,
            session_log.session_id,
            session_log.original_path,
            session_log_format_name(session_log.format),
            session_log.source,
            session_log.sha256,
            i64::try_from(session_log.size_bytes)
                .context("session log size cannot fit in SQLite integer")?,
            session_log.storage_uri,
            session_log.local_path,
            session_log.external_uri,
            evidence_availability_name(session_log.availability),
            session_log.repo_context_id,
            session_log.uploaded_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn insert_repo_context(connection: &Connection, repo_context: &IndexedRepoContext) -> Result<()> {
    connection.execute(
        "INSERT INTO repo_contexts (repo_context_id, branch, head_commit, dirty, captured_at) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            repo_context.repo_context_id,
            repo_context.branch,
            repo_context.head_commit,
            repo_context.dirty,
            repo_context.captured_at.to_rfc3339(),
        ],
    )?;
    Ok(())
}

fn readonly_connection(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY).with_context(
        || {
            format!(
                "failed to open SQLite index read-only at {}",
                path.display()
            )
        },
    )
}

fn metadata_value(connection: &Connection, key: &str) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .context("failed to read SQLite metadata")
}

fn count_rows(connection: &Connection, table: &str) -> Result<usize> {
    let count = connection.query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
        row.get::<_, i64>(0)
    })?;
    usize::try_from(count).context("SQLite row count cannot fit in usize")
}

fn collect_attachments(
    connection: &Connection,
    artifact_id: &str,
) -> Result<Vec<SqliteAttachmentRecord>> {
    let mut statement = connection.prepare(
        "SELECT attachments.attachment_id, attachments.name, attachments.original_path, \
         attachments.content_type, attachments.sha256, attachments.size_bytes, \
         attachments.storage_uri, attachments.external_uri, attachments.availability, \
         attachments.repo_context_id, attachments.uploaded_at \
         FROM attachments \
         JOIN artifact_attachments ON artifact_attachments.attachment_id = attachments.attachment_id \
         WHERE artifact_attachments.artifact_id = ?1 \
         ORDER BY attachments.uploaded_at, attachments.attachment_id",
    )?;
    let rows = statement.query_map(params![artifact_id], |row| {
        let size_bytes: i64 = row.get(5)?;
        Ok(SqliteAttachmentRecord {
            attachment_id: row.get(0)?,
            name: row.get(1)?,
            original_path: row.get(2)?,
            content_type: row.get(3)?,
            sha256: row.get(4)?,
            size_bytes: u64::try_from(size_bytes).unwrap_or_default(),
            storage_uri: row.get(6)?,
            external_uri: row.get(7)?,
            availability: row.get(8)?,
            repo_context_id: row.get(9)?,
            uploaded_at: row.get(10)?,
        })
    })?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row.context("failed to read SQLite attachment row")?);
    }
    Ok(records)
}

fn collect_diffs(connection: &Connection, artifact_id: &str) -> Result<Vec<SqliteDiffRecord>> {
    let mut statement = connection.prepare(
        "SELECT diff_id, diff_target, base_commit, head_commit, patch_id, summary_hash, file_count, \
         additions, deletions, binary_file_count, repo_context_id, captured_at \
         FROM diffs WHERE artifact_id = ?1 ORDER BY captured_at, diff_id",
    )?;
    let rows = statement.query_map(params![artifact_id], |row| {
        let file_count: i64 = row.get(6)?;
        let additions: i64 = row.get(7)?;
        let deletions: i64 = row.get(8)?;
        let binary_file_count: i64 = row.get(9)?;
        Ok(SqliteDiffRecord {
            diff_id: row.get(0)?,
            diff_target: row.get(1)?,
            base_commit: row.get(2)?,
            head_commit: row.get(3)?,
            patch_id: row.get(4)?,
            summary_hash: row.get(5)?,
            file_count: usize::try_from(file_count).unwrap_or_default(),
            additions: u64::try_from(additions).unwrap_or_default(),
            deletions: u64::try_from(deletions).unwrap_or_default(),
            binary_file_count: usize::try_from(binary_file_count).unwrap_or_default(),
            repo_context_id: row.get(10)?,
            captured_at: row.get(11)?,
        })
    })?;
    let mut records = Vec::new();
    for row in rows {
        records.push(row.context("failed to read SQLite diff row")?);
    }
    Ok(records)
}

fn collect_values(connection: &Connection, sql: &str, id: &str) -> Result<Vec<String>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(params![id], |row| row.get::<_, String>(0))?;
    let mut values = Vec::new();
    for row in rows {
        values.push(row.context("failed to read SQLite related id")?);
    }
    Ok(values)
}

fn normalized_limit(limit: usize) -> i64 {
    i64::try_from(limit.max(1)).unwrap_or(i64::MAX)
}

fn actor_type_name(actor_type: ActorType) -> &'static str {
    match actor_type {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}

fn mission_status_name(status: MissionStatus) -> &'static str {
    match status {
        MissionStatus::Planned => "planned",
        MissionStatus::Active => "active",
        MissionStatus::Blocked => "blocked",
        MissionStatus::Completed => "completed",
        MissionStatus::Archived => "archived",
    }
}

fn artifact_kind_name(kind: ArtifactKind) -> &'static str {
    match kind {
        ArtifactKind::Decision => "decision",
        ArtifactKind::FileRef => "file_ref",
        ArtifactKind::Patch => "patch",
        ArtifactKind::Review => "review",
        ArtifactKind::TestResult => "test_result",
        ArtifactKind::Acceptance => "acceptance",
        ArtifactKind::Note => "note",
    }
}

fn diff_target_name(target: DiffTarget) -> &'static str {
    match target {
        DiffTarget::Working => "working",
        DiffTarget::Staged => "staged",
        DiffTarget::Range => "range",
    }
}

fn diff_file_change_kind_name(kind: DiffFileChangeKind) -> &'static str {
    match kind {
        DiffFileChangeKind::Added => "added",
        DiffFileChangeKind::Modified => "modified",
        DiffFileChangeKind::Deleted => "deleted",
        DiffFileChangeKind::Renamed => "renamed",
        DiffFileChangeKind::Copied => "copied",
        DiffFileChangeKind::TypeChanged => "type_changed",
        DiffFileChangeKind::Unknown => "unknown",
    }
}

fn session_log_format_name(format: SessionLogFormat) -> &'static str {
    match format {
        SessionLogFormat::Text => "text",
        SessionLogFormat::Jsonl => "jsonl",
        SessionLogFormat::Markdown => "markdown",
        SessionLogFormat::Unknown => "unknown",
    }
}

fn evidence_availability_name(availability: EvidenceAvailability) -> &'static str {
    match availability {
        EvidenceAvailability::LocalPointer => "local_pointer",
        EvidenceAvailability::LocalBlob => "local_blob",
        EvidenceAvailability::RemoteBlob => "remote_blob",
    }
}

fn event_type_name(event_type: EventType) -> &'static str {
    match event_type {
        EventType::OrgCreated => "org.created",
        EventType::OrgUpdated => "org.updated",
        EventType::ProjectCreated => "project.created",
        EventType::ProjectUpdated => "project.updated",
        EventType::MissionCreated => "mission.created",
        EventType::MissionUpdated => "mission.updated",
        EventType::SessionStarted => "session.started",
        EventType::SessionLinkedToMission => "session.linked_to_mission",
        EventType::SessionLogUploaded => "session.log_uploaded",
        EventType::ArtifactCreated => "artifact.created",
        EventType::ArtifactUpdated => "artifact.updated",
        EventType::ArtifactLinkedToMission => "artifact.linked_to_mission",
        EventType::ArtifactFileRefRecorded => "artifact.file_ref_recorded",
        EventType::ArtifactAttachmentUploaded => "artifact.attachment_uploaded",
        EventType::ArtifactReviewed => "artifact.reviewed",
        EventType::ArtifactAccepted => "artifact.accepted",
        EventType::RepoContextCaptured => "repo_context.captured",
        EventType::DiffCaptured => "diff.captured",
        EventType::ExternalRefLinked => "external_ref.linked",
        EventType::SourceSessionObserved => "source.session_observed",
        EventType::CausalLinked => "causal.linked",
    }
}

fn causal_relation_name(relation: CausalRelation) -> &'static str {
    match relation {
        CausalRelation::TriggeredBy => "triggered_by",
        CausalRelation::DerivedFrom => "derived_from",
        CausalRelation::Supersedes => "supersedes",
        CausalRelation::RespondsTo => "responds_to",
        CausalRelation::Rationale => "rationale",
    }
}
