//! Unified metadata database skeleton for global Brick state.
//!
//! The database lives at `<BRICK_HOME>/metadata.sqlite` and is independent from
//! the repo-local JSONL provenance queue. Version mismatches reset the first-stage
//! schema because these tables are source metadata index scaffolding, not the durable
//! provenance source of truth.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::{
    metadata_db_path, metadata_db_path_in_home, FileSessionBlameEvidenceKind, FileSessionBlameRow,
    SourceFileSessionBlameQuery, SourcePlanListQuery, SourcePlanRecord,
    SourcePlanSessionEdgeRecord, SourcePlanWithEdgesUpsert,
};

/// Current schema version for the unified metadata database.
pub const METADATA_DB_SCHEMA_VERSION: u16 = 5;

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
    pub session_created_at: Option<DateTime<Utc>>,
    pub session_updated_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub repo_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub touched_files_json: Option<Value>,
    pub listable: bool,
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
    pub session_created_at: Option<DateTime<Utc>>,
    pub session_updated_at: Option<DateTime<Utc>>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub repo_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub touched_files_json: Option<Value>,
    pub listable: bool,
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
    pub offset: usize,
}

/// Input for inserting or updating a source-profile row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProfileUpsert {
    pub source_id: String,
    pub name: Option<String>,
    pub app_id: Option<String>,
    pub actor_id: Option<String>,
    pub actor_type: Option<String>,
    pub profile_json: Option<Value>,
}

/// Typed source-profile row returned by metadata DB queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceProfileRecord {
    pub source_id: String,
    pub name: Option<String>,
    pub app_id: Option<String>,
    pub actor_id: Option<String>,
    pub actor_type: Option<String>,
    pub profile_json: Option<Value>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Lifecycle status for a source scan row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceScanStatus {
    Running,
    Completed,
    Error,
}

impl SourceScanStatus {
    /// Returns the database string form for this status.
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceScanStatus::Running => "running",
            SourceScanStatus::Completed => "completed",
            SourceScanStatus::Error => "error",
        }
    }
}

