//! Unified metadata database skeleton for global Brick state.
//!
//! The database lives at `<BRICK_HOME>/metadata.sqlite` and is independent from
//! the repo-local JSONL provenance queue. Version mismatches reset the first-stage
//! schema because these tables are metadata/cache scaffolding, not the durable
//! provenance source of truth.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::Value;

use crate::{metadata_db_path, metadata_db_path_in_home};

/// Current schema version for the unified metadata database.
pub const METADATA_DB_SCHEMA_VERSION: u16 = 1;

const METADATA_KEY_SCHEMA_VERSION: &str = "schema_version";
const METADATA_KEY_RESET_AT: &str = "reset_at";
const METADATA_KEY_INITIALIZED_AT: &str = "initialized_at";

/// Open metadata DB handle.
#[derive(Debug)]
pub struct MetadataDb {
    connection: Connection,
    path: PathBuf,
}

/// Input for creating or updating a source-session row.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSessionUpsert {
    pub source_id: String,
    pub external_session_id: String,
    pub title: Option<String>,
    pub name: Option<String>,
    pub source_path: Option<PathBuf>,
    pub source_uri: Option<String>,
    pub source_mtime: Option<DateTime<Utc>>,
    pub source_size: Option<u64>,
    pub source_fingerprint: Option<String>,
    pub parser_version: Option<String>,
    pub discovered_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub metadata_json: Option<Value>,
}

/// Typed source-session row returned by metadata DB queries.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceSessionRecord {
    pub source_id: String,
    pub external_session_id: String,
    pub title: Option<String>,
    pub name: Option<String>,
    pub source_path: Option<PathBuf>,
    pub source_uri: Option<String>,
    pub source_mtime: Option<DateTime<Utc>>,
    pub source_size: Option<u64>,
    pub source_fingerprint: Option<String>,
    pub parser_version: Option<String>,
    pub discovered_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata_json: Option<Value>,
}

/// Optional filters for listing source-session rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourceSessionListQuery {
    pub source_id: Option<String>,
    pub limit: usize,
}

impl MetadataDb {
    /// Opens and initializes the global metadata DB under resolved `BRICK_HOME`.
    pub fn open_global() -> Result<Self> {
        Self::open_path(metadata_db_path()?)
    }

    /// Opens and initializes the metadata DB under an explicit Brick home.
    pub fn open_in_home(brick_home: impl AsRef<Path>) -> Result<Self> {
        Self::open_path(metadata_db_path_in_home(brick_home))
    }

