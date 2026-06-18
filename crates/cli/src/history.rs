//! JSON history command surface for native source profiles.
//!
//! This module intentionally stays read-only and non-interactive. The first-stage
//! implementation adapts configured source profiles and native session file
//! listings into stable JSON DTOs that can be consumed by ORGII-style callers.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use brick_core::{
    list_native_source_sessions, NativeSourceSession, SourceProfile, SourceProfileStore,
};
use brick_protocol::ActorType;
use chrono::{DateTime, Utc};
use serde::Serialize;

use crate::args::{HistoryCommand, HistoryFormatArg};

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

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct HistoryChunksResponse {
    pub source_id: String,
    pub session_id: String,
    pub chunks: Vec<ActivityChunkDto>,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ActivityChunkDto {
    pub chunk_id: String,
    pub source_id: String,
    pub session_id: String,
    pub kind: String,
    pub started_at: Option<String>,
    pub ended_at: Option<String>,
    pub path: Option<String>,
    pub text: Option<String>,
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
            print_json(&build_sessions_response(&profile, limit, offset)?)
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
            print_json(&build_recent_paths_response(
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
            read_profile(profiles, &source)?;
            print_json(&HistoryChunksResponse {
                source_id: source,
                session_id,
                chunks: Vec::new(),
            })
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
    profile: &SourceProfile,
    limit: usize,
    offset: usize,
) -> Result<HistorySessionsResponse> {
    let scan_limit = offset.saturating_add(limit).saturating_add(1);
    let sessions = list_native_source_sessions(profile, Some(scan_limit))?;
    let total = sessions.len();
    let has_more = total > offset.saturating_add(limit);
    let sessions = sessions
        .into_iter()
        .skip(offset)
        .take(limit)
        .map(|session| session_row(&profile.name, session))
        .collect();
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
    source_id: String,
    profiles: &[SourceProfile],
    limit: usize,
) -> Result<HistoryRecentPathsResponse> {
    let mut paths = Vec::new();
    for profile in profiles {
        paths.extend(
            list_native_source_sessions(profile, Some(limit))?
                .into_iter()
                .map(|session| recent_path_row(&profile.name, session)),
        );
    }
    paths.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
    paths.truncate(limit);
    Ok(HistoryRecentPathsResponse {
        source_id,
        limit,
        paths,
    })
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

fn session_row(source_id: &str, session: NativeSourceSession) -> HistorySessionRow {
    HistorySessionRow {
        source_id: source_id.to_string(),
        app_id: session.source_app_id,
        session_id: session.external_session_id.clone(),
        external_session_id: session.external_session_id,
        title: session.title,
        path: session.path.display().to_string(),
        size_bytes: session.size_bytes,
        modified_at: session.modified_at.and_then(format_system_time),
    }
}

fn recent_path_row(source_id: &str, session: NativeSourceSession) -> HistoryRecentPathRow {
    HistoryRecentPathRow {
        source_id: source_id.to_string(),
        app_id: session.source_app_id,
        session_id: session.external_session_id,
        path: session.path.display().to_string(),
        title: session.title,
        size_bytes: session.size_bytes,
        modified_at: session.modified_at.and_then(format_system_time),
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

fn format_system_time(time: SystemTime) -> Option<String> {
    time.duration_since(UNIX_EPOCH).ok()?;
    let datetime: DateTime<Utc> = time.into();
    Some(datetime.to_rfc3339())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::Duration;

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
        fs::write(session_dir.join("one.jsonl"), "one").expect("write one");
        fs::write(session_dir.join("two.jsonl"), "two").expect("write two");
        fs::write(session_dir.join("three.jsonl"), "three").expect("write three");

        let mut profile = profile("claude_code");
        profile.app_id = Some("claude_code".to_string());
        profile.session_log_path = Some(session_dir);

        let response = build_sessions_response(&profile, 1, 1).expect("build sessions");

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

        let response = build_recent_paths_response("all".to_string(), &[first, second], 10)
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