/// Typed source-scan row returned by metadata DB queries.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceScanRecord {
    pub source_scan_id: i64,
    pub source_id: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: Option<DateTime<Utc>>,
    pub status: String,
    pub metadata_json: Option<Value>,
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
        let touched_files_json = serialize_metadata_json(session.touched_files_json.as_ref())?;
        let metadata_json = serialize_metadata_json(session.metadata_json.as_ref())?;
        transaction.execute(
            "INSERT INTO source_sessions (
                source_id, external_session_id, title, name, source_path, source_uri,
                source_mtime, source_size, source_fingerprint, parser_version,
                session_created_at, session_updated_at, model, input_tokens, output_tokens, repo_path, branch,
                files_changed, lines_added, lines_removed, touched_files_json, listable,
                discovered_at, last_seen_at, created_at, updated_at, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27)
             ON CONFLICT(source_id, external_session_id) DO UPDATE SET
                title = excluded.title,
                name = excluded.name,
                source_path = excluded.source_path,
                source_uri = excluded.source_uri,
                source_mtime = excluded.source_mtime,
                source_size = excluded.source_size,
                source_fingerprint = excluded.source_fingerprint,
                parser_version = excluded.parser_version,
                session_created_at = excluded.session_created_at,
                session_updated_at = excluded.session_updated_at,
                model = excluded.model,
                input_tokens = excluded.input_tokens,
                output_tokens = excluded.output_tokens,
                repo_path = excluded.repo_path,
                branch = excluded.branch,
                files_changed = excluded.files_changed,
                lines_added = excluded.lines_added,
                lines_removed = excluded.lines_removed,
                touched_files_json = excluded.touched_files_json,
                listable = excluded.listable,
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
                session.session_created_at.map(|value| value.to_rfc3339()),
                session.session_updated_at.map(|value| value.to_rfc3339()),
                session.model,
                optional_u64_to_i64(session.input_tokens)?,
                optional_u64_to_i64(session.output_tokens)?,
                session
                    .repo_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                session.branch,
                optional_u64_to_i64(session.files_changed)?,
                optional_u64_to_i64(session.lines_added)?,
                optional_u64_to_i64(session.lines_removed)?,
                touched_files_json,
                session.listable,
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
        let offset = normalized_offset(query.offset);
        let mut statement = self.connection.prepare(
            "SELECT source_id, external_session_id, title, name, source_path, source_uri,
                    source_mtime, source_size, source_fingerprint, parser_version,
                    session_created_at, session_updated_at, model, input_tokens, output_tokens, repo_path, branch,
                    files_changed, lines_added, lines_removed, touched_files_json, listable,
                    discovered_at, last_seen_at, created_at, updated_at, metadata_json
             FROM source_sessions
             WHERE (?1 IS NULL OR source_id = ?1)
               AND listable = 1
             ORDER BY last_seen_at DESC, source_id ASC, external_session_id ASC
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(
            params![query.source_id, limit, offset],
            source_session_from_row,
        )?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.context("failed to read metadata source-session row")?);
        }
        Ok(records)
    }

    /// Queries source metadata rows that touched a file path.
    pub fn query_source_file_session_blame(
        &self,
        query: &SourceFileSessionBlameQuery,
    ) -> Result<Vec<FileSessionBlameRow>> {
        let limit = normalized_limit(query.limit);
        let mut statement = self.connection.prepare(
            "SELECT source_id, external_session_id, title, name, source_path, source_uri,
                    source_mtime, source_size, source_fingerprint, parser_version,
                    session_created_at, session_updated_at, model, input_tokens, output_tokens, repo_path, branch,
                    files_changed, lines_added, lines_removed, touched_files_json, listable,
                    discovered_at, last_seen_at, created_at, updated_at, metadata_json
             FROM source_sessions
             WHERE (?1 IS NULL OR source_id = ?1)
               AND (?2 IS NULL OR repo_path = ?2)
               AND touched_files_json IS NOT NULL
             ORDER BY last_seen_at DESC, source_id ASC, external_session_id ASC
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                query.source_id,
                query
                    .repo_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                limit,
            ],
            source_session_from_row,
        )?;
        let mut records = Vec::new();
        for row in rows {
            let record = row.context("failed to read metadata source-session blame row")?;
            if touched_files_from_value(record.touched_files_json.as_ref())
                .iter()
                .any(|path| path == &query.file_path)
            {
                records.push(source_session_blame_row(&query.file_path, record));
            }
        }
        Ok(records)
    }

    /// Counts source-session rows matching an optional source filter.
    pub fn count_source_sessions(&self, source_id: Option<&str>) -> Result<usize> {
        let count = self.connection.query_row(
            "SELECT COUNT(*) FROM source_sessions WHERE (?1 IS NULL OR source_id = ?1) AND listable = 1",
            params![source_id],
            |row| row.get::<_, i64>(0),
        )?;
        usize::try_from(count).context("metadata source-session count exceeds usize")
    }

    /// Reads one source-session row by source and external session ID.
    pub fn get_source_session(
        &self,
        source_id: &str,
        external_session_id: &str,
    ) -> Result<Option<SourceSessionRecord>> {
        read_source_session(&self.connection, source_id, external_session_id)
    }

    /// Updates only the last-seen/updated timestamps for an existing source session.
    pub fn touch_source_session_last_seen(
        &mut self,
        source_id: &str,
        external_session_id: &str,
        last_seen_at: DateTime<Utc>,
    ) -> Result<bool> {
        let now = Utc::now();
        let affected = self.connection.execute(
            "UPDATE source_sessions
             SET last_seen_at = ?3, updated_at = ?4
             WHERE source_id = ?1 AND external_session_id = ?2",
            params![
                source_id,
                external_session_id,
                last_seen_at.to_rfc3339(),
                now.to_rfc3339(),
            ],
        )?;
        Ok(affected > 0)
    }

    /// Inserts or updates one source-profile row keyed by source ID.
    pub fn upsert_source_profile(
        &mut self,
        profile: &SourceProfileUpsert,
    ) -> Result<SourceProfileRecord> {
        let now = Utc::now();
        let profile_json = serialize_metadata_json(profile.profile_json.as_ref())?;
        self.connection.execute(
            "INSERT INTO source_profiles (
                source_id, name, app_id, actor_id, actor_type, profile_json,
                created_at, updated_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(source_id) DO UPDATE SET
                name = excluded.name,
                app_id = excluded.app_id,
                actor_id = excluded.actor_id,
                actor_type = excluded.actor_type,
                profile_json = excluded.profile_json,
                updated_at = excluded.updated_at",
            params![
                profile.source_id,
                profile.name,
                profile.app_id,
                profile.actor_id,
                profile.actor_type,
                profile_json,
                now.to_rfc3339(),
                now.to_rfc3339(),
            ],
        )?;
        read_source_profile(&self.connection, &profile.source_id)?
            .context("metadata source-profile row missing after upsert")
    }

    /// Reads one source-profile row by source ID.
    pub fn get_source_profile(&self, source_id: &str) -> Result<Option<SourceProfileRecord>> {
        read_source_profile(&self.connection, source_id)
    }

    /// Inserts a running source-scan row and returns its generated ID.
    pub fn begin_source_scan(&mut self, source_id: &str) -> Result<i64> {
        let now = Utc::now();
        self.connection.execute(
            "INSERT INTO source_scans (source_id, source_root_id, started_at, finished_at, status, metadata_json)
             VALUES (?1, NULL, ?2, NULL, ?3, NULL)",
            params![
                source_id,
                now.to_rfc3339(),
                SourceScanStatus::Running.as_str(),
            ],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Finalizes a source-scan row with a terminal status and optional metadata.
    pub fn finish_source_scan(
        &mut self,
        source_scan_id: i64,
        status: SourceScanStatus,
        metadata_json: Option<&Value>,
    ) -> Result<()> {
        let now = Utc::now();
        let metadata_json = serialize_metadata_json(metadata_json)?;
        self.connection.execute(
            "UPDATE source_scans
             SET finished_at = ?2, status = ?3, metadata_json = ?4
             WHERE source_scan_id = ?1",
            params![
                source_scan_id,
                now.to_rfc3339(),
                status.as_str(),
                metadata_json,
            ],
        )?;
        Ok(())
    }

    /// Reads one source-scan row by ID.
    pub fn get_source_scan(&self, source_scan_id: i64) -> Result<Option<SourceScanRecord>> {
        self.connection
            .query_row(
                "SELECT source_scan_id, source_id, started_at, finished_at, status, metadata_json
                 FROM source_scans
                 WHERE source_scan_id = ?1",
                params![source_scan_id],
                source_scan_from_row,
            )
            .optional()
            .context("failed to read metadata source-scan row")
    }

    /// Returns the autoincrement row id for an existing source session.
    pub fn get_source_session_id(
        &self,
        source_id: &str,
        external_session_id: &str,
    ) -> Result<Option<i64>> {
        self.connection
            .query_row(
                "SELECT source_session_id FROM source_sessions
                 WHERE source_id = ?1 AND external_session_id = ?2",
                params![source_id, external_session_id],
                |row| row.get(0),
            )
            .optional()
            .context("failed to read source-session id")
    }

    /// Inserts or updates one source-root row and returns its id.
    pub fn upsert_source_root(
        &mut self,
        source_id: &str,
        root_path: Option<&str>,
        root_uri: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.connection.execute(
            "INSERT INTO source_roots (source_id, root_path, root_uri, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(source_id, root_path, root_uri) DO UPDATE SET updated_at = excluded.updated_at",
            params![source_id, root_path, root_uri, now],
        )?;
        self.connection
            .query_row(
                "SELECT source_root_id FROM source_roots
                 WHERE source_id = ?1 AND root_path IS ?2 AND root_uri IS ?3",
                params![source_id, root_path, root_uri],
                |row| row.get(0),
            )
            .context("failed to read source-root id after upsert")
    }

    /// Inserts or updates one workspace-root row and returns its id.
    pub fn upsert_workspace_root(
        &mut self,
        root_path: &str,
        root_uri: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.connection.execute(
            "INSERT INTO workspace_roots (root_path, root_uri, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?3)
             ON CONFLICT(root_path) DO UPDATE SET
                root_uri = excluded.root_uri,
                updated_at = excluded.updated_at",
            params![root_path, root_uri, now],
        )?;
        self.connection
            .query_row(
                "SELECT workspace_root_id FROM workspace_roots WHERE root_path = ?1",
                params![root_path],
                |row| row.get(0),
            )
            .context("failed to read workspace-root id after upsert")
    }

    /// Inserts or updates one git-repository row and returns its id.
    pub fn upsert_git_repository(
        &mut self,
        repo_path: Option<&str>,
        repo_uri: Option<&str>,
        remote_url: Option<&str>,
        head_commit: Option<&str>,
    ) -> Result<i64> {
        let now = Utc::now().to_rfc3339();
        self.connection.execute(
            "INSERT INTO git_repositories (repo_path, repo_uri, remote_url, head_commit, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?5)
             ON CONFLICT(repo_path, repo_uri) DO UPDATE SET
                remote_url = excluded.remote_url,
                head_commit = excluded.head_commit,
                updated_at = excluded.updated_at",
            params![repo_path, repo_uri, remote_url, head_commit, now],
        )?;
        self.connection
            .query_row(
                "SELECT git_repository_id FROM git_repositories
                 WHERE repo_path IS ?1 AND repo_uri IS ?2",
                params![repo_path, repo_uri],
                |row| row.get(0),
            )
            .context("failed to read git-repository id after upsert")
    }

    /// Links a source session to a workspace root (idempotent).
    pub fn link_session_workspace_root(
        &mut self,
        source_session_id: i64,
        workspace_root_id: i64,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO source_session_workspace_roots
                (source_session_id, workspace_root_id) VALUES (?1, ?2)",
            params![source_session_id, workspace_root_id],
        )?;
        Ok(())
    }

    /// Links a source session to a git repository (idempotent).
    pub fn link_session_git_repository(
        &mut self,
        source_session_id: i64,
        git_repository_id: i64,
    ) -> Result<()> {
        self.connection.execute(
            "INSERT OR IGNORE INTO source_session_git_repositories
                (source_session_id, git_repository_id) VALUES (?1, ?2)",
            params![source_session_id, git_repository_id],
        )?;
        Ok(())
    }

    /// Lists workspace-root paths linked to a source session.
    pub fn list_session_workspace_roots(&self, source_session_id: i64) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT w.root_path FROM workspace_roots w
             JOIN source_session_workspace_roots l ON l.workspace_root_id = w.workspace_root_id
             WHERE l.source_session_id = ?1
             ORDER BY w.root_path",
        )?;
        let rows = statement
            .query_map(params![source_session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Lists git-repository paths linked to a source session.
    pub fn list_session_git_repositories(&self, source_session_id: i64) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT g.repo_path FROM git_repositories g
             JOIN source_session_git_repositories l ON l.git_repository_id = g.git_repository_id
             WHERE l.source_session_id = ?1 AND g.repo_path IS NOT NULL
             ORDER BY g.repo_path",
        )?;
        let rows = statement
            .query_map(params![source_session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Inserts one source-session resource pointer (idempotent on kind+path).
    pub fn upsert_source_session_resource(
        &mut self,
        source_session_id: i64,
        resource_kind: &str,
        resource_path: Option<&str>,
        resource_uri: Option<&str>,
        metadata_json: Option<&Value>,
    ) -> Result<()> {
        let metadata_json = serialize_metadata_json(metadata_json)?;
        self.connection.execute(
            "INSERT INTO source_session_resources
                (source_session_id, resource_kind, resource_path, resource_uri, metadata_json)
             SELECT ?1, ?2, ?3, ?4, ?5
             WHERE NOT EXISTS (
                SELECT 1 FROM source_session_resources
                WHERE source_session_id = ?1 AND resource_kind = ?2 AND resource_path IS ?3
             )",
            params![
                source_session_id,
                resource_kind,
                resource_path,
                resource_uri,
                metadata_json
            ],
        )?;
        Ok(())
    }

    /// Lists resource kinds/paths for a source session.
    pub fn list_source_session_resources(
        &self,
        source_session_id: i64,
    ) -> Result<Vec<(String, Option<String>)>> {
        let mut statement = self.connection.prepare(
            "SELECT resource_kind, resource_path FROM source_session_resources
             WHERE source_session_id = ?1
             ORDER BY resource_kind, resource_path",
        )?;
        let rows = statement
            .query_map(params![source_session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Links a Brick provenance session to a source session (idempotent).
    pub fn link_brick_session_to_source_session(
        &mut self,
        brick_session_id: &str,
        source_session_id: i64,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        self.connection.execute(
            "INSERT OR IGNORE INTO brick_session_source_sessions
                (brick_session_id, source_session_id, linked_at) VALUES (?1, ?2, ?3)",
            params![brick_session_id, source_session_id, now],
        )?;
        Ok(())
    }

    /// Lists `(source_id, external_session_id)` pairs linked to a Brick session.
    pub fn list_source_sessions_for_brick_session(
        &self,
        brick_session_id: &str,
    ) -> Result<Vec<(String, String)>> {
        let mut statement = self.connection.prepare(
            "SELECT s.source_id, s.external_session_id FROM source_sessions s
             JOIN brick_session_source_sessions b ON b.source_session_id = s.source_session_id
             WHERE b.brick_session_id = ?1
             ORDER BY s.source_id, s.external_session_id",
        )?;
        let rows = statement
            .query_map(params![brick_session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Lists Brick session ids linked to a source session.
    pub fn list_brick_sessions_for_source_session(
        &self,
        source_session_id: i64,
    ) -> Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT brick_session_id FROM brick_session_source_sessions
             WHERE source_session_id = ?1
             ORDER BY linked_at",
        )?;
        let rows = statement
            .query_map(params![source_session_id], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    /// Inserts or updates one source-plan row and replaces its recovered session edges.
    pub fn upsert_source_plan_with_edges(
        &mut self,
        input: &SourcePlanWithEdgesUpsert,
    ) -> Result<SourcePlanRecord> {
        let transaction = self
            .connection
            .transaction()
            .context("failed to start metadata source-plan upsert")?;
        let now = Utc::now();
        let metadata_json = serialize_metadata_json(input.plan.metadata_json.as_ref())?;
        transaction.execute(
            "INSERT INTO source_plans (
                source_id, external_plan_id, title, source_path, source_uri, source_mtime,
                parser_version, discovered_at, last_seen_at, created_at, updated_at, metadata_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(source_id, external_plan_id) DO UPDATE SET
                title = excluded.title,
                source_path = excluded.source_path,
                source_uri = excluded.source_uri,
                source_mtime = excluded.source_mtime,
                parser_version = excluded.parser_version,
                discovered_at = excluded.discovered_at,
                last_seen_at = excluded.last_seen_at,
                updated_at = excluded.updated_at,
                metadata_json = excluded.metadata_json",
            params![
                input.plan.source_id,
                input.plan.external_plan_id,
                input.plan.title,
                input
                    .plan
                    .source_path
                    .as_ref()
                    .map(|path| path.display().to_string()),
                input.plan.source_uri,
                input.plan.source_mtime.map(|value| value.to_rfc3339()),
                input.plan.parser_version,
                input.plan.discovered_at.to_rfc3339(),
                input.plan.last_seen_at.to_rfc3339(),
                now.to_rfc3339(),
                now.to_rfc3339(),
                metadata_json,
            ],
        )?;
        let source_plan_id = read_source_plan_id(
            &transaction,
            &input.plan.source_id,
            &input.plan.external_plan_id,
        )?
        .context("metadata source-plan row missing after upsert")?;
        transaction.execute(
            "DELETE FROM source_plan_session_edges WHERE source_plan_id = ?1",
            params![source_plan_id],
        )?;
        for edge in &input.edges {
            if edge.source_id != input.plan.source_id
                || edge.external_plan_id != input.plan.external_plan_id
            {
                anyhow::bail!(
                    "source plan edge key does not match plan key: {}/{}",
                    edge.source_id,
                    edge.external_plan_id
                );
            }
            let todo_ids_json = serialize_metadata_json(edge.todo_ids_json.as_ref())?;
            let edge_metadata_json = serialize_metadata_json(edge.metadata_json.as_ref())?;
            transaction.execute(
                "INSERT INTO source_plan_session_edges (
                    source_plan_id, external_session_id, role, todo_ids_json,
                    discovered_at, last_seen_at, created_at, updated_at, metadata_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                params![
                    source_plan_id,
                    edge.external_session_id,
                    edge.role.as_str(),
                    todo_ids_json,
                    edge.discovered_at.to_rfc3339(),
                    edge.last_seen_at.to_rfc3339(),
                    now.to_rfc3339(),
                    now.to_rfc3339(),
                    edge_metadata_json,
                ],
            )?;
        }
        let record = read_source_plan(
            &transaction,
            &input.plan.source_id,
            &input.plan.external_plan_id,
        )?;
        transaction
            .commit()
            .context("failed to commit metadata source-plan upsert")?;
        record.context("metadata source-plan row missing after upsert")
    }

    /// Lists source-plan rows in deterministic most-recent-first order.
    pub fn list_source_plans(&self, query: &SourcePlanListQuery) -> Result<Vec<SourcePlanRecord>> {
        let limit = normalized_limit(query.limit);
        let offset = normalized_offset(query.offset);
        let mut statement = self.connection.prepare(
            "SELECT source_id, external_plan_id, title, source_path, source_uri, source_mtime,
                    parser_version, discovered_at, last_seen_at, created_at, updated_at, metadata_json
             FROM source_plans
             WHERE (?1 IS NULL OR source_id = ?1)
             ORDER BY last_seen_at DESC, source_id ASC, external_plan_id ASC
             LIMIT ?2 OFFSET ?3",
        )?;
        let rows = statement.query_map(
            params![query.source_id, limit, offset],
            source_plan_from_row,
        )?;
        let mut records = Vec::new();
        for row in rows {
            records.push(row.context("failed to read metadata source-plan row")?);
        }
        Ok(records)
    }

    /// Counts source-plan rows for pagination metadata.
    pub fn count_source_plans(&self, source_id: Option<&str>) -> Result<usize> {
        let count = self.connection.query_row(
            "SELECT COUNT(*) FROM source_plans WHERE (?1 IS NULL OR source_id = ?1)",
            params![source_id],
            |row| row.get::<_, i64>(0),
        )?;
        usize::try_from(count).context("metadata source-plan count exceeds usize")
    }

    /// Lists recovered source plan-to-session edges.
    pub fn list_source_plan_session_edges(
        &self,
        source_id: Option<&str>,
        external_plan_ids: &[String],
    ) -> Result<Vec<SourcePlanSessionEdgeRecord>> {
        let mut statement = self.connection.prepare(
            "SELECT p.source_id, p.external_plan_id, e.external_session_id, e.role, e.todo_ids_json,
                    e.discovered_at, e.last_seen_at, e.created_at, e.updated_at, e.metadata_json
             FROM source_plan_session_edges e
             JOIN source_plans p ON p.source_plan_id = e.source_plan_id
             WHERE (?1 IS NULL OR p.source_id = ?1)
             ORDER BY p.source_id ASC, p.external_plan_id ASC, e.external_session_id ASC, e.role ASC",
        )?;
        let rows = statement.query_map(params![source_id], source_plan_session_edge_from_row)?;
        let mut records = Vec::new();
        for row in rows {
            let record = row.context("failed to read metadata source-plan edge row")?;
            if external_plan_ids.is_empty() || external_plan_ids.contains(&record.external_plan_id)
            {
                records.push(record);
            }
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
             session_created_at TEXT,
             session_updated_at TEXT,
             model TEXT,
             input_tokens INTEGER,
             output_tokens INTEGER,
             repo_path TEXT,
             branch TEXT,
             files_changed INTEGER,
             lines_added INTEGER,
             lines_removed INTEGER,
             touched_files_json TEXT,
             listable INTEGER NOT NULL DEFAULT 1,
             discovered_at TEXT NOT NULL,
             last_seen_at TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             metadata_json TEXT,
             UNIQUE(source_id, external_session_id)
         );
         CREATE TABLE IF NOT EXISTS source_plans (
             source_plan_id INTEGER PRIMARY KEY AUTOINCREMENT,
             source_id TEXT NOT NULL,
             external_plan_id TEXT NOT NULL,
             title TEXT,
             source_path TEXT,
             source_uri TEXT,
             source_mtime TEXT,
             parser_version TEXT,
             discovered_at TEXT NOT NULL,
             last_seen_at TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             metadata_json TEXT,
             UNIQUE(source_id, external_plan_id)
         );
         CREATE TABLE IF NOT EXISTS source_plan_session_edges (
             source_plan_session_edge_id INTEGER PRIMARY KEY AUTOINCREMENT,
             source_plan_id INTEGER NOT NULL,
             external_session_id TEXT NOT NULL,
             role TEXT NOT NULL,
             todo_ids_json TEXT,
             discovered_at TEXT NOT NULL,
             last_seen_at TEXT NOT NULL,
             created_at TEXT NOT NULL,
             updated_at TEXT NOT NULL,
             metadata_json TEXT,
             UNIQUE(source_plan_id, external_session_id, role),
             FOREIGN KEY(source_plan_id) REFERENCES source_plans(source_plan_id) ON DELETE CASCADE
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
         CREATE INDEX IF NOT EXISTS idx_source_sessions_repo_path ON source_sessions(source_id, repo_path);
         CREATE INDEX IF NOT EXISTS idx_source_sessions_fingerprint ON source_sessions(source_fingerprint);
         CREATE INDEX IF NOT EXISTS idx_source_plans_source ON source_plans(source_id, last_seen_at);
         CREATE INDEX IF NOT EXISTS idx_source_plans_path ON source_plans(source_path);
         CREATE INDEX IF NOT EXISTS idx_source_plan_edges_session ON source_plan_session_edges(external_session_id, role);", 
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
    connection.execute_batch("PRAGMA foreign_keys = OFF;")?;
    for table in existing_user_tables(connection)? {
        connection.execute(
            &format!("DROP TABLE IF EXISTS {}", quote_identifier(&table)),
            [],
        )?;
    }
    create_schema(connection)?;
    connection.execute_batch("PRAGMA foreign_keys = ON;")?;
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

fn read_source_plan_id(
    connection: &Connection,
    source_id: &str,
    external_plan_id: &str,
) -> Result<Option<i64>> {
    connection
        .query_row(
            "SELECT source_plan_id FROM source_plans WHERE source_id = ?1 AND external_plan_id = ?2",
            params![source_id, external_plan_id],
            |row| row.get(0),
        )
        .optional()
        .context("failed to read metadata source-plan ID")
}

fn read_source_plan(
    connection: &Connection,
    source_id: &str,
    external_plan_id: &str,
) -> Result<Option<SourcePlanRecord>> {
    connection
        .query_row(
            "SELECT source_id, external_plan_id, title, source_path, source_uri, source_mtime,
                    parser_version, discovered_at, last_seen_at, created_at, updated_at, metadata_json
             FROM source_plans
             WHERE source_id = ?1 AND external_plan_id = ?2",
            params![source_id, external_plan_id],
            source_plan_from_row,
        )
        .optional()
        .context("failed to read metadata source-plan row")
}

fn source_plan_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourcePlanRecord> {
    let source_path: Option<String> = row.get(3)?;
    let source_mtime: Option<String> = row.get(5)?;
    let discovered_at: String = row.get(7)?;
    let last_seen_at: String = row.get(8)?;
    let created_at: String = row.get(9)?;
    let updated_at: String = row.get(10)?;
    let metadata_json: Option<String> = row.get(11)?;
    Ok(SourcePlanRecord {
        source_id: row.get(0)?,
        external_plan_id: row.get(1)?,
        title: row.get(2)?,
        source_path: source_path.map(PathBuf::from),
        source_uri: row.get(4)?,
        source_mtime: parse_optional_datetime(source_mtime)?,
        parser_version: row.get(6)?,
        discovered_at: parse_datetime(discovered_at)?,
        last_seen_at: parse_datetime(last_seen_at)?,
        created_at: parse_datetime(created_at)?,
        updated_at: parse_datetime(updated_at)?,
        metadata_json: parse_metadata_json(metadata_json)?,
    })
}

fn source_plan_session_edge_from_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<SourcePlanSessionEdgeRecord> {
    let role: String = row.get(3)?;
    let todo_ids_json: Option<String> = row.get(4)?;
    let discovered_at: String = row.get(5)?;
    let last_seen_at: String = row.get(6)?;
    let created_at: String = row.get(7)?;
    let updated_at: String = row.get(8)?;
    let metadata_json: Option<String> = row.get(9)?;
    Ok(SourcePlanSessionEdgeRecord {
        source_id: row.get(0)?,
        external_plan_id: row.get(1)?,
        external_session_id: row.get(2)?,
        role: role.parse().map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(
                3,
                rusqlite::types::Type::Text,
                Box::<dyn std::error::Error + Send + Sync>::from(err),
            )
        })?,
        todo_ids_json: parse_metadata_json(todo_ids_json)?,
        discovered_at: parse_datetime(discovered_at)?,
        last_seen_at: parse_datetime(last_seen_at)?,
        created_at: parse_datetime(created_at)?,
        updated_at: parse_datetime(updated_at)?,
        metadata_json: parse_metadata_json(metadata_json)?,
    })
}

fn read_source_profile(
    connection: &Connection,
    source_id: &str,
) -> Result<Option<SourceProfileRecord>> {
    connection
        .query_row(
            "SELECT source_id, name, app_id, actor_id, actor_type, profile_json,
                    created_at, updated_at
             FROM source_profiles
             WHERE source_id = ?1",
            params![source_id],
            source_profile_from_row,
        )
        .optional()
        .context("failed to read metadata source-profile row")
}

fn source_profile_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceProfileRecord> {
    let profile_json: Option<String> = row.get(5)?;
    let created_at: String = row.get(6)?;
    let updated_at: String = row.get(7)?;
    Ok(SourceProfileRecord {
        source_id: row.get(0)?,
        name: row.get(1)?,
        app_id: row.get(2)?,
        actor_id: row.get(3)?,
        actor_type: row.get(4)?,
        profile_json: parse_metadata_json(profile_json)?,
        created_at: parse_datetime(created_at)?,
        updated_at: parse_datetime(updated_at)?,
    })
}

fn source_scan_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceScanRecord> {
    let started_at: String = row.get(2)?;
    let finished_at: Option<String> = row.get(3)?;
    let metadata_json: Option<String> = row.get(5)?;
    Ok(SourceScanRecord {
        source_scan_id: row.get(0)?,
        source_id: row.get(1)?,
        started_at: parse_datetime(started_at)?,
        finished_at: parse_optional_datetime(finished_at)?,
        status: row.get(4)?,
        metadata_json: parse_metadata_json(metadata_json)?,
    })
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
                    session_created_at, session_updated_at, model, input_tokens, output_tokens, repo_path, branch,
                    files_changed, lines_added, lines_removed, touched_files_json, listable,
                    discovered_at, last_seen_at, created_at, updated_at, metadata_json
             FROM source_sessions
             WHERE source_id = ?1 AND external_session_id = ?2",
            params![source_id, external_session_id],
            source_session_from_row,
        )
        .optional()
        .context("failed to read metadata source-session row")
}

fn source_session_blame_row(file_path: &str, record: SourceSessionRecord) -> FileSessionBlameRow {
    let app_id = record
        .metadata_json
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("app_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| Some(record.source_id.clone()));
    let actor_id = record
        .metadata_json
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("actor_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    let actor_type = record
        .metadata_json
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("actor_type"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    FileSessionBlameRow {
        file_path: file_path.to_string(),
        session_id: None,
        external_session_id: Some(record.external_session_id.clone()),
        source_id: Some(record.source_id.clone()),
        app_id,
        actor_id,
        actor_type,
        evidence_kind: FileSessionBlameEvidenceKind::SourceMetadata,
        last_seen_at: record.last_seen_at.to_rfc3339(),
        lines_added: record.lines_added,
        lines_removed: record.lines_removed,
        files_changed: record.files_changed,
        confidence: Some("metadata_only".to_string()),
        source_pointer: Some(json!({
            "source_id": record.source_id,
            "external_session_id": record.external_session_id,
            "source_path": record.source_path.map(|path| path.display().to_string()),
            "source_uri": record.source_uri,
            "source_record_key": record
                .metadata_json
                .as_ref()
                .and_then(Value::as_object)
                .and_then(|metadata| metadata.get("source_record_key"))
                .and_then(Value::as_str),
            "parser_version": record.parser_version,
            "repo_path": record.repo_path.map(|path| path.display().to_string()),
            "branch": record.branch,
            "source_fingerprint": record.source_fingerprint,
        })),
    }
}

fn touched_files_from_value(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn source_session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceSessionRecord> {
    let source_path: Option<String> = row.get(4)?;
    let source_mtime: Option<String> = row.get(6)?;
    let source_size: Option<i64> = row.get(7)?;
    let session_created_at: Option<String> = row.get(10)?;
    let session_updated_at: Option<String> = row.get(11)?;
    let input_tokens: Option<i64> = row.get(13)?;
    let output_tokens: Option<i64> = row.get(14)?;
    let repo_path: Option<String> = row.get(15)?;
    let files_changed: Option<i64> = row.get(17)?;
    let lines_added: Option<i64> = row.get(18)?;
    let lines_removed: Option<i64> = row.get(19)?;
    let touched_files_json: Option<String> = row.get(20)?;
    let discovered_at: String = row.get(22)?;
    let last_seen_at: String = row.get(23)?;
    let created_at: String = row.get(24)?;
    let updated_at: String = row.get(25)?;
    let metadata_json: Option<String> = row.get(26)?;
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
        session_created_at: parse_optional_datetime(session_created_at)?,
        session_updated_at: parse_optional_datetime(session_updated_at)?,
        model: row.get(12)?,
        input_tokens: optional_i64_to_u64(input_tokens)?,
        output_tokens: optional_i64_to_u64(output_tokens)?,
        repo_path: repo_path.map(PathBuf::from),
        branch: row.get(16)?,
        files_changed: optional_i64_to_u64(files_changed)?,
        lines_added: optional_i64_to_u64(lines_added)?,
        lines_removed: optional_i64_to_u64(lines_removed)?,
        touched_files_json: parse_metadata_json(touched_files_json)?,
        listable: row.get(21)?,
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

fn normalized_offset(offset: usize) -> i64 {
    i64::try_from(offset).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use chrono::{TimeZone, Utc};
    use serde_json::json;

    use super::*;
    use crate::{SourcePlanSessionEdgeUpsert, SourcePlanWithEdgesUpsert};

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
            session_created_at: Some(discovered_at),
            session_updated_at: Some(last_seen_at),
            model: Some("model-a".to_string()),
            input_tokens: Some(10),
            output_tokens: Some(20),
            repo_path: Some(PathBuf::from("/tmp/repo")),
            branch: Some("main".to_string()),
            files_changed: Some(2),
            lines_added: Some(3),
            lines_removed: Some(4),
            touched_files_json: Some(json!(["src/lib.rs", "README.md"])),
            listable: true,
            discovered_at,
            last_seen_at,
            metadata_json: Some(json!({
                "phase": "first-slice",
                "app_id": "test-app",
                "actor_id": "agent-1",
                "actor_type": "agent",
            })),
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
        assert_eq!(inserted.session_created_at, inserted.source_mtime);
        assert_eq!(inserted.model.as_deref(), Some("model-a"));
        assert_eq!(inserted.input_tokens, Some(10));
        assert_eq!(inserted.output_tokens, Some(20));
        assert_eq!(inserted.repo_path.as_deref(), Some(Path::new("/tmp/repo")));
        assert_eq!(inserted.branch.as_deref(), Some("main"));
        assert_eq!(inserted.files_changed, Some(2));
        assert_eq!(inserted.lines_added, Some(3));
        assert_eq!(inserted.lines_removed, Some(4));
        assert_eq!(
            inserted.touched_files_json,
            Some(json!(["src/lib.rs", "README.md"]))
        );
        assert!(inserted.listable);
        assert_eq!(
            inserted.metadata_json,
            Some(json!({
                "phase": "first-slice",
                "app_id": "test-app",
                "actor_id": "agent-1",
                "actor_type": "agent",
            }))
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
                offset: 0,
            })
            .expect("list source sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].external_session_id, TEST_EXTERNAL_SESSION_ID);
        assert_eq!(
            sessions[0].parser_version.as_deref(),
            Some(TEST_PARSER_VERSION)
        );
        assert_eq!(
            db.count_source_sessions(Some(TEST_SOURCE_ID))
                .expect("count source sessions"),
            1
        );
    }

    #[test]
    fn queries_source_metadata_file_session_blame() {
        let path = temp_home("source-blame").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");
        db.upsert_source_session(&sample_upsert("Blame session", 0))
            .expect("insert source session");

        let rows = db
            .query_source_file_session_blame(&SourceFileSessionBlameQuery {
                file_path: "src/lib.rs".to_string(),
                source_id: Some(TEST_SOURCE_ID.to_string()),
                repo_path: Some(PathBuf::from("/tmp/repo")),
                limit: 20,
            })
            .expect("query source file blame");

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].file_path, "src/lib.rs");
        assert_eq!(
            rows[0].external_session_id.as_deref(),
            Some(TEST_EXTERNAL_SESSION_ID)
        );
        assert_eq!(rows[0].source_id.as_deref(), Some(TEST_SOURCE_ID));
        assert_eq!(rows[0].app_id.as_deref(), Some("test-app"));
        assert_eq!(rows[0].actor_id.as_deref(), Some("agent-1"));
        assert_eq!(rows[0].actor_type.as_deref(), Some("agent"));
        assert_eq!(rows[0].evidence_kind.as_str(), "source_metadata");
        assert_eq!(rows[0].lines_added, Some(3));
        assert_eq!(rows[0].lines_removed, Some(4));
        assert_eq!(rows[0].files_changed, Some(2));
        assert_eq!(rows[0].confidence.as_deref(), Some("metadata_only"));

        let missing_rows = db
            .query_source_file_session_blame(&SourceFileSessionBlameQuery {
                file_path: "src/missing.rs".to_string(),
                source_id: Some(TEST_SOURCE_ID.to_string()),
                repo_path: Some(PathBuf::from("/tmp/repo")),
                limit: 20,
            })
            .expect("query missing source file blame");
        assert!(missing_rows.is_empty());
    }

    #[test]
    fn upserts_source_plans_and_preserves_unresolved_session_edges() {
        let path = temp_home("plan-edges").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 18, 2, 3, 4)
            .single()
            .expect("valid plan timestamp");
        let input = SourcePlanWithEdgesUpsert {
            plan: crate::SourcePlanUpsert {
                source_id: TEST_SOURCE_ID.to_string(),
                external_plan_id: "plan-1".to_string(),
                title: Some("Plan one".to_string()),
                source_path: Some(PathBuf::from("/tmp/plan-1.plan.md")),
                source_uri: Some("file:///tmp/plan-1.plan.md".to_string()),
                source_mtime: Some(now),
                parser_version: Some("plan-parser-v1".to_string()),
                discovered_at: now,
                last_seen_at: now,
                metadata_json: Some(json!({ "kind": "cursor-plan" })),
            },
            edges: vec![
                SourcePlanSessionEdgeUpsert {
                    source_id: TEST_SOURCE_ID.to_string(),
                    external_plan_id: "plan-1".to_string(),
                    external_session_id: "missing-session".to_string(),
                    role: crate::SourcePlanSessionEdgeRole::ReferencedBy,
                    todo_ids_json: None,
                    discovered_at: now,
                    last_seen_at: now,
                    metadata_json: None,
                },
                SourcePlanSessionEdgeUpsert {
                    source_id: TEST_SOURCE_ID.to_string(),
                    external_plan_id: "plan-1".to_string(),
                    external_session_id: "builder-session".to_string(),
                    role: crate::SourcePlanSessionEdgeRole::BuiltBy,
                    todo_ids_json: Some(json!(["todo-1"])),
                    discovered_at: now,
                    last_seen_at: now,
                    metadata_json: None,
                },
            ],
        };

        let plan = db
            .upsert_source_plan_with_edges(&input)
            .expect("upsert source plan");
        let plans = db
            .list_source_plans(&SourcePlanListQuery {
                source_id: Some(TEST_SOURCE_ID.to_string()),
                limit: 10,
                offset: 0,
            })
            .expect("list source plans");
        let edges = db
            .list_source_plan_session_edges(Some(TEST_SOURCE_ID), &[])
            .expect("list source plan edges");

        assert_eq!(plan.external_plan_id, "plan-1");
        assert_eq!(plans.len(), 1);
        assert_eq!(edges.len(), 2);
        assert!(edges.iter().any(|edge| {
            edge.external_session_id == "missing-session"
                && edge.role == crate::SourcePlanSessionEdgeRole::ReferencedBy
        }));
        assert!(edges.iter().any(|edge| {
            edge.external_session_id == "builder-session"
                && edge.role == crate::SourcePlanSessionEdgeRole::BuiltBy
                && edge.todo_ids_json == Some(json!(["todo-1"]))
        }));
    }

    #[test]
    fn lists_source_plans_with_pagination_and_filters_edges_to_page() {
        let path = temp_home("plan-pagination").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");
        let base = Utc
            .with_ymd_and_hms(2026, 6, 18, 2, 3, 4)
            .single()
            .expect("valid plan timestamp");

        for index in 0..3 {
            let plan_id = format!("plan-{index}");
            db.upsert_source_plan_with_edges(&SourcePlanWithEdgesUpsert {
                plan: crate::SourcePlanUpsert {
                    source_id: TEST_SOURCE_ID.to_string(),
                    external_plan_id: plan_id.clone(),
                    title: Some(plan_id.clone()),
                    source_path: Some(PathBuf::from(format!("/tmp/{plan_id}.plan.md"))),
                    source_uri: None,
                    source_mtime: Some(base),
                    parser_version: Some("plan-parser-v1".to_string()),
                    discovered_at: base,
                    last_seen_at: base + chrono::Duration::seconds(index),
                    metadata_json: None,
                },
                edges: vec![SourcePlanSessionEdgeUpsert {
                    source_id: TEST_SOURCE_ID.to_string(),
                    external_plan_id: plan_id.clone(),
                    external_session_id: format!("session-{index}"),
                    role: crate::SourcePlanSessionEdgeRole::ReferencedBy,
                    todo_ids_json: None,
                    discovered_at: base,
                    last_seen_at: base,
                    metadata_json: None,
                }],
            })
            .expect("upsert source plan");
        }

        let plans = db
            .list_source_plans(&SourcePlanListQuery {
                source_id: Some(TEST_SOURCE_ID.to_string()),
                limit: 1,
                offset: 1,
            })
            .expect("list source plans");
        let plan_ids = plans
            .iter()
            .map(|plan| plan.external_plan_id.clone())
            .collect::<Vec<_>>();
        let edges = db
            .list_source_plan_session_edges(Some(TEST_SOURCE_ID), &plan_ids)
            .expect("list source plan edges");

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].external_plan_id, "plan-1");
        assert_eq!(
            db.count_source_plans(Some(TEST_SOURCE_ID))
                .expect("count source plans"),
            3
        );
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].external_plan_id, "plan-1");
        assert_eq!(edges[0].external_session_id, "session-1");
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

    #[test]
    fn resets_incomplete_foreign_key_schema() {
        let path = temp_home("reset-incomplete-fk").join(crate::METADATA_DB_FILE);
        let connection = Connection::open(&path).expect("open raw metadata DB");
        connection
            .execute_batch(
                "CREATE TABLE metadata (key TEXT PRIMARY KEY, value TEXT NOT NULL);
                 INSERT INTO metadata (key, value) VALUES ('schema_version', '1');
                 CREATE TABLE source_sessions (source_session_id INTEGER PRIMARY KEY AUTOINCREMENT);
                 CREATE TABLE source_session_workspace_roots (
                     source_session_id INTEGER NOT NULL,
                     workspace_root_id INTEGER NOT NULL,
                     FOREIGN KEY(source_session_id) REFERENCES source_sessions(source_session_id) ON DELETE CASCADE,
                     FOREIGN KEY(workspace_root_id) REFERENCES workspace_roots(workspace_root_id) ON DELETE CASCADE
                 );",
            )
            .expect("seed incomplete metadata DB");
        drop(connection);

        let db = MetadataDb::open_path(&path).expect("open reset metadata DB");
        assert_eq!(
            db.schema_version().expect("schema version"),
            METADATA_DB_SCHEMA_VERSION
        );
        assert!(table_exists(&db.connection, "workspace_roots").expect("inspect workspace table"));
    }

    #[test]
    fn touch_last_seen_preserves_created_at_and_other_fields() {
        let path = temp_home("touch-last-seen").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");
        let inserted = db
            .upsert_source_session(&sample_upsert("Touch session", 0))
            .expect("insert source session");

        let later = inserted.last_seen_at + chrono::Duration::seconds(30);
        let touched = db
            .touch_source_session_last_seen(TEST_SOURCE_ID, TEST_EXTERNAL_SESSION_ID, later)
            .expect("touch last seen");
        assert!(touched);

        let reread = db
            .get_source_session(TEST_SOURCE_ID, TEST_EXTERNAL_SESSION_ID)
            .expect("read source session")
            .expect("session present");
        assert_eq!(reread.last_seen_at, later);
        assert_eq!(reread.created_at, inserted.created_at);
        assert_eq!(reread.title.as_deref(), Some("Touch session"));
        assert_eq!(reread.source_fingerprint, inserted.source_fingerprint);

        let missing = db
            .touch_source_session_last_seen(TEST_SOURCE_ID, "does-not-exist", later)
            .expect("touch missing session");
        assert!(!missing);
    }

    #[test]
    fn fingerprint_change_drives_reindex_decision() {
        let path = temp_home("fingerprint-delta").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");

        let mut first = sample_upsert("Delta session", 0);
        first.source_fingerprint = Some("2026-06-18T01:02:03+00:00:42".to_string());
        let inserted = db
            .upsert_source_session(&first)
            .expect("insert source session");

        // Same fingerprint -> caller should skip and only touch last_seen.
        let existing = db
            .get_source_session(TEST_SOURCE_ID, TEST_EXTERNAL_SESSION_ID)
            .expect("read existing")
            .expect("present");
        assert_eq!(
            existing.source_fingerprint.as_deref(),
            Some("2026-06-18T01:02:03+00:00:42")
        );

        // Changed fingerprint -> caller should re-upsert; created_at stays stable.
        let mut second = sample_upsert("Delta session updated", 1);
        second.source_fingerprint = Some("2026-06-18T09:09:09+00:00:84".to_string());
        let updated = db
            .upsert_source_session(&second)
            .expect("update source session");
        assert_eq!(
            updated.source_fingerprint.as_deref(),
            Some("2026-06-18T09:09:09+00:00:84")
        );
        assert_eq!(updated.created_at, inserted.created_at);
        assert_ne!(updated.source_fingerprint, inserted.source_fingerprint);
    }

    #[test]
    fn upserts_and_reads_source_profile() {
        let path = temp_home("source-profile").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");

        let inserted = db
            .upsert_source_profile(&SourceProfileUpsert {
                source_id: "cursor_ide".to_string(),
                name: Some("cursor_ide".to_string()),
                app_id: Some("cursor".to_string()),
                actor_id: Some("agent-1".to_string()),
                actor_type: Some("agent".to_string()),
                profile_json: Some(json!({ "name": "cursor_ide", "app_id": "cursor" })),
            })
            .expect("insert source profile");
        assert_eq!(inserted.source_id, "cursor_ide");
        assert_eq!(inserted.app_id.as_deref(), Some("cursor"));

        let updated = db
            .upsert_source_profile(&SourceProfileUpsert {
                source_id: "cursor_ide".to_string(),
                name: Some("cursor_ide".to_string()),
                app_id: Some("cursor-renamed".to_string()),
                actor_id: Some("agent-1".to_string()),
                actor_type: Some("agent".to_string()),
                profile_json: Some(json!({ "name": "cursor_ide" })),
            })
            .expect("update source profile");
        assert_eq!(updated.app_id.as_deref(), Some("cursor-renamed"));
        assert_eq!(updated.created_at, inserted.created_at);

        let read = db
            .get_source_profile("cursor_ide")
            .expect("read source profile")
            .expect("profile present");
        assert_eq!(read.app_id.as_deref(), Some("cursor-renamed"));
        assert_eq!(read.actor_type.as_deref(), Some("agent"));
        assert!(db
            .get_source_profile("missing")
            .expect("read missing profile")
            .is_none());
    }

    #[test]
    fn source_scan_lifecycle_running_to_completed() {
        let path = temp_home("source-scan").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");

        let scan_id = db.begin_source_scan("claude_code").expect("begin scan");
        let running = db
            .get_source_scan(scan_id)
            .expect("read scan")
            .expect("scan present");
        assert_eq!(running.source_id, "claude_code");
        assert_eq!(running.status, "running");
        assert!(running.finished_at.is_none());

        db.finish_source_scan(
            scan_id,
            SourceScanStatus::Completed,
            Some(&json!({ "scanned": 3, "reindexed": 1, "skipped": 2 })),
        )
        .expect("finish scan");

        let completed = db
            .get_source_scan(scan_id)
            .expect("read scan")
            .expect("scan present");
        assert_eq!(completed.status, "completed");
        assert!(completed.finished_at.is_some());
        assert_eq!(
            completed.metadata_json,
            Some(json!({ "scanned": 3, "reindexed": 1, "skipped": 2 }))
        );
    }

    #[test]
    fn links_session_to_workspace_roots_idempotently() {
        let path = temp_home("workspace-links").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");
        db.upsert_source_session(&sample_upsert("WS session", 0))
            .expect("insert session");
        let session_id = db
            .get_source_session_id(TEST_SOURCE_ID, TEST_EXTERNAL_SESSION_ID)
            .expect("read id")
            .expect("id present");

        let root_a = db
            .upsert_workspace_root("/workspace/a", None)
            .expect("upsert root a");
        let root_b = db
            .upsert_workspace_root("/workspace/b", None)
            .expect("upsert root b");
        db.link_session_workspace_root(session_id, root_a)
            .expect("link a");
        db.link_session_workspace_root(session_id, root_b)
            .expect("link b");
        // Re-link is a no-op (idempotent), and re-upsert returns the same id.
        db.link_session_workspace_root(session_id, root_a)
            .expect("relink a");
        assert_eq!(
            db.upsert_workspace_root("/workspace/a", None)
                .expect("re-upsert a"),
            root_a
        );

        let roots = db
            .list_session_workspace_roots(session_id)
            .expect("list roots");
        assert_eq!(roots, vec!["/workspace/a", "/workspace/b"]);
    }

    #[test]
    fn links_session_to_git_repository() {
        let path = temp_home("git-links").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");
        db.upsert_source_session(&sample_upsert("Git session", 0))
            .expect("insert session");
        let session_id = db
            .get_source_session_id(TEST_SOURCE_ID, TEST_EXTERNAL_SESSION_ID)
            .expect("read id")
            .expect("id present");

        let repo_id = db
            .upsert_git_repository(
                Some("/workspace/repo"),
                None,
                Some("git@host:repo.git"),
                None,
            )
            .expect("upsert repo");
        db.link_session_git_repository(session_id, repo_id)
            .expect("link repo");

        let repos = db
            .list_session_git_repositories(session_id)
            .expect("list repos");
        assert_eq!(repos, vec!["/workspace/repo"]);
    }

    #[test]
    fn records_source_roots_and_session_resources() {
        let path = temp_home("roots-resources").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");
        let root_id = db
            .upsert_source_root(TEST_SOURCE_ID, Some("/logs/source"), None)
            .expect("upsert source root");
        assert_eq!(
            db.upsert_source_root(TEST_SOURCE_ID, Some("/logs/source"), None)
                .expect("re-upsert source root"),
            root_id
        );

        db.upsert_source_session(&sample_upsert("Res session", 0))
            .expect("insert session");
        let session_id = db
            .get_source_session_id(TEST_SOURCE_ID, TEST_EXTERNAL_SESSION_ID)
            .expect("read id")
            .expect("id present");
        db.upsert_source_session_resource(
            session_id,
            "plan_file",
            Some("/plans/plan-1.md"),
            None,
            Some(&json!({ "kind": "cursor-plan" })),
        )
        .expect("insert resource");
        // Idempotent on (kind, path).
        db.upsert_source_session_resource(
            session_id,
            "plan_file",
            Some("/plans/plan-1.md"),
            None,
            None,
        )
        .expect("re-insert resource");

        let resources = db
            .list_source_session_resources(session_id)
            .expect("list resources");
        assert_eq!(
            resources,
            vec![(
                "plan_file".to_string(),
                Some("/plans/plan-1.md".to_string())
            )]
        );
    }

    #[test]
    fn bridges_brick_session_to_source_session() {
        let path = temp_home("brick-bridge").join(crate::METADATA_DB_FILE);
        let mut db = MetadataDb::open_path(&path).expect("open metadata DB");
        db.upsert_source_session(&sample_upsert("Bridge session", 0))
            .expect("insert session");
        let session_id = db
            .get_source_session_id(TEST_SOURCE_ID, TEST_EXTERNAL_SESSION_ID)
            .expect("read id")
            .expect("id present");

        db.link_brick_session_to_source_session("brick-sess-1", session_id)
            .expect("link bridge");
        // Idempotent.
        db.link_brick_session_to_source_session("brick-sess-1", session_id)
            .expect("relink bridge");

        let sources = db
            .list_source_sessions_for_brick_session("brick-sess-1")
            .expect("list by brick session");
        assert_eq!(
            sources,
            vec![(
                TEST_SOURCE_ID.to_string(),
                TEST_EXTERNAL_SESSION_ID.to_string()
            )]
        );
        let bricks = db
            .list_brick_sessions_for_source_session(session_id)
            .expect("list by source session");
        assert_eq!(bricks, vec!["brick-sess-1".to_string()]);
    }
}
