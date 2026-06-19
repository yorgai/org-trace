//! JSON history command surface for native source profiles.
//!
//! This module intentionally stays read-only and non-interactive. The first-stage
//! implementation adapts configured source profiles and native session file
//! listings into stable JSON DTOs that can be consumed by ORGII-style callers.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;
#[cfg(test)]
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Result};
use brick_core::{
    format_source_session_chunks, list_source_plans, list_source_sessions, metadata_db_path,
    query_sqlite_file_session_blame, ActivityChunk, FileSessionBlameRow, LocalStore, MetadataDb,
    NativeSourceSession, SourceFileSessionBlameQuery, SourcePlanListQuery, SourcePlanRecord,
    SourcePlanSessionEdgeRecord, SourceProfile, SourceProfileStore, SourceProfileUpsert,
    SourceScanStatus, SourceSessionListQuery, SourceSessionRecord, SourceSessionUpsert,
    SqliteFileSessionBlameQuery,
};
use brick_protocol::ActorType;
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};

use crate::args::{
    HistoryCommand, HistoryExportFormatArg, HistoryExportSchemaArg, HistoryFormatArg,
};

const HISTORY_EXPORT_SCHEMA_AUDIT_V1: &str = "audit-v1";
const HISTORY_EXPORT_SCHEMA_SOURCE_METADATA_V1: &str = "source-metadata-v1";
const EXPORT_AVAILABILITY_METADATA_ONLY: &str = "metadata_only";
const SOURCE_INDEX_REFRESH_LIMIT: usize = 100_000;
const EXPORT_REFRESH_LIMIT: usize = SOURCE_INDEX_REFRESH_LIMIT;

/// Version of the `brick history` adapter contract this binary implements.
pub const HISTORY_CONTRACT_VERSION: u32 = 1;

/// Truncates every long string value inside a chunk's `args`/`result` so a single
/// huge tool output (e.g. a multi-KB command stdout) cannot blow up a paginated
/// listing. Each truncated value is replaced with its head plus a marker that
/// tells the caller exactly how to refetch the full chunk untruncated.
fn truncate_chunk_fields(chunk: &mut ActivityChunk, max_bytes: usize, absolute_offset: usize) {
    truncate_value(&mut chunk.args, max_bytes, absolute_offset);
    truncate_value(&mut chunk.result, max_bytes, absolute_offset);
}

/// Builds a paginated, optionally field-truncated chunk page for one session.
/// Shared by the `history chunks` CLI handler and the MCP `read_session` tool so
/// both stay in lockstep on pagination + truncation behavior.
pub(crate) fn build_chunks_response(
    profile: &SourceProfile,
    source_id: &str,
    session_id: &str,
    limit: usize,
    offset: usize,
    max_field_bytes: usize,
) -> Result<HistoryChunksResponse> {
    let mut metadata_db = MetadataDb::open_global()?;
    refresh_profiles_to_metadata(
        &mut metadata_db,
        std::slice::from_ref(profile),
        EXPORT_REFRESH_LIMIT,
    )?;
    let record = metadata_db
        .get_source_session(&profile.name, session_id)?
        .ok_or_else(|| anyhow!("source session not found: {}/{}", profile.name, session_id))?;
    let all_chunks = format_chunks_for_record(&record)?;
    let total_chunks = all_chunks.len();
    let mut page: Vec<_> = all_chunks.into_iter().skip(offset).take(limit).collect();
    let returned = page.len();
    if max_field_bytes > 0 {
        for (index, chunk) in page.iter_mut().enumerate() {
            truncate_chunk_fields(chunk, max_field_bytes, offset + index);
        }
    }
    Ok(HistoryChunksResponse {
        source_id: source_id.to_string(),
        session_id: session_id.to_string(),
        total_chunks,
        offset,
        returned,
        has_more: offset + returned < total_chunks,
        chunks: page,
    })
}

/// Recursively truncates string leaves over `max_bytes`, descending into arrays
/// and objects. Non-string scalars are left untouched.
fn truncate_value(value: &mut Value, max_bytes: usize, absolute_offset: usize) {
    match value {
        Value::String(text) => {
            if text.len() > max_bytes {
                *text = truncate_string(text, max_bytes, absolute_offset);
            }
        }
        Value::Array(items) => {
            for item in items {
                truncate_value(item, max_bytes, absolute_offset);
            }
        }
        Value::Object(map) => {
            for (_, item) in map.iter_mut() {
                truncate_value(item, max_bytes, absolute_offset);
            }
        }
        _ => {}
    }
}

/// Keeps the first `max_bytes` of `text` (cut on a UTF-8 char boundary) and
/// appends a marker recording the original byte length and the exact refetch
/// command (`--offset N --limit 1 --max-field-bytes 0`) for the full value.
fn truncate_string(text: &str, max_bytes: usize, absolute_offset: usize) -> String {
    let mut end = max_bytes.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}…[truncated, full {} bytes — refetch: --offset {} --limit 1 --max-field-bytes 0]",
        &text[..end],
        text.len(),
        absolute_offset
    )
}