    /// Opens and initializes a metadata DB at an explicit path.
    pub fn open_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create metadata DB directory {}",
                    parent.display()
                )
            })?;
        }
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open metadata DB at {}", path.display()))?;
        prepare_schema(&connection)?;
        Ok(Self { connection, path })
    }

    /// Returns the filesystem path backing this database.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Returns the active metadata schema version.
    pub fn schema_version(&self) -> Result<u16> {
        metadata_value(&self.connection, METADATA_KEY_SCHEMA_VERSION)?
            .context("metadata DB schema version is missing")?
            .parse::<u16>()
            .context("failed to parse metadata DB schema version")
    }

    /// Inserts or updates one source-session row keyed by source and external ID.
    pub fn upsert_source_session(
        &mut self,
        session: &SourceSessionUpsert,
    ) -> Result<SourceSessionRecord> {
        let transaction = self
            .connection
            .transaction()
            .context("failed to start metadata source-session upsert")?;
        let now = Utc::now();
        let metadata_json = serialize_metadata_json(session.metadata_json.as_ref())?;
        transaction.execute(
            "INSERT INTO source_sessions (
                source_id, external_session_id, title, name, source_path, source_uri,
                source_mtime, source_size, source_fingerprint, parser_version,
                discovered_at, last_seen_at, created_at, updated_at, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15)
             ON CONFLICT(source_id, external_session_id) DO UPDATE SET
                title = excluded.title,
                name = excluded.name,
                source_path = excluded.source_path,
                source_uri = excluded.source_uri,
                source_mtime = excluded.source_mtime,
                source_size = excluded.source_size,
                source_fingerprint = excluded.source_fingerprint,
                parser_version = excluded.parser_version,
                discovered_at = excluded.discovered_at,
                last_seen_at = excluded.last_seen_at,
                updated_at = excluded.updated_at,
                metadata_json = excluded.metadata_json",
            params![
                session.source_id,
                session.external_session_id,
                session.title,
                session.name,
                session
                    .source_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                session.source_uri,
                session.source_mtime.map(|value| value.to_rfc3339()),
                optional_u64_to_i64(session.source_size)?,
                session.source_fingerprint,
                session.parser_version,
                session.discovered_at.to_rfc3339(),
                session.last_seen_at.to_rfc3339(),
                now.to_rfc3339(),
                now.to_rfc3339(),
                metadata_json,
            ],
        )?;
        let record = read_source_session(
            &transaction,
            &session.source_id,
            &session.external_session_id,
        )?;
        transaction
            .commit()
            .context("failed to commit metadata source-session upsert")?;
        record.context("metadata source-session row missing after upsert")
    }

    /// Lists source-session rows in deterministic most-recent-first order.
    pub fn list_source_sessions(
        &self,
        query: &SourceSessionListQuery,
    ) -> Result<Vec<SourceSessionRecord>> {
        let limit = normalized_limit(query.limit);
        let mut statement = self.connection.prepare(
            "SELECT source_id, external_session_id, title, name, source_path, source_uri,
                    source_mtime, source_size, source_fingerprint, parser_version,
                    discovered_at, last_seen_at, created_at, updated_at, metadata_json
             FROM source_sessions
             WHERE (?1 IS NULL OR source_id = ?1)
             ORDER BY last_seen_at DESC, source_id ASC, external_session_id ASC
             LIMIT ?2",
        )?;
        let rows = statement.query_map(params![query.source_id, limit], source_session_from_row)?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.context("failed to read metadata source-session row")?);
        }
        Ok(records)
    }
}

