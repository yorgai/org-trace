//! JSON history command surface for native source profiles.
//!
//! This module intentionally stays read-only and non-interactive. The first-stage
//! implementation adapts configured source profiles and native session file
//! listings into stable JSON DTOs that can be consumed by ORGII-style callers.

use std::path::PathBuf;
use std::time::SystemTime;
#[cfg(test)]
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Result};
use brick_core::{
    format_source_session_chunks, list_source_plans, list_source_sessions, ActivityChunk,
    MetadataDb, NativeSourceSession, SourceProfile, SourceProfileStore, SourceSessionListQuery,
    SourceSessionRecord, SourceSessionUpsert,
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
const EXPORT_REFRESH_LIMIT: usize = 10_000;

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

/// Executes read-only history subcommands and emits machine-readable JSON.
pub fn handle_history(command: HistoryCommand, profiles: &SourceProfileStore) -> Result<()> {
    match command {
        HistoryCommand::Sources { format } => {
            ensure_json(format);
            print_json(&build_sources_response(profiles)?)
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
            refresh_profiles_to_metadata(
                &mut metadata_db,
                std::slice::from_ref(&profile),
                offset.saturating_add(limit).saturating_add(1),
            )?;
            print_json(&build_sessions_response(
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
            refresh_profiles_to_metadata(&mut metadata_db, &selected_profiles, limit)?;
            print_json(&build_recent_paths_response(
                &metadata_db,
                source_label,
                &selected_profiles,
                limit,
            )?)
        }
        HistoryCommand::Chunks {
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
            let record = metadata_db
                .get_source_session(&profile.name, &session_id)?
                .ok_or_else(|| {
                    anyhow!("source session not found: {}/{}", profile.name, session_id)
                })?;
            let chunks = format_chunks_for_record(&record)?;
            print_json(&HistoryChunksResponse {
                source_id: source,
                session_id,
                chunks,
            })
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
    }
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
        "source_id,app_id,external_session_id,title,model,created_at,updated_at,repo_path,branch,input_tokens,output_tokens,total_tokens,files_changed,lines_added,lines_removed,touched_files,source_path,chunk_id,chunk_created_at,chunk_action_type,chunk_function,chunk_args_json,chunk_result_json"
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

fn refresh_profiles_to_metadata(
    metadata_db: &mut MetadataDb,
    profiles: &[SourceProfile],
    limit: usize,
) -> Result<()> {
    for profile in profiles {
        for session in list_source_sessions(profile, Some(limit))? {
            let upsert = source_session_upsert(&profile.name, session);
            metadata_db.upsert_source_session(&upsert)?;
        }
        for plan in list_source_plans(profile)? {
            metadata_db.upsert_source_plan_with_edges(&plan)?;
        }
    }
    Ok(())
}

fn source_session_upsert(source_id: &str, session: NativeSourceSession) -> SourceSessionUpsert {
    let now = Utc::now();
    let source_mtime = session.modified_at.map(system_time_to_utc);
    SourceSessionUpsert {
        source_id: source_id.to_string(),
        external_session_id: session.external_session_id,
        title: session.title.clone(),
        name: session.title,
        source_path: Some(session.path.clone()),
        source_uri: Some(format!("file://{}", session.path.display())),
        source_mtime,
        source_size: Some(session.size_bytes),
        source_fingerprint: None,
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
        listable: true,
        discovered_at: now,
        last_seen_at: session
            .session_updated_at
            .map(system_time_to_utc)
            .or(source_mtime)
            .unwrap_or(now),
        metadata_json: Some(json!({ "app_id": session.source_app_id })),
    }
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

fn read_profile(profiles: &SourceProfileStore, source: &str) -> Result<SourceProfile> {
    profiles
        .read_profile(source)?
        .ok_or_else(|| anyhow!("source profile not found: {source}"))
}

fn print_json<T: Serialize>(value: &T) -> Result<()> {
    println!("{}", serde_json::to_string(value)?);
    Ok(())
}

fn ensure_json(format: HistoryFormatArg) {
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

    use super::*;

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
            parser_version: Some("claude-code-jsonl-v1".to_string()),
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
            "claude-code-jsonl-v1"
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
        let chunk = user_message_chunk(
            "session-1",
            "claudecode",
            0,
            "2026-06-18T01:00:00Z",
            "hello, \"world\"",
        );
        let row = export_csv_row(&source_session_record_for_export(), Some(&chunk));

        assert_eq!(row[0], "claude_code");
        assert_eq!(row[2], "session-1");
        assert_eq!(row[10], "25");
        assert_eq!(row[11], "125");
        assert_eq!(row[15], "src/lib.rs;README.md");
        assert_eq!(row[20], FUNCTION_USER_MESSAGE);
        assert!(row[22].starts_with('"'));
        assert!(row[22].contains("hello,"));
        assert!(row[22].contains("world"));
    }

    #[test]
    fn chunks_response_serializes_empty_activity_chunk_list() {
        let response = HistoryChunksResponse {
            source_id: "cursor".to_string(),
            session_id: "session-1".to_string(),
            chunks: Vec::new(),
        };

        let serialized = serde_json::to_value(&response).expect("serialize chunks");
        assert_eq!(serialized["source_id"], "cursor");
        assert_eq!(serialized["session_id"], "session-1");
        assert!(serialized["chunks"]
            .as_array()
            .expect("chunks array")
            .is_empty());
    }
}