/// Prints machine-readable Brick version and schema info for adapter gating.
pub fn print_version(format: HistoryFormatArg) -> Result<()> {
    ensure_json(format);
    print_json(&json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "metadata_db_schema_version": brick_core::METADATA_DB_SCHEMA_VERSION,
        "history_contract_version": HISTORY_CONTRACT_VERSION,
    }))
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistorySourcesResponse {
    pub sources: Vec<HistorySourceRow>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistorySourceRow {
    pub source_id: String,
    pub app_id: Option<String>,
    pub actor_id: Option<String>,
    pub actor_type: Option<String>,
    pub selected: bool,
    pub store_root: Option<String>,
    pub session_db_path: Option<String>,
    pub session_log_path: Option<String>,
    pub evidence_root: Option<String>,
    pub cursor_state_db_path: Option<String>,
    pub notes: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistorySessionsResponse {
    pub source_id: String,
    pub limit: usize,
    pub offset: usize,
    pub total: usize,
    pub has_more: bool,
    pub sessions: Vec<HistorySessionRow>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistorySessionRow {
    pub source_id: String,
    pub app_id: String,
    pub session_id: String,
    pub external_session_id: String,
    pub title: Option<String>,
    pub path: String,
    pub size_bytes: u64,
    pub modified_at: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub touched_files: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct HistoryPlansResponse {
    pub source_id: String,
    pub limit: usize,
    pub offset: usize,
    pub total: usize,
    pub has_more: bool,
    pub plans: Vec<HistoryPlanRow>,
    pub edges: Vec<HistoryPlanSessionEdgeRow>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct HistoryPlanRow {
    pub source_id: String,
    pub plan_id: String,
    pub external_plan_id: String,
    pub title: Option<String>,
    pub source_path: Option<String>,
    pub source_uri: Option<String>,
    pub source_mtime: Option<String>,
    pub parser_version: Option<String>,
    pub discovered_at: String,
    pub last_seen_at: String,
    pub created_at: String,
    pub updated_at: String,
    pub metadata_json: Option<Value>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct HistoryPlanSessionEdgeRow {
    pub source_id: String,
    pub plan_id: String,
    pub external_plan_id: String,
    pub external_session_id: String,
    pub role: String,
    pub todo_ids_json: Option<Value>,
    pub discovered_at: String,
    pub last_seen_at: String,
    pub created_at: String,
    pub updated_at: String,
    pub metadata_json: Option<Value>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistoryRecentPathsResponse {
    pub source_id: String,
    pub limit: usize,
    pub paths: Vec<HistoryRecentPathRow>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistoryRecentPathRow {
    pub source_id: String,
    pub app_id: String,
    pub session_id: String,
    pub path: String,
    pub title: Option<String>,
    pub size_bytes: u64,
    pub modified_at: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct HistoryChunksResponse {
    pub source_id: String,
    pub session_id: String,
    /// Total chunks in the session, before pagination.
    pub total_chunks: usize,
    /// Zero-based offset this page started at.
    pub offset: usize,
    /// Number of chunks returned in this page.
    pub returned: usize,
    /// Whether more chunks exist after this page (offset + returned < total).
    pub has_more: bool,
    pub chunks: Vec<ActivityChunkDto>,
}

pub type ActivityChunkDto = ActivityChunk;

#[derive(Debug, Serialize, PartialEq)]
pub struct AuditSessionExportV1 {
    pub schema: String,
    pub exported_at: String,
    pub source: AuditSourceRef,
    pub session: AuditSessionMetadata,
    pub token_usage: AuditTokenUsage,
    pub impact: AuditImpact,
    pub evidence: AuditEvidenceRef,
    pub chunks: Vec<ActivityChunkDto>,
    pub source_metadata: Option<Value>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AuditSourceRef {
    pub source_id: String,
    pub app_id: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AuditSessionMetadata {
    pub session_id: String,
    pub external_session_id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AuditTokenUsage {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AuditImpact {
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub touched_files: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct AuditEvidenceRef {
    pub availability: String,
    pub source_path: String,
    pub source_uri: Option<String>,
    pub source_size_bytes: Option<u64>,
    pub source_modified_at: Option<String>,
    pub parser_version: Option<String>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct SourceMetadataSessionExportV1 {
    pub schema: String,
    pub exported_at: String,
    pub source_session: SourceMetadataSessionRow,
    pub chunks: Vec<ActivityChunkDto>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct SourceMetadataSessionRow {
    pub source_id: String,
    pub app_id: String,
    pub external_session_id: String,
    pub title: Option<String>,
    pub name: Option<String>,
    pub source_path: Option<String>,
    pub source_uri: Option<String>,
    pub source_mtime: Option<String>,
    pub source_size: Option<u64>,
    pub source_fingerprint: Option<String>,
    pub parser_version: Option<String>,
    pub session_created_at: Option<String>,
    pub session_updated_at: Option<String>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub touched_files: Vec<String>,
    pub listable: bool,
    pub discovered_at: String,
    pub last_seen_at: String,
    pub metadata_json: Option<Value>,
}

#[derive(Debug, Serialize, PartialEq)]
pub struct HistoryFileSessionBlameResponse {
    pub schema: String,
    pub file_path: String,
    pub source: String,
    pub limit: usize,
    pub status: String,
    pub truncated: bool,
    pub errors: Vec<String>,
    pub rows: Vec<FileSessionBlameRow>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistoryDoctorResponse {
    pub source: String,
    pub metadata_db_path: Option<String>,
    pub selected_profile: Option<String>,
    pub rows: Vec<HistoryDoctorRow>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistoryDoctorRow {
    pub source_id: String,
    pub status: String,
    pub provider_kind: String,
    pub parser_kind: String,
    pub profile: DoctorProfileDiagnostic,
    pub configured_paths: Vec<DoctorPathDiagnostic>,
    pub provider_metadata: DoctorProviderMetadataDiagnostic,
    pub indexed_counts: DoctorIndexedCounts,
    pub notes: Vec<String>,
    pub errors: Vec<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DoctorProfileDiagnostic {
    pub exists: bool,
    pub selected: bool,
    pub app_id: Option<String>,
    pub actor_id: Option<String>,
    pub actor_type: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DoctorPathDiagnostic {
    pub field: String,
    pub required: bool,
    pub configured: bool,
    pub path: Option<String>,
    pub exists: Option<bool>,
    pub readable: Option<bool>,
    pub kind: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DoctorProviderMetadataDiagnostic {
    pub status: String,
    pub session_rows: Option<usize>,
    pub plan_rows: Option<usize>,
    pub error: Option<String>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct DoctorIndexedCounts {
    pub source_sessions: Option<usize>,
    pub source_plans: Option<usize>,
    pub error: Option<String>,
}

/// Executes read-only history subcommands and emits machine-readable JSON.
pub fn handle_history(
    command: HistoryCommand,
    profiles: &SourceProfileStore,
    store: &LocalStore,
) -> Result<()> {
    match command {
        HistoryCommand::Sources { format } => {
            ensure_json(format);
            print_json(&build_sources_response(profiles)?)
        }
        HistoryCommand::Live {
            source,
            limit,
            window_secs,
            format,
        } => {
            ensure_json(format);
            let selected_profiles = if source == "all" {
                profiles.list_profiles()?
            } else {
                vec![read_profile(profiles, &source)?]
            };
            print_json(&build_live_response(
                &selected_profiles,
                &source,
                limit,
                window_secs,
            )?)
        }
        HistoryCommand::Sessions {
            source,
            limit,
            offset,
            format,
        } => {
            ensure_json(format);
            let profile = read_profile(profiles, &source)?;
            let mut metadata_db = MetadataDb::open_global()?;
            let stats = refresh_profiles_to_metadata(
                &mut metadata_db,
                std::slice::from_ref(&profile),
                SOURCE_INDEX_REFRESH_LIMIT,
            )?;
            eprint_refresh_stats(&profile.name, stats);
            print_json(&build_sessions_response(
                &metadata_db,
                &profile,
                limit,
                offset,
            )?)
        }
        HistoryCommand::Plans {
            source,
            limit,
            offset,
            format,
        } => {
            ensure_json(format);
            let profile = read_profile(profiles, &source)?;
            let mut metadata_db = MetadataDb::open_global()?;
            refresh_profiles_to_metadata(&mut metadata_db, std::slice::from_ref(&profile), 0)?;
            print_json(&build_plans_response(
                &metadata_db,
                &profile,
                limit,
                offset,
            )?)
        }
        HistoryCommand::RecentPaths {
            source,
            limit,
            format,
        } => {
            ensure_json(format);
            let source_label = source.clone();
            let selected_profiles = if source == "all" {
                profiles.list_profiles()?
            } else {
                vec![read_profile(profiles, &source)?]
            };
            let mut metadata_db = MetadataDb::open_global()?;
            refresh_profiles_to_metadata(
                &mut metadata_db,
                &selected_profiles,
                SOURCE_INDEX_REFRESH_LIMIT,
            )?;
            print_json(&build_recent_paths_response(
                &metadata_db,
                source_label,
                &selected_profiles,
                limit,
            )?)
        }
        HistoryCommand::Doctor { source, format } => {
            ensure_json(format);
            print_json(&build_doctor_response(profiles, &source)?)
        }
        HistoryCommand::Chunks {
            source,
            session_id,
            limit,
            offset,
            max_field_bytes,
            format,
        } => {
            ensure_json(format);
            let profile = read_profile(profiles, &source)?;
            let response = build_chunks_response(
                &profile,
                &source,
                &session_id,
                limit,
                offset,
                max_field_bytes,
            )?;
            print_json(&response)
        }
        HistoryCommand::Export {
            source,
            session_id,
            schema,
            format,
        } => {
            let profile = read_profile(profiles, &source)?;
            let mut metadata_db = MetadataDb::open_global()?;
            refresh_profiles_to_metadata(
                &mut metadata_db,
                std::slice::from_ref(&profile),
                EXPORT_REFRESH_LIMIT,
            )?;
            let record = metadata_db
                .get_source_session(&profile.name, &session_id)?
                .ok_or_else(|| {
                    anyhow!("source session not found: {}/{}", profile.name, session_id)
                })?;
            let chunks = format_chunks_for_record(&record)?;
            print_export(schema, format, record, chunks)
        }
        HistoryCommand::FileSessionBlame {
            path,
            source,
            limit,
            format,
        } => {
            ensure_json(format);
            print_json(&build_file_session_blame_response(
                store, profiles, &path, &source, limit,
            )?)
        }
        HistoryCommand::Link {
            brick_session,
            source,
            session_id,
            format,
        } => {
            ensure_json(format);
            let profile = read_profile(profiles, &source)?;
            let mut metadata_db = MetadataDb::open_global()?;
            refresh_profiles_to_metadata(
                &mut metadata_db,
                std::slice::from_ref(&profile),
                EXPORT_REFRESH_LIMIT,
            )?;
            let source_session_id = metadata_db
                .get_source_session_id(&profile.name, &session_id)?
                .ok_or_else(|| {
                    anyhow!("source session not found: {}/{}", profile.name, session_id)
                })?;
            metadata_db.link_brick_session_to_source_session(&brick_session, source_session_id)?;
            print_json(&json!({
                "brick_session_id": brick_session,
                "source_id": profile.name,
                "external_session_id": session_id,
                "linked": true,
            }))
        }
        HistoryCommand::Linked {
            brick_session,
            format,
        } => {
            ensure_json(format);
            let metadata_db = MetadataDb::open_global()?;
            let pairs = metadata_db.list_source_sessions_for_brick_session(&brick_session)?;
            let sessions: Vec<Value> = pairs
                .into_iter()
                .map(|(source_id, external_session_id)| {
                    json!({
                        "source_id": source_id,
                        "external_session_id": external_session_id,
                    })
                })
                .collect();
            print_json(&json!({
                "brick_session_id": brick_session,
                "sessions": sessions,
            }))
        }
    }
}

pub(crate) fn build_file_session_blame_response(
    store: &LocalStore,
    profiles: &SourceProfileStore,
    file_path: &str,
    source: &str,
    limit: usize,
) -> Result<HistoryFileSessionBlameResponse> {
    let mut rows = Vec::new();
    let mut errors = Vec::new();
    let query_limit = limit.max(1) + 1;

    match store.rebuild_sqlite_index() {
        Ok(_) => match query_sqlite_file_session_blame(
            &store.sqlite_index_path(),
            &SqliteFileSessionBlameQuery {
                file_path: file_path.to_string(),
                limit: query_limit,
            },
        ) {
            Ok(runtime_rows) => rows.extend(runtime_rows),
            Err(error) => errors.push(format!("runtime_index_query: {error}")),
        },
        Err(error) => errors.push(format!("runtime_index_rebuild: {error}")),
    }

    let selected_profiles = if source == "all" {
        profiles.list_profiles()?
    } else {
        vec![read_profile(profiles, source)?]
    };
    let mut metadata_db = match MetadataDb::open_global() {
        Ok(metadata_db) => Some(metadata_db),
        Err(error) => {
            errors.push(format!("source_metadata_open: {error}"));
            None
        }
    };
    if let Some(metadata_db) = metadata_db.as_mut() {
        match refresh_profiles_to_metadata(metadata_db, &selected_profiles, EXPORT_REFRESH_LIMIT) {
            Ok(_) => {
                let query_source = (source != "all").then(|| source.to_string());
                match metadata_db.query_source_file_session_blame(&SourceFileSessionBlameQuery {
                    file_path: file_path.to_string(),
                    source_id: query_source,
                    repo_path: Some(store.repo_root().to_path_buf()),
                    limit: query_limit,
                }) {
                    Ok(source_rows) => rows.extend(source_rows),
                    Err(error) => errors.push(format!("source_metadata_query: {error}")),
                }
            }
            Err(error) => errors.push(format!("source_metadata_refresh: {error}")),
        }
    }

    rows.sort_by(|left, right| {
        right
            .last_seen_at
            .cmp(&left.last_seen_at)
            .then_with(|| left.file_path.cmp(&right.file_path))
            .then_with(|| left.session_id.cmp(&right.session_id))
            .then_with(|| left.external_session_id.cmp(&right.external_session_id))
            .then_with(|| {
                left.evidence_kind
                    .as_str()
                    .cmp(right.evidence_kind.as_str())
            })
    });
    let result_limit = limit.max(1);
    let truncated = rows.len() > result_limit;
    rows.truncate(result_limit);
    let status = if errors.is_empty() {
        if rows.is_empty() {
            "empty"
        } else {
            "ok"
        }
    } else {
        "error"
    }
    .to_string();

    Ok(HistoryFileSessionBlameResponse {
        schema: "file-session-blame-v1".to_string(),
        file_path: file_path.to_string(),
        source: source.to_string(),
        limit,
        status,
        truncated,
        errors,
        rows,
    })
}

fn build_sources_response(profiles: &SourceProfileStore) -> Result<HistorySourcesResponse> {
    let selected = profiles.selected_profile_name()?;
    let sources = profiles
        .list_profiles()?
        .into_iter()
        .map(|profile| source_row(profile, selected.as_deref()))
        .collect();
    Ok(HistorySourcesResponse { sources })
}

fn build_sessions_response(
    metadata_db: &MetadataDb,
    profile: &SourceProfile,
    limit: usize,
    offset: usize,
) -> Result<HistorySessionsResponse> {
    let records = metadata_db.list_source_sessions(&SourceSessionListQuery {
        source_id: Some(profile.name.clone()),
        limit,
        offset,
    })?;
    let total = metadata_db.count_source_sessions(Some(&profile.name))?;
    let sessions = records.into_iter().map(session_row).collect::<Vec<_>>();
    let has_more = offset.saturating_add(sessions.len()) < total;
    Ok(HistorySessionsResponse {
        source_id: profile.name.clone(),
        limit,
        offset,
        total,
        has_more,
        sessions,
    })
}

/// JSON DTO for `brick history live`.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistoryLiveResponse {
    pub source_id: String,
    pub window_secs: u64,
    pub count: usize,
    pub sessions: Vec<LiveSessionRow>,
}

/// One currently-active session, with the resolved work scope and a short
/// "what it is doing" summary for cross-session awareness.
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LiveSessionRow {
    pub source_id: String,
    pub app_id: String,
    pub external_session_id: String,
    pub title: Option<String>,
    pub path: String,
    /// Resolved work scope (git repo root or cwd); `None` when too shallow.
    pub work_scope: Option<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub last_activity: Option<String>,
    pub touched_files: Vec<String>,
}

/// Scans the given profiles, returning only sessions whose liveness is `Active`,
/// most-recent first. Shared by the `history live` CLI and the MCP `live_sessions`
/// tool so both report identical state. Probe failures on one source are skipped,
/// not fatal — one unreadable source must not blind the others.
pub(crate) fn collect_live_sessions(
    profiles: &[SourceProfile],
    window_secs: u64,
    limit: usize,
) -> Vec<NativeSourceSession> {
    let _ = window_secs; // window is enforced inside the core liveness probe.
    let mut live: Vec<NativeSourceSession> = Vec::new();
    for profile in profiles {
        let Ok(sessions) = list_source_sessions(profile, Some(limit.max(50))) else {
            continue;
        };
        live.extend(sessions.into_iter().filter(brick_core::is_active));
    }
    live.sort_by_key(|session| std::cmp::Reverse(session.last_activity));
    live.truncate(limit);
    live
}

fn build_live_response(
    profiles: &[SourceProfile],
    source_label: &str,
    limit: usize,
    window_secs: u64,
) -> Result<HistoryLiveResponse> {
    let live = collect_live_sessions(profiles, window_secs, limit);
    let sessions = live.iter().map(live_session_row).collect::<Vec<_>>();
    Ok(HistoryLiveResponse {
        source_id: source_label.to_string(),
        window_secs,
        count: sessions.len(),
        sessions,
    })
}

/// Projects a live `NativeSourceSession` into its JSON row, resolving work scope.
pub(crate) fn live_session_row(session: &NativeSourceSession) -> LiveSessionRow {
    LiveSessionRow {
        source_id: session.source_app_id.clone(),
        app_id: session.source_app_id.clone(),
        external_session_id: session.external_session_id.clone(),
        title: session.title.clone(),
        path: session.path.display().to_string(),
        work_scope: brick_core::work_scope(session).map(|path| path.display().to_string()),
        repo_path: display_path(session.repo_path.clone()),
        branch: session.branch.clone(),
        last_activity: session.last_activity.map(system_time_to_rfc3339),
        touched_files: session.touched_files.clone(),
    }
}

/// Formats a `SystemTime` as an RFC3339 UTC string.
fn system_time_to_rfc3339(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339()
}

/// A tiered cross-session awareness notice for a file the caller is about to
/// touch. Tier 1 = another live session touched the *same file*; Tier 2 =
/// another live session is active in the *same work scope* (different file).
#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LiveBroadcast {
    /// "file" (same file collision) or "scope" (same work scope, different file).
    pub tier: &'static str,
    pub message: String,
    pub sessions: Vec<LiveSessionRow>,
}

/// Builds the tiered live broadcast for `path`, excluding the caller's own
/// session(s) by path identity. Returns `None` when no live session overlaps —
/// the common case, so recall stays quiet unless there is real contention.
///
/// `self_path` is the transcript path of the calling session when known, so a
/// session never warns about itself.
pub(crate) fn build_live_broadcast(
    profiles: &[SourceProfile],
    target_path: &str,
    self_path: Option<&str>,
) -> Option<LiveBroadcast> {
    let live = collect_live_sessions(profiles, 0, 50);
    let target = PathBuf::from(target_path);
    let target_name = target.file_name().map(|name| name.to_owned());

    let mut file_hits: Vec<LiveSessionRow> = Vec::new();
    let mut scope_hits: Vec<LiveSessionRow> = Vec::new();

    for session in &live {
        if self_path.is_some_and(|own| own == session.path.to_string_lossy()) {
            continue;
        }
        // Tier 1: did this session touch the same file (by full path or basename)?
        let touched_same = session.touched_files.iter().any(|touched| {
            touched == target_path
                || PathBuf::from(touched)
                    .file_name()
                    .map(|name| name.to_owned())
                    == target_name
        });
        if touched_same {
            file_hits.push(live_session_row(session));
            continue;
        }
        // Tier 2: is this session active in the same work scope as the target?
        if let Some(scope) = brick_core::work_scope(session) {
            if target.starts_with(&scope) {
                scope_hits.push(live_session_row(session));
            }
        }
    }

    if !file_hits.is_empty() {
        let who = describe_sessions(&file_hits);
        return Some(LiveBroadcast {
            tier: "file",
            message: format!(
                "⚠️ {} active session(s) recently changed this same file: {who}. \
                 Coordinate or re-check the file before editing.",
                file_hits.len()
            ),
            sessions: file_hits,
        });
    }
    if !scope_hits.is_empty() {
        let who = describe_sessions(&scope_hits);
        return Some(LiveBroadcast {
            tier: "scope",
            message: format!(
                "ℹ️ {} active session(s) are working in this same project right now: {who}. \
                 Your view of the code here may change under you.",
                scope_hits.len()
            ),
            sessions: scope_hits,
        });
    }
    None
}

/// Renders a short "who + what" phrase from live session rows for the broadcast
/// message — uses each session's title as the "what it is doing" summary.
fn describe_sessions(rows: &[LiveSessionRow]) -> String {
    rows.iter()
        .take(3)
        .map(|row| {
            let what = row.title.as_deref().unwrap_or("(no title)");
            format!("{} \"{}\"", row.app_id, what)
        })
        .collect::<Vec<_>>()
        .join("; ")
}

fn build_plans_response(
    metadata_db: &MetadataDb,
    profile: &SourceProfile,
    limit: usize,
    offset: usize,
) -> Result<HistoryPlansResponse> {
    let records = metadata_db.list_source_plans(&SourcePlanListQuery {
        source_id: Some(profile.name.clone()),
        limit,
        offset,
    })?;
    let total = metadata_db.count_source_plans(Some(&profile.name))?;
    let external_plan_ids = records
        .iter()
        .map(|record| record.external_plan_id.clone())
        .collect::<Vec<_>>();
    let edges = if external_plan_ids.is_empty() {
        Vec::new()
    } else {
        metadata_db
            .list_source_plan_session_edges(Some(&profile.name), &external_plan_ids)?
            .into_iter()
            .map(plan_session_edge_row)
            .collect::<Vec<_>>()
    };
    let plans = records.into_iter().map(plan_row).collect::<Vec<_>>();
    let has_more = offset.saturating_add(plans.len()) < total;
    Ok(HistoryPlansResponse {
        source_id: profile.name.clone(),
        limit,
        offset,
        total,
        has_more,
        plans,
        edges,
    })
}

fn build_recent_paths_response(
    metadata_db: &MetadataDb,
    source_id: String,
    profiles: &[SourceProfile],
    limit: usize,
) -> Result<HistoryRecentPathsResponse> {
    let query_source = if source_id == "all" {
        None
    } else {
        Some(source_id.clone())
    };
    let configured_sources = profiles
        .iter()
        .map(|profile| profile.name.as_str())
        .collect::<Vec<_>>();
    let paths = metadata_db
        .list_source_sessions(&SourceSessionListQuery {
            source_id: query_source,
            limit,
            offset: 0,
        })?
        .into_iter()
        .filter(|record| configured_sources.contains(&record.source_id.as_str()))
        .map(recent_path_row)
        .collect();
    Ok(HistoryRecentPathsResponse {
        source_id,
        limit,
        paths,
    })
}

fn build_doctor_response(
    profiles: &SourceProfileStore,
    source: &str,
) -> Result<HistoryDoctorResponse> {
    let selected_profile = profiles.selected_profile_name()?;
    let rows = doctor_profiles(profiles, source, selected_profile.as_deref())?
        .into_iter()
        .map(|(profile, exists)| build_doctor_row(profile, exists, selected_profile.as_deref()))
        .collect();
    Ok(HistoryDoctorResponse {
        source: source.to_string(),
        metadata_db_path: metadata_db_path()
            .map(|path| path.display().to_string())
            .ok(),
        selected_profile,
        rows,
    })
}

fn doctor_profiles(
    profiles: &SourceProfileStore,
    source: &str,
    selected: Option<&str>,
) -> Result<Vec<(SourceProfile, bool)>> {
    if source == "all" {
        return profiles.list_profiles().map(|profiles| {
            profiles
                .into_iter()
                .map(|profile| (profile, true))
                .collect()
        });
    }
    match profiles.read_profile(source)? {
        Some(profile) => Ok(vec![(profile, true)]),
        None => Ok(vec![(missing_source_profile(source, selected), false)]),
    }
}

fn missing_source_profile(source: &str, selected: Option<&str>) -> SourceProfile {
    SourceProfile {
        name: source.to_string(),
        app_id: None,
        actor_id: None,
        actor_type: None,
        store_root: None,
        session_db_path: None,
        session_log_path: None,
        evidence_root: None,
        cursor_state_db_path: None,
        default_full_evidence_upload: None,
        notes: selected.map(|name| format!("selected profile is {name}")),
    }
}

fn build_doctor_row(
    profile: SourceProfile,
    profile_exists: bool,
    selected: Option<&str>,
) -> HistoryDoctorRow {
    let provider_kind = provider_kind(&profile.name).to_string();
    let parser_kind = parser_kind(&profile.name).to_string();
    let configured_paths = doctor_path_diagnostics(&profile);
    let mut errors = configured_paths
        .iter()
        .filter_map(|path| path.error.clone())
        .collect::<Vec<_>>();
    if !profile_exists {
        errors.push(format!("source profile not found: {}", profile.name));
    }
    let provider_metadata = if profile_exists {
        doctor_provider_metadata(&profile)
    } else {
        DoctorProviderMetadataDiagnostic {
            status: "skipped".to_string(),
            session_rows: None,
            plan_rows: None,
            error: Some("profile missing".to_string()),
        }
    };
    if let Some(error) = &provider_metadata.error {
        errors.push(error.clone());
    }
    let indexed_counts = doctor_indexed_counts(&profile.name);
    let mut notes = provider_notes(&profile);
    if !profile_exists {
        notes.push(
            "run `brick source scan --write-defaults` or `brick source configure`".to_string(),
        );
    }
    let status = if errors.is_empty() { "ok" } else { "error" }.to_string();

    HistoryDoctorRow {
        source_id: profile.name.clone(),
        status,
        provider_kind,
        parser_kind,
        profile: DoctorProfileDiagnostic {
            exists: profile_exists,
            selected: selected == Some(profile.name.as_str()),
            app_id: profile.app_id,
            actor_id: profile.actor_id,
            actor_type: profile
                .actor_type
                .map(format_actor_type)
                .map(str::to_string),
        },
        configured_paths,
        provider_metadata,
        indexed_counts,
        notes,
        errors,
    }
}

fn doctor_provider_metadata(profile: &SourceProfile) -> DoctorProviderMetadataDiagnostic {
    let sessions = list_source_sessions(profile, Some(1));
    let plans = list_source_plans(profile);
    match (sessions, plans) {
        (Ok(sessions), Ok(plans)) => DoctorProviderMetadataDiagnostic {
            status: "ok".to_string(),
            session_rows: Some(sessions.len()),
            plan_rows: Some(plans.len()),
            error: None,
        },
        (session_result, plan_result) => DoctorProviderMetadataDiagnostic {
            status: "error".to_string(),
            session_rows: session_result.as_ref().ok().map(Vec::len),
            plan_rows: plan_result.as_ref().ok().map(Vec::len),
            error: Some(
                [session_result.err(), plan_result.err()]
                    .into_iter()
                    .flatten()
                    .map(|error| error.to_string())
                    .collect::<Vec<_>>()
                    .join("; "),
            ),
        },
    }
}

fn doctor_indexed_counts(source_id: &str) -> DoctorIndexedCounts {
    match MetadataDb::open_global() {
        Ok(metadata_db) => DoctorIndexedCounts {
            source_sessions: metadata_db.count_source_sessions(Some(source_id)).ok(),
            source_plans: metadata_db.count_source_plans(Some(source_id)).ok(),
            error: None,
        },
        Err(error) => DoctorIndexedCounts {
            source_sessions: None,
            source_plans: None,
            error: Some(error.to_string()),
        },
    }
}

fn doctor_path_diagnostics(profile: &SourceProfile) -> Vec<DoctorPathDiagnostic> {
    source_path_specs(profile)
        .into_iter()
        .map(|(field, required, path)| doctor_path_diagnostic(field, required, path))
        .collect()
}

fn doctor_path_diagnostic(
    field: &str,
    required: bool,
    path: Option<&Path>,
) -> DoctorPathDiagnostic {
    let Some(path) = path else {
        return DoctorPathDiagnostic {
            field: field.to_string(),
            required,
            configured: false,
            path: None,
            exists: None,
            readable: None,
            kind: None,
            error: required.then(|| format!("required source path not configured: {field}")),
        };
    };
    let metadata = fs::metadata(path);
    let (exists, readable, kind, error) = match metadata {
        Ok(metadata) => {
            let kind = if metadata.is_dir() {
                Some("directory".to_string())
            } else if metadata.is_file() {
                Some("file".to_string())
            } else {
                Some("other".to_string())
            };
            (Some(true), Some(path_readable(path, &metadata)), kind, None)
        }
        Err(error) => (
            Some(false),
            Some(false),
            None,
            Some(format!(
                "failed to read {field} at {}: {error}",
                path.display()
            )),
        ),
    };
    DoctorPathDiagnostic {
        field: field.to_string(),
        required,
        configured: true,
        path: Some(path.display().to_string()),
        exists,
        readable,
        kind,
        error,
    }
}

fn path_readable(path: &Path, metadata: &fs::Metadata) -> bool {
    if metadata.is_dir() {
        fs::read_dir(path).is_ok()
    } else {
        fs::File::open(path).is_ok()
    }
}

fn source_path_specs(profile: &SourceProfile) -> Vec<(&'static str, bool, Option<&Path>)> {
    match profile.name.as_str() {
        "cursor_ide" | "windsurf" => vec![
            (
                "cursor_state_db_path",
                true,
                profile.cursor_state_db_path.as_deref(),
            ),
            ("session_db_path", false, profile.session_db_path.as_deref()),
        ],
        "opencode" => vec![
            ("session_db_path", true, profile.session_db_path.as_deref()),
            (
                "session_log_path",
                false,
                profile.session_log_path.as_deref(),
            ),
            ("evidence_root", false, profile.evidence_root.as_deref()),
        ],
        "claude_code" | "codex_app" => vec![
            (
                "session_log_path",
                true,
                profile.session_log_path.as_deref(),
            ),
            ("evidence_root", false, profile.evidence_root.as_deref()),
        ],
        _ => vec![
            (
                "session_log_path",
                false,
                profile.session_log_path.as_deref(),
            ),
            ("session_db_path", false, profile.session_db_path.as_deref()),
            ("evidence_root", false, profile.evidence_root.as_deref()),
            (
                "cursor_state_db_path",
                false,
                profile.cursor_state_db_path.as_deref(),
            ),
        ],
    }
}

fn provider_kind(source_id: &str) -> &'static str {
    match source_id {
        "claude_code" => "claude_code_jsonl",
        "codex_app" => "codex_app_jsonl",
        "cursor_ide" => "cursor_family_sqlite",
        "windsurf" => "cursor_family_sqlite",
        "opencode" => "opencode_sqlite",
        _ => "generic_native_files",
    }
}

fn parser_kind(source_id: &str) -> &'static str {
    match source_id {
        "claude_code" => "claude-code-jsonl-v4",
        "codex_app" => "codex-app-jsonl-v4",
        "cursor_ide" => "cursor-ide-composer-headers-v1",
        "windsurf" => "windsurf-composer-data-v1",
        "opencode" => "opencode-sqlite-v1",
        "orgii" => "orgii-sqlite-v2",
        "gemini" => "gemini-chat-json-v1",
        _ => "native-file-v1",
    }
}

fn provider_notes(profile: &SourceProfile) -> Vec<String> {
    let mut notes = Vec::new();
    match profile.name.as_str() {
        "cursor_ide" => notes.push("requires Cursor state.vscdb with composer headers".to_string()),
        "windsurf" => notes.push("requires Windsurf state.vscdb composerData rows".to_string()),
        "opencode" => notes.push("requires OpenCode opencode.db session table".to_string()),
        "claude_code" => notes.push("scans Claude Code JSONL transcript files".to_string()),
        "codex_app" => notes.push("scans Codex App JSONL session files".to_string()),
        _ => notes.push("uses generic native file listing".to_string()),
    }
    if let Some(note) = &profile.notes {
        notes.push(note.clone());
    }
    notes
}

fn print_export(
    schema: HistoryExportSchemaArg,
    format: HistoryExportFormatArg,
    record: SourceSessionRecord,
    chunks: Vec<ActivityChunkDto>,
) -> Result<()> {
    match (schema, format) {
        (HistoryExportSchemaArg::AuditV1, HistoryExportFormatArg::Json) => {
            print_json(&build_audit_export(record, chunks))
        }
        (HistoryExportSchemaArg::SourceMetadataV1, HistoryExportFormatArg::Json) => {
            print_json(&build_source_metadata_export(record, chunks))
        }
        (_, HistoryExportFormatArg::Csv) => print_export_csv(&record, &chunks),
    }
}

fn print_export_csv(record: &SourceSessionRecord, chunks: &[ActivityChunkDto]) -> Result<()> {
    let rows = if chunks.is_empty() {
        vec![export_csv_row(record, None)]
    } else {
        chunks
            .iter()
            .map(|chunk| export_csv_row(record, Some(chunk)))
            .collect::<Vec<_>>()
    };
    println!(
        "source_id,app_id,external_session_id,title,model,created_at,updated_at,repo_path,branch,input_tokens,output_tokens,total_tokens,files_changed,lines_added,lines_removed,touched_files,source_path,chunk_id,chunk_created_at,chunk_action_type,chunk_function,chunk_source_id,chunk_source_path,source_record_key,source_line_number,source_message_id,source_part_id,chunk_args_json,chunk_result_json"
    );
    for row in rows {
        println!("{}", row.join(","));
    }
    Ok(())
}

fn export_csv_row(record: &SourceSessionRecord, chunk: Option<&ActivityChunkDto>) -> Vec<String> {
    let touched_files = touched_files_from_record(record).join(";");
    let repo_path = record
        .repo_path
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let source_path = source_path_display(record);
    let chunk_args_json = chunk
        .map(|chunk| chunk.args.to_string())
        .unwrap_or_default();
    let chunk_result_json = chunk
        .map(|chunk| chunk.result.to_string())
        .unwrap_or_default();
    [
        record.source_id.clone(),
        app_id_from_metadata(record),
        record.external_session_id.clone(),
        record.title.clone().unwrap_or_default(),
        record.model.clone().unwrap_or_default(),
        record
            .session_created_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_default(),
        record
            .session_updated_at
            .map(|time| time.to_rfc3339())
            .unwrap_or_default(),
        repo_path,
        record.branch.clone().unwrap_or_default(),
        optional_u64_csv(record.input_tokens),
        optional_u64_csv(record.output_tokens),
        optional_u64_csv(total_tokens(record.input_tokens, record.output_tokens)),
        optional_u64_csv(record.files_changed),
        optional_u64_csv(record.lines_added),
        optional_u64_csv(record.lines_removed),
        touched_files,
        source_path,
        chunk
            .map(|chunk| chunk.chunk_id.clone())
            .unwrap_or_default(),
        chunk
            .map(|chunk| chunk.created_at.clone())
            .unwrap_or_default(),
        chunk
            .map(|chunk| chunk.action_type.clone())
            .unwrap_or_default(),
        chunk
            .map(|chunk| chunk.function.clone())
            .unwrap_or_default(),
        chunk
            .and_then(|chunk| chunk.source_id.clone())
            .unwrap_or_default(),
        chunk
            .and_then(|chunk| chunk.source_path.clone())
            .unwrap_or_default(),
        chunk
            .and_then(|chunk| chunk.source_record_key.clone())
            .unwrap_or_default(),
        chunk
            .and_then(|chunk| chunk.source_line_number)
            .map(|line_number| line_number.to_string())
            .unwrap_or_default(),
        chunk
            .and_then(|chunk| chunk.source_message_id.clone())
            .unwrap_or_default(),
        chunk
            .and_then(|chunk| chunk.source_part_id.clone())
            .unwrap_or_default(),
        chunk_args_json,
        chunk_result_json,
    ]
    .into_iter()
    .map(csv_cell)
    .collect()
}

fn optional_u64_csv(value: Option<u64>) -> String {
    value.map(|number| number.to_string()).unwrap_or_default()
}

fn csv_cell(value: String) -> String {
    let escaped = value.replace('"', "\"\"");
    if escaped.contains(',') || escaped.contains('\n') || escaped.contains('"') {
        format!("\"{escaped}\"")
    } else {
        escaped
    }
}

fn build_audit_export(
    record: SourceSessionRecord,
    chunks: Vec<ActivityChunkDto>,
) -> AuditSessionExportV1 {
    let exported_at = Utc::now().to_rfc3339();
    let app_id = app_id_from_metadata(&record);
    let touched_files = touched_files_from_record(&record);
    let source_path = source_path_display(&record);
    let repo_path = record
        .repo_path
        .as_ref()
        .map(|path| path.display().to_string());
    let total_tokens = total_tokens(record.input_tokens, record.output_tokens);
    AuditSessionExportV1 {
        schema: HISTORY_EXPORT_SCHEMA_AUDIT_V1.to_string(),
        exported_at,
        source: AuditSourceRef {
            source_id: record.source_id.clone(),
            app_id,
        },
        session: AuditSessionMetadata {
            session_id: record.external_session_id.clone(),
            external_session_id: record.external_session_id,
            title: record.title,
            model: record.model,
            created_at: record.session_created_at.map(|time| time.to_rfc3339()),
            updated_at: record.session_updated_at.map(|time| time.to_rfc3339()),
            repo_path,
            branch: record.branch,
        },
        token_usage: AuditTokenUsage {
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            total_tokens,
        },
        impact: AuditImpact {
            files_changed: record.files_changed,
            lines_added: record.lines_added,
            lines_removed: record.lines_removed,
            touched_files,
        },
        evidence: AuditEvidenceRef {
            availability: EXPORT_AVAILABILITY_METADATA_ONLY.to_string(),
            source_path,
            source_uri: record.source_uri,
            source_size_bytes: record.source_size,
            source_modified_at: record.source_mtime.map(|time| time.to_rfc3339()),
            parser_version: record.parser_version,
        },
        chunks,
        source_metadata: record.metadata_json,
    }
}

fn build_source_metadata_export(
    record: SourceSessionRecord,
    chunks: Vec<ActivityChunkDto>,
) -> SourceMetadataSessionExportV1 {
    let exported_at = Utc::now().to_rfc3339();
    let app_id = app_id_from_metadata(&record);
    let touched_files = touched_files_from_record(&record);
    let source_path = record
        .source_path
        .as_ref()
        .map(|path| path.display().to_string());
    let repo_path = record
        .repo_path
        .as_ref()
        .map(|path| path.display().to_string());
    let total_tokens = total_tokens(record.input_tokens, record.output_tokens);
    SourceMetadataSessionExportV1 {
        schema: HISTORY_EXPORT_SCHEMA_SOURCE_METADATA_V1.to_string(),
        exported_at,
        source_session: SourceMetadataSessionRow {
            source_id: record.source_id,
            app_id,
            external_session_id: record.external_session_id,
            title: record.title,
            name: record.name,
            source_path,
            source_uri: record.source_uri,
            source_mtime: record.source_mtime.map(|time| time.to_rfc3339()),
            source_size: record.source_size,
            source_fingerprint: record.source_fingerprint,
            parser_version: record.parser_version,
            session_created_at: record.session_created_at.map(|time| time.to_rfc3339()),
            session_updated_at: record.session_updated_at.map(|time| time.to_rfc3339()),
            model: record.model,
            input_tokens: record.input_tokens,
            output_tokens: record.output_tokens,
            total_tokens,
            repo_path,
            branch: record.branch,
            files_changed: record.files_changed,
            lines_added: record.lines_added,
            lines_removed: record.lines_removed,
            touched_files,
            listable: record.listable,
            discovered_at: record.discovered_at.to_rfc3339(),
            last_seen_at: record.last_seen_at.to_rfc3339(),
            metadata_json: record.metadata_json,
        },
        chunks,
    }
}

fn format_chunks_for_record(record: &SourceSessionRecord) -> Result<Vec<ActivityChunkDto>> {
    format_source_session_chunks(
        &record.source_id,
        &record.external_session_id,
        record.source_path.as_deref(),
    )
}

fn source_row(profile: SourceProfile, selected: Option<&str>) -> HistorySourceRow {
    HistorySourceRow {
        selected: selected == Some(profile.name.as_str()),
        source_id: profile.name,
        app_id: profile.app_id,
        actor_id: profile.actor_id,
        actor_type: profile
            .actor_type
            .map(format_actor_type)
            .map(str::to_string),
        store_root: display_path(profile.store_root),
        session_db_path: display_path(profile.session_db_path),
        session_log_path: display_path(profile.session_log_path),
        evidence_root: display_path(profile.evidence_root),
        cursor_state_db_path: display_path(profile.cursor_state_db_path),
        notes: profile.notes,
    }
}

/// Aggregate counters describing a metadata refresh pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct RefreshStats {
    scanned: usize,
    reindexed: usize,
    skipped: usize,
}

impl RefreshStats {
    fn merge(&mut self, other: RefreshStats) {
        self.scanned += other.scanned;
        self.reindexed += other.reindexed;
        self.skipped += other.skipped;
    }
}

fn eprint_refresh_stats(source_id: &str, stats: RefreshStats) {
    eprintln!(
        "source={source_id} scanned={} reindexed={} skipped={}",
        stats.scanned, stats.reindexed, stats.skipped
    );
}

pub(crate) fn refresh_profiles_to_metadata(
    metadata_db: &mut MetadataDb,
    profiles: &[SourceProfile],
    limit: usize,
) -> Result<RefreshStats> {
    let mut totals = RefreshStats::default();
    for profile in profiles {
        metadata_db.upsert_source_profile(&source_profile_upsert(profile))?;
        let scan_id = metadata_db.begin_source_scan(&profile.name)?;
        let profile_stats = refresh_single_profile(metadata_db, profile, limit);
        match &profile_stats {
            Ok(stats) => {
                metadata_db.finish_source_scan(
                    scan_id,
                    SourceScanStatus::Completed,
                    Some(&json!({
                        "scanned": stats.scanned,
                        "reindexed": stats.reindexed,
                        "skipped": stats.skipped,
                    })),
                )?;
                totals.merge(*stats);
            }
            Err(error) => {
                metadata_db.finish_source_scan(
                    scan_id,
                    SourceScanStatus::Error,
                    Some(&json!({ "error": error.to_string() })),
                )?;
            }
        }
        profile_stats?;
    }
    Ok(totals)
}

fn refresh_single_profile(
    metadata_db: &mut MetadataDb,
    profile: &SourceProfile,
    limit: usize,
) -> Result<RefreshStats> {
    let mut stats = RefreshStats::default();
    record_source_roots(metadata_db, profile)?;
    for session in list_source_sessions(profile, Some(limit))? {
        stats.scanned += 1;
        let repo_path = session
            .repo_path
            .as_ref()
            .map(|path| path.display().to_string());
        let upsert = source_session_upsert(profile, session);
        let existing =
            metadata_db.get_source_session(&upsert.source_id, &upsert.external_session_id)?;
        let unchanged = matches!(
            (&existing, &upsert.source_fingerprint),
            (Some(record), Some(fingerprint))
                if record.source_fingerprint.as_deref() == Some(fingerprint.as_str())
        );
        if unchanged {
            metadata_db.touch_source_session_last_seen(
                &upsert.source_id,
                &upsert.external_session_id,
                upsert.last_seen_at,
            )?;
            stats.skipped += 1;
        } else {
            metadata_db.upsert_source_session(&upsert)?;
            stats.reindexed += 1;
        }
        link_session_repo(
            metadata_db,
            &upsert.source_id,
            &upsert.external_session_id,
            repo_path.as_deref(),
        )?;
    }
    for plan in list_source_plans(profile)? {
        metadata_db.upsert_source_plan_with_edges(&plan)?;
    }
    Ok(stats)
}

/// Records the native scan roots a profile reads from into source_roots.
fn record_source_roots(metadata_db: &mut MetadataDb, profile: &SourceProfile) -> Result<()> {
    let roots = [
        profile.session_log_path.as_ref(),
        profile.evidence_root.as_ref(),
        profile.session_db_path.as_ref(),
        profile.cursor_state_db_path.as_ref(),
    ];
    for root in roots.into_iter().flatten() {
        let root_path = root.display().to_string();
        metadata_db.upsert_source_root(&profile.name, Some(&root_path), None)?;
    }
    Ok(())
}

/// Persists a source profile row and its configured scan roots into the global
/// metadata DB. Shared by `source configure` / `source scan --write-defaults`
/// so profile config side effects are not limited to history/import refreshes.
pub(crate) fn persist_profile_metadata(
    metadata_db: &mut MetadataDb,
    profile: &SourceProfile,
) -> Result<()> {
    metadata_db.upsert_source_profile(&source_profile_upsert(profile))?;
    record_source_roots(metadata_db, profile)?;
    Ok(())
}

/// Links a session's repo path into workspace_roots + git_repositories M:N tables.
fn link_session_repo(
    metadata_db: &mut MetadataDb,
    source_id: &str,
    external_session_id: &str,
    repo_path: Option<&str>,
) -> Result<()> {
    let Some(repo_path) = repo_path else {
        return Ok(());
    };
    let Some(source_session_id) =
        metadata_db.get_source_session_id(source_id, external_session_id)?
    else {
        return Ok(());
    };
    let workspace_root_id = metadata_db.upsert_workspace_root(repo_path, None)?;
    metadata_db.link_session_workspace_root(source_session_id, workspace_root_id)?;
    let git_repository_id = metadata_db.upsert_git_repository(Some(repo_path), None, None, None)?;
    metadata_db.link_session_git_repository(source_session_id, git_repository_id)?;
    Ok(())
}

fn source_profile_upsert(profile: &SourceProfile) -> SourceProfileUpsert {
    SourceProfileUpsert {
        source_id: profile.name.clone(),
        name: Some(profile.name.clone()),
        app_id: profile.app_id.clone(),
        actor_id: profile.actor_id.clone(),
        actor_type: profile
            .actor_type
            .map(format_actor_type)
            .map(str::to_string),
        profile_json: serde_json::to_value(profile).ok(),
    }
}

fn source_session_upsert(
    profile: &SourceProfile,
    session: NativeSourceSession,
) -> SourceSessionUpsert {
    let now = Utc::now();
    let source_mtime = session.modified_at.map(system_time_to_utc);
    let listable = session.listable;
    // Fold the parser version into the fingerprint so that upgrading a parser
    // (which changes what we extract, e.g. touched_files) invalidates already-
    // indexed rows and forces a re-parse, without the user having to remember a
    // manual reindex. mtime+size alone would skip unchanged files forever.
    let source_fingerprint = source_mtime.map(|mtime| {
        format!(
            "{}:{}:{}",
            mtime.to_rfc3339(),
            session.size_bytes,
            session.parser_version
        )
    });
    let metadata_json = source_session_metadata(profile, &session);
    SourceSessionUpsert {
        source_id: profile.name.clone(),
        external_session_id: session.external_session_id,
        title: session.title.clone(),
        name: session.title,
        source_path: Some(session.path.clone()),
        source_uri: Some(format!("file://{}", session.path.display())),
        source_mtime,
        source_size: Some(session.size_bytes),
        source_fingerprint,
        parser_version: Some(session.parser_version),
        session_created_at: session.session_created_at.map(system_time_to_utc),
        session_updated_at: session.session_updated_at.map(system_time_to_utc),
        model: session.model,
        input_tokens: session.input_tokens,
        output_tokens: session.output_tokens,
        repo_path: session.repo_path,
        branch: session.branch,
        files_changed: session.files_changed,
        lines_added: session.lines_added,
        lines_removed: session.lines_removed,
        touched_files_json: Some(json!(session.touched_files)),
        listable,
        discovered_at: now,
        last_seen_at: session
            .session_updated_at
            .map(system_time_to_utc)
            .or(source_mtime)
            .unwrap_or(now),
        metadata_json,
    }
}

fn source_session_metadata(
    profile: &SourceProfile,
    session: &NativeSourceSession,
) -> Option<Value> {
    let mut metadata = json!({
        "app_id": session.source_app_id,
        "actor_id": profile.actor_id,
        "actor_type": profile.actor_type.map(format_actor_type),
    });
    if let Some(provider_metadata) = session.metadata_json.as_ref() {
        if let (Some(metadata_object), Some(provider_object)) =
            (metadata.as_object_mut(), provider_metadata.as_object())
        {
            for (key, value) in provider_object {
                metadata_object.insert(key.clone(), value.clone());
            }
        } else {
            metadata["sourceMetadata"] = provider_metadata.clone();
        }
    }
    Some(metadata)
}

fn session_row(record: SourceSessionRecord) -> HistorySessionRow {
    let app_id = app_id_from_metadata(&record);
    let path = source_path_display(&record);
    let repo_path = record
        .repo_path
        .as_ref()
        .map(|path| path.display().to_string());
    let touched_files = touched_files_from_record(&record);
    let total_tokens = total_tokens(record.input_tokens, record.output_tokens);
    HistorySessionRow {
        source_id: record.source_id.clone(),
        app_id,
        session_id: record.external_session_id.clone(),
        external_session_id: record.external_session_id,
        title: record.title,
        path,
        size_bytes: record.source_size.unwrap_or_default(),
        modified_at: record.source_mtime.map(|time| time.to_rfc3339()),
        created_at: record.session_created_at.map(|time| time.to_rfc3339()),
        updated_at: record.session_updated_at.map(|time| time.to_rfc3339()),
        model: record.model,
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        total_tokens,
        repo_path,
        branch: record.branch,
        files_changed: record.files_changed,
        lines_added: record.lines_added,
        lines_removed: record.lines_removed,
        touched_files,
    }
}

fn recent_path_row(record: SourceSessionRecord) -> HistoryRecentPathRow {
    let app_id = app_id_from_metadata(&record);
    let path = source_path_display(&record);
    HistoryRecentPathRow {
        source_id: record.source_id.clone(),
        app_id,
        session_id: record.external_session_id,
        path,
        title: record.title,
        size_bytes: record.source_size.unwrap_or_default(),
        modified_at: record.source_mtime.map(|time| time.to_rfc3339()),
    }
}

fn plan_row(record: SourcePlanRecord) -> HistoryPlanRow {
    HistoryPlanRow {
        source_id: record.source_id.clone(),
        plan_id: record.external_plan_id.clone(),
        external_plan_id: record.external_plan_id,
        title: record.title,
        source_path: display_path(record.source_path),
        source_uri: record.source_uri,
        source_mtime: record.source_mtime.map(|time| time.to_rfc3339()),
        parser_version: record.parser_version,
        discovered_at: record.discovered_at.to_rfc3339(),
        last_seen_at: record.last_seen_at.to_rfc3339(),
        created_at: record.created_at.to_rfc3339(),
        updated_at: record.updated_at.to_rfc3339(),
        metadata_json: record.metadata_json,
    }
}

fn plan_session_edge_row(record: SourcePlanSessionEdgeRecord) -> HistoryPlanSessionEdgeRow {
    HistoryPlanSessionEdgeRow {
        source_id: record.source_id.clone(),
        plan_id: record.external_plan_id.clone(),
        external_plan_id: record.external_plan_id,
        external_session_id: record.external_session_id,
        role: record.role.as_str().to_string(),
        todo_ids_json: record.todo_ids_json,
        discovered_at: record.discovered_at.to_rfc3339(),
        last_seen_at: record.last_seen_at.to_rfc3339(),
        created_at: record.created_at.to_rfc3339(),
        updated_at: record.updated_at.to_rfc3339(),
        metadata_json: record.metadata_json,
    }
}

fn source_path_display(record: &SourceSessionRecord) -> String {
    record
        .source_path
        .as_ref()
        .map(|path| path.display().to_string())
        .or_else(|| record.source_uri.clone())
        .unwrap_or_default()
}

fn app_id_from_metadata(record: &SourceSessionRecord) -> String {
    record
        .metadata_json
        .as_ref()
        .and_then(Value::as_object)
        .and_then(|metadata| metadata.get("app_id"))
        .and_then(Value::as_str)
        .unwrap_or(&record.source_id)
        .to_string()
}

fn touched_files_from_record(record: &SourceSessionRecord) -> Vec<String> {
    record
        .touched_files_json
        .as_ref()
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

fn total_tokens(input_tokens: Option<u64>, output_tokens: Option<u64>) -> Option<u64> {
    match (input_tokens, output_tokens) {
        (None, None) => None,
        (input, output) => Some(
            input
                .unwrap_or_default()
                .saturating_add(output.unwrap_or_default()),
        ),
    }
}

pub(crate) fn read_profile(profiles: &SourceProfileStore, source: &str) -> Result<SourceProfile> {
    profiles
        .read_profile(source)?
        .ok_or_else(|| anyhow!("source profile not found: {source}"))
}

pub(crate) fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

pub(crate) fn ensure_json(format: HistoryFormatArg) {
    match format {
        HistoryFormatArg::Json => {}
    }
}

fn display_path(path: Option<PathBuf>) -> Option<String> {
    path.map(|path| path.display().to_string())
}

fn format_actor_type(actor_type: ActorType) -> &'static str {
    match actor_type {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}

#[cfg(test)]
fn format_system_time(time: SystemTime) -> Option<String> {
    time.duration_since(UNIX_EPOCH).ok()?;
    Some(system_time_to_utc(time).to_rfc3339())
}

fn system_time_to_utc(time: SystemTime) -> DateTime<Utc> {
    time.into()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

    use brick_core::{user_message_chunk, ACTION_TYPE_RAW, FUNCTION_USER_MESSAGE};
    use chrono::TimeZone;
    use serde_json::json;

    use super::*;

    #[test]
    fn truncate_string_keeps_short_values_via_value_walker() {
        let mut value = json!("short");
        truncate_value(&mut value, 100, 0);
        assert_eq!(value, json!("short"));
    }

    #[test]
    fn truncate_value_cuts_long_string_and_records_refetch() {
        let mut value = json!("x".repeat(5000));
        truncate_value(&mut value, 100, 7);
        let text = value.as_str().expect("string");
        assert!(text.len() < 5000);
        assert!(text.contains("full 5000 bytes"));
        assert!(text.contains("--offset 7 --limit 1 --max-field-bytes 0"));
    }

    #[test]
    fn truncate_value_descends_into_nested_objects_and_arrays() {
        let mut value = json!({
            "output": "y".repeat(3000),
            "nested": { "deep": ["z".repeat(3000), "ok"] },
            "small": "fine",
            "count": 42,
        });
        truncate_value(&mut value, 50, 2);
        assert!(value["output"].as_str().unwrap().contains("truncated"));
        assert!(value["nested"]["deep"][0]
            .as_str()
            .unwrap()
            .contains("truncated"));
        assert_eq!(value["nested"]["deep"][1], json!("ok"));
        assert_eq!(value["small"], json!("fine"));
        assert_eq!(value["count"], json!(42)); // non-strings untouched
    }

    #[test]
    fn truncate_string_cuts_on_utf8_char_boundary() {
        // Each '中' is 3 bytes; a max of 7 must not split the 3rd char.
        let text = "中".repeat(10);
        let out = truncate_string(&text, 7, 0);
        // Head is valid UTF-8 (no panic) and shorter than original.
        assert!(out.starts_with("中中"));
        assert!(out.contains("truncated"));
    }

    fn temp_repo_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-history-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(path.join(".git")).expect("create fake git dir");
        path
    }

    fn profile(name: &str) -> SourceProfile {
        SourceProfile {
            name: name.to_string(),
            app_id: Some(format!("{name}_app")),
            actor_id: Some("agent-1".to_string()),
            actor_type: Some(ActorType::Agent),
            store_root: Some(PathBuf::from("store")),
            session_db_path: Some(PathBuf::from("sessions.db")),
            session_log_path: None,
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: Some("test source".to_string()),
        }
    }

    #[test]
    fn source_rows_are_stable_json_dtos() {
        let repo_root = temp_repo_root("sources");
        let profiles = SourceProfileStore::new(&repo_root);
        let cursor = profile("cursor");
        profiles.write_profile(&cursor).expect("write profile");
        profiles.use_profile("cursor").expect("select profile");

        let response = build_sources_response(&profiles).expect("build sources");

        assert_eq!(response.sources.len(), 1);
        assert_eq!(response.sources[0].source_id, "cursor");
        assert_eq!(response.sources[0].actor_type.as_deref(), Some("agent"));
        assert!(response.sources[0].selected);
        let serialized = serde_json::to_value(&response).expect("serialize sources");
        assert_eq!(serialized["sources"][0]["source_id"], "cursor");
        assert_eq!(serialized["sources"][0]["session_db_path"], "sessions.db");
    }

    #[test]
    fn doctor_reports_missing_profile_as_structured_error() {
        let repo_root = temp_repo_root("doctor-missing");
        let profiles = SourceProfileStore::new(&repo_root);

        let response = build_doctor_response(&profiles, "claude_code").expect("build doctor");

        assert_eq!(response.source, "claude_code");
        assert_eq!(response.rows.len(), 1);
        let row = &response.rows[0];
        assert_eq!(row.source_id, "claude_code");
        assert_eq!(row.status, "error");
        assert_eq!(row.provider_kind, "claude_code_jsonl");
        assert_eq!(row.parser_kind, "claude-code-jsonl-v4");
        assert!(!row.profile.exists);
        assert!(row
            .errors
            .iter()
            .any(|error| error.contains("source profile not found")));
        assert!(row
            .configured_paths
            .iter()
            .any(|path| path.field == "session_log_path" && path.required && !path.configured));
        let serialized = serde_json::to_value(&response).expect("serialize doctor");
        assert_eq!(
            serialized["rows"][0]["provider_metadata"]["status"],
            "skipped"
        );
    }

    #[test]
    fn doctor_reports_configured_file_source_health() {
        let repo_root = temp_repo_root("doctor-source");
        let profiles = SourceProfileStore::new(&repo_root);
        let session_dir = repo_root.join("claude");
        fs::create_dir_all(&session_dir).expect("create session dir");
        fs::write(
            session_dir.join("session-1.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"hello\"}}\n",
        )
        .expect("write session");
        let mut source_profile = profile("claude_code");
        source_profile.app_id = Some("claude_code".to_string());
        source_profile.session_log_path = Some(session_dir.clone());
        source_profile.session_db_path = None;
        profiles
            .write_profile(&source_profile)
            .expect("write source profile");
        profiles.use_profile("claude_code").expect("select source");

        let response = build_doctor_response(&profiles, "all").expect("build doctor");

        assert_eq!(response.source, "all");
        assert_eq!(response.selected_profile.as_deref(), Some("claude_code"));
        assert_eq!(response.rows.len(), 1);
        let row = &response.rows[0];
        assert_eq!(row.status, "ok");
        assert!(row.profile.exists);
        assert!(row.profile.selected);
        assert_eq!(row.provider_metadata.status, "ok");
        assert_eq!(row.provider_metadata.session_rows, Some(1));
        assert_eq!(row.provider_metadata.plan_rows, Some(0));
        let session_path = row
            .configured_paths
            .iter()
            .find(|path| path.field == "session_log_path")
            .expect("session_log_path diagnostic");
        assert!(session_path.configured);
        assert_eq!(session_path.exists, Some(true));
        assert_eq!(session_path.readable, Some(true));
        assert_eq!(session_path.kind.as_deref(), Some("directory"));
        assert!(row.errors.is_empty());
    }

    #[test]
    fn doctor_reports_missing_required_provider_path() {
        let mut source_profile = profile("opencode");
        source_profile.session_db_path = None;
        source_profile.session_log_path = None;
        source_profile.evidence_root = None;

        let row = build_doctor_row(source_profile, true, None);

        assert_eq!(row.status, "error");
        assert_eq!(row.provider_kind, "opencode_sqlite");
        assert!(row
            .configured_paths
            .iter()
            .any(|path| path.field == "session_db_path" && path.required && !path.configured));
        assert!(row
            .errors
            .iter()
            .any(|error| error.contains("required source path not configured: session_db_path")));
        assert_eq!(row.provider_metadata.status, "error");
    }

    #[test]
    fn sessions_page_applies_limit_offset_and_has_more() {
        let root = temp_repo_root("sessions-root");
        let session_dir = root.join("native");
        fs::create_dir_all(&session_dir).expect("create native dir");
        fs::write(
            session_dir.join("one.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"one\"}}\n",
        )
        .expect("write one");
        fs::write(
            session_dir.join("two.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:01:00Z\",\"message\":{\"role\":\"user\",\"content\":\"two\"}}\n",
        )
        .expect("write two");
        fs::write(
            session_dir.join("three.jsonl"),
            "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:02:00Z\",\"message\":{\"role\":\"user\",\"content\":\"three\"}}\n",
        )
        .expect("write three");

        let mut profile = profile("claude_code");
        profile.app_id = Some("claude_code".to_string());
        profile.session_log_path = Some(session_dir);

        let mut metadata_db =
            MetadataDb::open_path(root.join("metadata.sqlite")).expect("open metadata DB");
        refresh_profiles_to_metadata(&mut metadata_db, std::slice::from_ref(&profile), 10)
            .expect("refresh metadata index");
        let response =
            build_sessions_response(&metadata_db, &profile, 1, 1).expect("build sessions");

        assert_eq!(response.source_id, "claude_code");
        assert_eq!(response.limit, 1);
        assert_eq!(response.offset, 1);
        assert_eq!(response.sessions.len(), 1);
        assert!(response.has_more);
        assert_eq!(response.sessions[0].source_id, "claude_code");
        assert_eq!(response.sessions[0].app_id, "claude_code");
    }

    #[test]
    fn recent_paths_can_aggregate_all_sources() {
        let root = temp_repo_root("recent-root");
        let first_dir = root.join("first");
        let second_dir = root.join("second");
        fs::create_dir_all(&first_dir).expect("create first dir");
        fs::create_dir_all(&second_dir).expect("create second dir");
        fs::write(first_dir.join("alpha.jsonl"), "alpha").expect("write alpha");
        fs::write(second_dir.join("beta.jsonl"), "beta").expect("write beta");

        let mut first = profile("first");
        first.session_log_path = Some(first_dir);
        let mut second = profile("second");
        second.session_log_path = Some(second_dir);

        let mut metadata_db =
            MetadataDb::open_path(root.join("metadata.sqlite")).expect("open metadata DB");
        let profiles = [first, second];
        refresh_profiles_to_metadata(&mut metadata_db, &profiles, 10)
            .expect("refresh metadata index");
        let response = build_recent_paths_response(&metadata_db, "all".to_string(), &profiles, 10)
            .expect("build recent paths");

        assert_eq!(response.source_id, "all");
        assert_eq!(response.paths.len(), 2);
        assert!(response.paths.iter().any(|row| row.source_id == "first"));
        assert!(response.paths.iter().any(|row| row.source_id == "second"));
    }

    #[test]
    fn plans_response_includes_rows_and_unresolved_edges() {
        let root = temp_repo_root("plans-root");
        let mut metadata_db =
            MetadataDb::open_path(root.join("metadata.sqlite")).expect("open metadata DB");
        let profile = profile("cursor_ide");
        let now = Utc
            .with_ymd_and_hms(2026, 6, 18, 2, 3, 4)
            .single()
            .expect("valid plan timestamp");

        metadata_db
            .upsert_source_plan_with_edges(&brick_core::SourcePlanWithEdgesUpsert {
                plan: brick_core::SourcePlanUpsert {
                    source_id: profile.name.clone(),
                    external_plan_id: "plan-1".to_string(),
                    title: Some("Plan one".to_string()),
                    source_path: Some(PathBuf::from("/tmp/plan-1.plan.md")),
                    source_uri: Some("file:///tmp/plan-1.plan.md".to_string()),
                    source_mtime: Some(now),
                    parser_version: Some("cursor-ide-plan-registry-v1".to_string()),
                    discovered_at: now,
                    last_seen_at: now,
                    metadata_json: Some(json!({ "raw": true })),
                },
                edges: vec![brick_core::SourcePlanSessionEdgeUpsert {
                    source_id: profile.name.clone(),
                    external_plan_id: "plan-1".to_string(),
                    external_session_id: "missing-session".to_string(),
                    role: brick_core::SourcePlanSessionEdgeRole::ReferencedBy,
                    todo_ids_json: Some(json!(["todo-1"])),
                    discovered_at: now,
                    last_seen_at: now,
                    metadata_json: Some(json!({ "edge": true })),
                }],
            })
            .expect("upsert source plan");

        let response =
            build_plans_response(&metadata_db, &profile, 20, 0).expect("build plans response");
        let serialized = serde_json::to_value(&response).expect("serialize plans response");

        assert_eq!(response.source_id, "cursor_ide");
        assert_eq!(response.total, 1);
        assert!(!response.has_more);
        assert_eq!(response.plans[0].plan_id, "plan-1");
        assert_eq!(
            response.plans[0].source_path.as_deref(),
            Some("/tmp/plan-1.plan.md")
        );
        assert_eq!(response.edges[0].external_session_id, "missing-session");
        assert_eq!(response.edges[0].role, "referenced_by");
        assert_eq!(serialized["edges"][0]["todo_ids_json"], json!(["todo-1"]));
    }

    #[test]
    fn formats_system_time_as_rfc3339() {
        let formatted =
            format_system_time(UNIX_EPOCH + Duration::from_secs(1)).expect("format timestamp");

        assert_eq!(formatted, "1970-01-01T00:00:01+00:00");
    }

    fn source_session_record_for_export() -> SourceSessionRecord {
        let created_at = Utc
            .with_ymd_and_hms(2026, 6, 18, 1, 2, 3)
            .single()
            .expect("valid timestamp");
        let updated_at = Utc
            .with_ymd_and_hms(2026, 6, 18, 1, 3, 4)
            .single()
            .expect("valid timestamp");
        SourceSessionRecord {
            source_id: "claude_code".to_string(),
            external_session_id: "session-1".to_string(),
            title: Some("Investigate bug".to_string()),
            name: Some("Investigate bug".to_string()),
            source_path: Some(PathBuf::from("/tmp/session-1.jsonl")),
            source_uri: Some("file:///tmp/session-1.jsonl".to_string()),
            source_mtime: Some(updated_at),
            source_size: Some(123),
            source_fingerprint: Some("sha256:abc".to_string()),
            parser_version: Some("claude-code-jsonl-v4".to_string()),
            session_created_at: Some(created_at),
            session_updated_at: Some(updated_at),
            model: Some("claude-sonnet".to_string()),
            input_tokens: Some(100),
            output_tokens: Some(25),
            repo_path: Some(PathBuf::from("/tmp/repo")),
            branch: Some("main".to_string()),
            files_changed: Some(2),
            lines_added: Some(10),
            lines_removed: Some(3),
            touched_files_json: Some(json!(["src/lib.rs", "README.md"])),
            listable: true,
            discovered_at: created_at,
            last_seen_at: updated_at,
            created_at,
            updated_at,
            metadata_json: Some(json!({ "app_id": "claude_code" })),
        }
    }

    #[test]
    fn audit_export_serializes_universal_session_shape() {
        let response = build_audit_export(source_session_record_for_export(), Vec::new());

        assert_eq!(response.schema, HISTORY_EXPORT_SCHEMA_AUDIT_V1);
        assert_eq!(response.source.source_id, "claude_code");
        assert_eq!(response.session.session_id, "session-1");
        assert_eq!(response.token_usage.total_tokens, Some(125));
        assert_eq!(
            response.impact.touched_files,
            vec!["src/lib.rs", "README.md"]
        );
        assert_eq!(
            response.evidence.availability,
            EXPORT_AVAILABILITY_METADATA_ONLY
        );
        assert!(response.chunks.is_empty());
        let serialized = serde_json::to_value(&response).expect("serialize audit export");
        assert_eq!(serialized["schema"], HISTORY_EXPORT_SCHEMA_AUDIT_V1);
        assert_eq!(serialized["session"]["model"], "claude-sonnet");
        assert_eq!(serialized["token_usage"]["total_tokens"], 125);
    }

    #[test]
    fn source_metadata_export_preserves_index_fields() {
        let response = build_source_metadata_export(source_session_record_for_export(), Vec::new());

        assert_eq!(response.schema, HISTORY_EXPORT_SCHEMA_SOURCE_METADATA_V1);
        assert_eq!(response.source_session.source_id, "claude_code");
        assert_eq!(response.source_session.external_session_id, "session-1");
        assert_eq!(response.source_session.source_size, Some(123));
        assert_eq!(
            response.source_session.source_fingerprint.as_deref(),
            Some("sha256:abc")
        );
        assert_eq!(response.source_session.total_tokens, Some(125));
        assert!(response.chunks.is_empty());
        let serialized = serde_json::to_value(&response).expect("serialize metadata export");
        assert_eq!(
            serialized["schema"],
            HISTORY_EXPORT_SCHEMA_SOURCE_METADATA_V1
        );
        assert_eq!(
            serialized["source_session"]["parser_version"],
            "claude-code-jsonl-v4"
        );
    }

    #[test]
    fn audit_export_includes_formatted_source_chunks() {
        let chunk = user_message_chunk(
            "session-1",
            "claudecode",
            0,
            "2026-06-18T01:00:00Z",
            "hello",
        );
        let response = build_audit_export(source_session_record_for_export(), vec![chunk]);

        assert_eq!(response.chunks.len(), 1);
        let serialized = serde_json::to_value(&response).expect("serialize audit chunks");
        assert_eq!(serialized["chunks"][0]["action_type"], ACTION_TYPE_RAW);
        assert_eq!(serialized["chunks"][0]["function"], FUNCTION_USER_MESSAGE);
        assert_eq!(
            serialized["chunks"][0]["result"]["message"]["content"],
            "hello"
        );
    }

    #[test]
    fn export_csv_row_flattens_session_and_chunk_fields() {
        let mut chunk = user_message_chunk(
            "session-1",
            "claudecode",
            0,
            "2026-06-18T01:00:00Z",
            "hello, \"world\"",
        );
        chunk.set_source_pointer(
            "claude_code",
            &PathBuf::from("/tmp/session-1.jsonl"),
            Some("session-1:7"),
            Some(7),
            Some("message-1"),
            Some("part-1"),
        );
        let row = export_csv_row(&source_session_record_for_export(), Some(&chunk));

        assert_eq!(row[0], "claude_code");
        assert_eq!(row[2], "session-1");
        assert_eq!(row[10], "25");
        assert_eq!(row[11], "125");
        assert_eq!(row[15], "src/lib.rs;README.md");
        assert_eq!(row[20], FUNCTION_USER_MESSAGE);
        assert_eq!(row[21], "claude_code");
        assert_eq!(row[22], "/tmp/session-1.jsonl");
        assert_eq!(row[23], "session-1:7");
        assert_eq!(row[24], "7");
        assert_eq!(row[25], "message-1");
        assert_eq!(row[26], "part-1");
        assert!(row[28].starts_with('"'));
        assert!(row[28].contains("hello,"));
        assert!(row[28].contains("world"));
    }

    #[test]
    fn chunks_response_serializes_empty_activity_chunk_list() {
        let response = HistoryChunksResponse {
            source_id: "cursor".to_string(),
            session_id: "session-1".to_string(),
            total_chunks: 0,
            offset: 0,
            returned: 0,
            has_more: false,
            chunks: Vec::new(),
        };

        let serialized = serde_json::to_value(&response).expect("serialize chunks");
        assert_eq!(serialized["source_id"], "cursor");
        assert_eq!(serialized["session_id"], "session-1");
        assert_eq!(serialized["total_chunks"], 0);
        assert_eq!(serialized["has_more"], false);
        assert!(serialized["chunks"]
            .as_array()
            .expect("chunks array")
            .is_empty());
    }
}