fn prepare_schema(connection: &Connection) -> Result<()> {
    if !table_exists(connection, "metadata")? {
        reset_schema(connection)
    } else if metadata_value(connection, METADATA_KEY_SCHEMA_VERSION)?
        .map(|value| value.parse::<u16>())
        .transpose()
        .context("failed to parse metadata DB schema version")?
        == Some(METADATA_DB_SCHEMA_VERSION)
    {
        create_schema(connection)
    } else {
        reset_schema(connection)
    }
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS metadata (
             key TEXT PRIMARY KEY,
             value TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS source_profiles (
             source_id TEXT PRIMARY KEY,
             name TEXT,
             app_id TEXT,
             actor_id TEXT,
             actor_type TEXT,
             profile_json TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS source_roots (
             source_root_id INTEGER PRIMARY KEY AUTOINCREMENT,
             source_id TEXT NOT NULL,
             root_path TEXT,
             root_uri TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             UNIQUE(source_id, root_path, root_uri)
         );
         CREATE TABLE IF NOT EXISTS source_scans (
             source_scan_id INTEGER PRIMARY KEY AUTOINCREMENT,
             source_id TEXT NOT NULL,
             source_root_id INTEGER,
             started_at TEXT NOT NULL,
             finished_at TEXT,
             status TEXT NOT NULL,
             metadata_json TEXT
         );
         CREATE TABLE IF NOT EXISTS source_sessions (
             source_session_id INTEGER PRIMARY KEY AUTOINCREMENT,
             source_id TEXT NOT NULL,
             external_session_id TEXT NOT NULL,
             title TEXT,
             name TEXT,
             source_path TEXT,
             source_uri TEXT,
             source_mtime TEXT,
             source_size INTEGER,
             source_fingerprint TEXT,
             parser_version TEXT,
             discovered_at TEXT NOT NULL,
             last_seen_at TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             metadata_json TEXT,
             UNIQUE(source_id, external_session_id)
         );
         CREATE TABLE IF NOT EXISTS source_session_resources (
             resource_id INTEGER PRIMARY KEY AUTOINCREMENT,
             source_session_id INTEGER NOT NULL,
             resource_kind TEXT NOT NULL,
             resource_path TEXT,
             resource_uri TEXT,
             metadata_json TEXT,
             FOREIGN KEY(source_session_id) REFERENCES source_sessions(source_session_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS workspace_roots (
             workspace_root_id INTEGER PRIMARY KEY AUTOINCREMENT,
             root_path TEXT NOT NULL UNIQUE,
             root_uri TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS git_repositories (
             git_repository_id INTEGER PRIMARY KEY AUTOINCREMENT,
             repo_path TEXT,
             repo_uri TEXT,
             remote_url TEXT,
             head_commit TEXT,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             UNIQUE(repo_path, repo_uri)
         );
         CREATE TABLE IF NOT EXISTS source_session_workspace_roots (
             source_session_id INTEGER NOT NULL,
             workspace_root_id INTEGER NOT NULL,
             PRIMARY KEY (source_session_id, workspace_root_id),
             FOREIGN KEY(source_session_id) REFERENCES source_sessions(source_session_id) ON DELETE CASCADE,
             FOREIGN KEY(workspace_root_id) REFERENCES workspace_roots(workspace_root_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS source_session_git_repositories (
             source_session_id INTEGER NOT NULL,
             git_repository_id INTEGER NOT NULL,
             PRIMARY KEY (source_session_id, git_repository_id),
             FOREIGN KEY(source_session_id) REFERENCES source_sessions(source_session_id) ON DELETE CASCADE,
             FOREIGN KEY(git_repository_id) REFERENCES git_repositories(git_repository_id) ON DELETE CASCADE
         );
         CREATE TABLE IF NOT EXISTS brick_session_source_sessions (
             brick_session_id TEXT NOT NULL,
             source_session_id INTEGER NOT NULL,
             linked_at TEXT NOT NULL,
             PRIMARY KEY (brick_session_id, source_session_id),
             FOREIGN KEY(source_session_id) REFERENCES source_sessions(source_session_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS idx_source_sessions_source ON source_sessions(source_id, last_seen_at);
         CREATE INDEX IF NOT EXISTS idx_source_sessions_path ON source_sessions(source_path);
         CREATE INDEX IF NOT EXISTS idx_source_sessions_fingerprint ON source_sessions(source_fingerprint);",
    )?;
    upsert_metadata(
        connection,
        METADATA_KEY_SCHEMA_VERSION,
        &METADATA_DB_SCHEMA_VERSION.to_string(),
    )?;
    upsert_metadata(
        connection,
        METADATA_KEY_INITIALIZED_AT,
        &Utc::now().to_rfc3339(),
    )?;
    Ok(())
}

fn reset_schema(connection: &Connection) -> Result<()> {
    for table in existing_user_tables(connection)? {
        connection.execute(
            &format!("DROP TABLE IF EXISTS {}", quote_identifier(&table)),
            [],
        )?;
    }
    create_schema(connection)?;
    upsert_metadata(connection, METADATA_KEY_RESET_AT, &Utc::now().to_rfc3339())?;
    Ok(())
}

fn existing_user_tables(connection: &Connection) -> Result<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT name FROM sqlite_master
         WHERE type = 'table' AND name NOT LIKE 'sqlite_%'
         ORDER BY name DESC",
    )?;
    let rows = statement.query_map([], |row| row.get(0))?;
    let mut tables = Vec::new();
    for row in rows {
        tables.push(row.context("failed to read metadata DB table name")?);
    }
    Ok(tables)
}

fn quote_identifier(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn table_exists(connection: &Connection, table_name: &str) -> Result<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
            params![table_name],
            |row| row.get::<_, bool>(0),
        )
        .context("failed to inspect metadata DB schema")
}

fn metadata_value(connection: &Connection, key: &str) -> Result<Option<String>> {
    connection
        .query_row(
            "SELECT value FROM metadata WHERE key = ?1",
            params![key],
            |row| row.get(0),
        )
        .optional()
        .with_context(|| format!("failed to read metadata DB key {key}"))
}

fn upsert_metadata(connection: &Connection, key: &str, value: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO metadata (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )?;
    Ok(())
}

fn read_source_session(
    connection: &Connection,
    source_id: &str,
    external_session_id: &str,
) -> Result<Option<SourceSessionRecord>> {
    connection
        .query_row(
            "SELECT source_id, external_session_id, title, name, source_path, source_uri,
                    source_mtime, source_size, source_fingerprint, parser_version,
                    discovered_at, last_seen_at, created_at, updated_at, metadata_json
             FROM source_sessions
             WHERE source_id = ?1 AND external_session_id = ?2",
            params![source_id, external_session_id],
            source_session_from_row,
        )
        .optional()
        .context("failed to read metadata source-session row")
}

fn source_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceSessionRecord> {
    let source_path: Option<String> = row.get(4)?;
    let source_mtime: Option<String> = row.get(6)?;
    let source_size: Option<i64> = row.get(7)?;
    let discovered_at: String = row.get(10)?;
    let last_seen_at: String = row.get(11)?;
    let created_at: String = row.get(12)?;
    let updated_at: String = row.get(13)?;
    let metadata_json: Option<String> = row.get(14)?;
    Ok(SourceSessionRecord {
        source_id: row.get(0)?,
        external_session_id: row.get(1)?,
        title: row.get(2)?,
        name: row.get(3)?,
        source_path: source_path.map(PathBuf::from),
        source_uri: row.get(5)?,
        source_mtime: parse_optional_datetime(source_mtime)?,
        source_size: optional_i64_to_u64(source_size)?,
        source_fingerprint: row.get(8)?,
        parser_version: row.get(9)?,
        discovered_at: parse_datetime(discovered_at)?,
        last_seen_at: parse_datetime(last_seen_at)?,
        created_at: parse_datetime(created_at)?,
        updated_at: parse_datetime(updated_at)?,
        metadata_json: parse_metadata_json(metadata_json)?,
    })
}

fn parse_datetime(value: String) -> rusqlite::Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(&value)
        .map(|parsed| parsed.with_timezone(&Utc))
        .map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(err))
        })
}

fn parse_optional_datetime(value: Option<String>) -> rusqlite::Result<Option<DateTime<Utc>>> {
    value.map(parse_datetime).transpose()
}

fn serialize_metadata_json(value: Option<&Value>) -> Result<Option<String>> {
    value
        .map(serde_json::to_string)
        .transpose()
        .context("failed to serialize source-session metadata JSON")
}

fn parse_metadata_json(value: Option<String>) -> rusqlite::Result<Option<Value>> {
    value
        .map(|json| {
            serde_json::from_str(&json).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Text,
                    Box::new(err),
                )
            })
        })
        .transpose()
}

fn optional_u64_to_i64(value: Option<u64>) -> Result<Option<i64>> {
    value
        .map(|number| i64::try_from(number).context("source_size exceeds SQLite INTEGER range"))
        .transpose()
}

fn optional_i64_to_u64(value: Option<i64>) -> rusqlite::Result<Option<u64>> {
    value
        .map(|number| {
            u64::try_from(number).map_err(|err| {
                rusqlite::Error::FromSqlConversionFailure(
                    0,
                    rusqlite::types::Type::Integer,
                    Box::new(err),
                )
            })
        })
        .transpose()
}

fn normalized_limit(limit: usize) -> i64 {
    if limit == 0 {
        100
    } else {
        i64::try_from(limit).unwrap_or(i64::MAX)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;

    const TEST_SOURCE_ID: &str = "test-source";
    const TEST_EXTERNAL_SESSION_ID: &str = "external-1";
    const TEST_PARSER_VERSION: &str = "parser-v1";
    const TEST_SOURCE_URI: &str = "file:///tmp/session.jsonl";
    const TEST_SOURCE_FINGERPRINT: &str = "sha256:test";

    fn temp_home(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-metadata-db-{name}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create temp metadata home");
        path
    }

    fn sample_upsert(title: &str, last_seen_offset: i64) -> SourceSessionUpsert {
        let discovered_at = Utc
            .with_ymd_and_hms(2026, 6, 18, 1, 2, 3)
            .single()
            .expect("valid discovered_at");
        let last_seen_at = Utc
            .with_ymd_and_hms(2026, 6, 18, 1, 2, 3 + last_seen_offset as u32)
            .single()
            .expect("valid last_seen_at");
        SourceSessionUpsert {
            source_id: TEST_SOURCE_ID.to_string(),
            external_session_id: TEST_EXTERNAL_SESSION_ID.to_string(),
            title: Some(title.to_string()),
            name: Some(title.to_string()),
            source_path: Some(PathBuf::from("/tmp/session.jsonl")),
            source_uri: Some(TEST_SOURCE_URI.to_string()),
            source_mtime: Some(discovered_at),
            source_size: Some(42),
            source_fingerprint: Some(TEST_SOURCE_FINGERPRINT.to_string()),
            parser_version: Some(TEST_PARSER_VERSION.to_string()),
            discovered_at,
            last_seen_at,
            metadata_json: Some(json!({ "phase": "first-slice" })),
        }
    }

    #[test]
    fn opens_db_under_explicit_brick_home() {
        let home = temp_home("home-path");
        let db = MetadataDb::open_in_home(&home).expect("open metadata DB");

        assert_eq!(db.path(), home.join(crate::METADATA_DB_FILE));
        assert_eq!(
            db.schema_version().expect("schema version"),
            METADATA_DB_SCHEMA_VERSION
        );
        assert!(db.path().exists());
    }

    #[test]
    fn upserts_and_lists_source_sessions() {
        let path = temp_home("upsert-list").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");

        let inserted = db
            .upsert_source_session(&sample_upsert("Original title", 0))
            .expect("insert source session");
        assert_eq!(inserted.title.as_deref(), Some("Original title"));
        assert_eq!(inserted.source_size, Some(42));
        assert_eq!(
            inserted.metadata_json,
            Some(json!({ "phase": "first-slice" }))
        );

        let mut updated_input = sample_upsert("Updated title", 1);
        updated_input.source_size = Some(84);
        let updated = db
            .upsert_source_session(&updated_input)
            .expect("update source session");
        assert_eq!(updated.title.as_deref(), Some("Updated title"));
        assert_eq!(updated.source_size, Some(84));
        assert_eq!(updated.created_at, inserted.created_at);
        assert!(updated.updated_at >= inserted.updated_at);

        let sessions = db
            .list_source_sessions(&SourceSessionListQuery {
                source_id: Some(TEST_SOURCE_ID.to_string()),
                limit: 10,
            })
            .expect("list source sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].external_session_id, TEST_EXTERNAL_SESSION_ID);
        assert_eq!(
            sessions[0].parser_version.as_deref(),
            Some(TEST_PARSER_VERSION)
        );
    }

    #[test]
    fn resets_unknown_schema_version() {
        let path = temp_home("reset").join(crate::METADATA_DB_FILE);
        let connection = Connection::open(&path).expect("open raw metadata DB");
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata (key, value) VALUES ('schema_version', '999');
                 CREATE TABLE obsolete_table (value TEXT);",
            )
            .expect("seed obsolete metadata DB");
        drop(connection);

        let db = MetadataDb::open_path(&path).expect("open reset metadata DB");
        assert_eq!(
            db.schema_version().expect("schema version"),
            METADATA_DB_SCHEMA_VERSION
        );
        let obsolete_exists: bool = db
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'obsolete_table')",
                [],
                |row| row.get(0),
            )
            .expect("inspect obsolete table");
        assert!(!obsolete_exists);
    }
}
