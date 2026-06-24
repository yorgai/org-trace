//! Internal history helpers used by explain/link and MCP.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::SystemTime;

use anyhow::Result;
use brick_core::{
    list_source_plans, list_source_sessions, ActivityChunk, DiscoveredPathKind, DiscoveredSource,
    LocalStore, MetadataDb, NativeSourceSession, SourceProfile, SourceProfileStore,
    SourceScanStatus, SourceSessionChunksUpsert, SourceSessionListQuery, SourceSessionRecord,
    SourceSessionUpsert,
};
use brick_protocol::{ActorRef, ActorType, SessionId, SourceSessionObservedPayload, TraceEvent};
use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::{json, Value};
use uuid::Uuid;

use crate::args::HistoryFormatArg;

#[cfg(feature = "sync")]
use brick_sync::auto_push_best_effort;

const AUTO_REFRESH_THROTTLE_SECS: i64 = 10;
const SOURCE_INDEX_REFRESH_LIMIT: usize = 100;

pub fn print_version(format: HistoryFormatArg) -> Result<()> {
    ensure_json(format);
    print_json(&json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
        "surface": "explain_link_planning",
    }))
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LiveSessionRow {
    pub source_id: String,
    pub app_id: String,
    pub external_session_id: String,
    pub title: Option<String>,
    pub path: String,
    pub work_scope: Option<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub last_activity: Option<String>,
    pub touched_files: Vec<String>,
}

fn collect_live_sessions(profiles: &[SourceProfile], limit: usize) -> Vec<NativeSourceSession> {
    let mut live = Vec::new();
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

fn live_session_row(session: &NativeSourceSession) -> LiveSessionRow {
    LiveSessionRow {
        source_id: session.source_app_id.clone(),
        app_id: session.source_app_id.clone(),
        external_session_id: session.external_session_id.clone(),
        title: session.title.clone(),
        path: session.path.display().to_string(),
        work_scope: brick_core::work_scope(session).map(|path| path.display().to_string()),
        repo_path: session
            .repo_path
            .as_ref()
            .map(|path| path.display().to_string()),
        branch: session.branch.clone(),
        last_activity: session.last_activity.map(system_time_to_rfc3339),
        touched_files: session.touched_files.clone(),
    }
}

fn system_time_to_rfc3339(time: SystemTime) -> String {
    DateTime::<Utc>::from(time).to_rfc3339()
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct LiveBroadcast {
    pub tier: &'static str,
    pub message: String,
    pub sessions: Vec<LiveSessionRow>,
}

pub(crate) fn build_live_broadcast(
    profiles: &[SourceProfile],
    target_path: &str,
    self_path: Option<&str>,
) -> Option<LiveBroadcast> {
    let live = collect_live_sessions(profiles, 50);
    let target = PathBuf::from(target_path);
    let target_name = target.file_name().map(|name| name.to_owned());

    let mut file_hits = Vec::new();
    let mut scope_hits = Vec::new();

    for session in &live {
        if self_path.is_some_and(|own| own == session.path.to_string_lossy()) {
            continue;
        }
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
                "⚠️ {} active session(s) recently changed this same file: {who}. Coordinate or re-check the file before editing.",
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
                "ℹ️ {} active session(s) are working in this same project right now: {who}. Your view of the code here may change under you.",
                scope_hits.len()
            ),
            sessions: scope_hits,
        });
    }
    None
}

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

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct RefreshStats {
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

fn refresh_profiles_to_metadata(
    store: &LocalStore,
    metadata_db: &mut MetadataDb,
    profiles: &[SourceProfile],
    limit: Option<usize>,
) -> Result<RefreshStats> {
    let mut totals = RefreshStats::default();
    let mut known_event_ids = store.known_event_ids()?;
    for profile in profiles {
        let scan_id = metadata_db.begin_source_scan(&profile.name)?;
        let watermark = metadata_db.get_source_watermark(&profile.name)?;
        let since = watermark
            .as_ref()
            .and_then(|(high_water, _)| high_water.as_deref());
        let profile_stats = match refresh_single_profile(
            store,
            metadata_db,
            profile,
            limit,
            since,
            &mut known_event_ids,
        ) {
            Ok((stats, max_updated_at)) => {
                metadata_db.finish_source_scan(
                    scan_id,
                    SourceScanStatus::Completed,
                    Some(&json!({
                        "scanned": stats.scanned,
                        "reindexed": stats.reindexed,
                        "skipped": stats.skipped,
                    })),
                )?;
                metadata_db.set_source_watermark(
                    &profile.name,
                    max_updated_at.as_deref(),
                    &Utc::now().to_rfc3339(),
                )?;
                totals.merge(stats);
                Ok(())
            }
            Err(error) => {
                metadata_db.finish_source_scan(
                    scan_id,
                    SourceScanStatus::Error,
                    Some(&json!({ "error": error.to_string() })),
                )?;
                Err(error)
            }
        };
        profile_stats?;
    }
    Ok(totals)
}

pub(crate) fn refresh_repo_sources_best_effort(store: &LocalStore) {
    if refresh_repo_sources(store).is_ok() {
        #[cfg(feature = "sync")]
        auto_push_best_effort(store);
    }
}

fn refresh_repo_sources(store: &LocalStore) -> Result<()> {
    let repo_root = store.repo_root();
    let mut profiles = SourceProfileStore::new(repo_root.to_path_buf()).list_profiles()?;
    if profiles.is_empty() {
        profiles = brick_core::discover_sources()
            .iter()
            .map(profile_from_discovered_source)
            .collect();
    }
    if profiles.is_empty() {
        return Ok(());
    }
    let mut metadata_db = MetadataDb::open_global()?;
    let now = Utc::now();
    let due: Vec<SourceProfile> = profiles
        .into_iter()
        .filter(
            |profile| match metadata_db.get_source_watermark(&profile.name) {
                Ok(Some((_, last_refreshed_at))) => {
                    DateTime::parse_from_rfc3339(&last_refreshed_at)
                        .map(|last| {
                            now.signed_duration_since(last.with_timezone(&Utc))
                                .num_seconds()
                                >= AUTO_REFRESH_THROTTLE_SECS
                        })
                        .unwrap_or(true)
                }
                _ => true,
            },
        )
        .collect();
    if due.is_empty() {
        backfill_source_session_events_from_metadata(store, &metadata_db)?;
        return Ok(());
    }
    refresh_profiles_to_metadata(
        store,
        &mut metadata_db,
        &due,
        Some(SOURCE_INDEX_REFRESH_LIMIT),
    )?;
    backfill_source_session_events_from_metadata(store, &metadata_db)?;
    Ok(())
}

fn refresh_single_profile(
    store: &LocalStore,
    metadata_db: &mut MetadataDb,
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<&str>,
    known_event_ids: &mut BTreeSet<Uuid>,
) -> Result<(RefreshStats, Option<String>)> {
    let mut stats = RefreshStats::default();
    let mut max_updated_at: Option<String> = None;
    record_source_roots(metadata_db, profile)?;
    for session in brick_core::list_source_sessions_since(profile, limit, since)? {
        stats.scanned += 1;
        if let Some(updated) = session.session_updated_at.map(system_time_to_utc) {
            let updated = updated.to_rfc3339();
            if max_updated_at
                .as_deref()
                .is_none_or(|current| updated.as_str() > current)
            {
                max_updated_at = Some(updated);
            }
        }
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
        let chunks = if unchanged {
            metadata_db.touch_source_session_last_seen(
                &upsert.source_id,
                &upsert.external_session_id,
                upsert.last_seen_at,
            )?;
            stats.skipped += 1;
            let chunks = metadata_db
                .list_source_session_chunks(&upsert.source_id, &upsert.external_session_id)?
                .into_iter()
                .filter_map(|row| serde_json::from_value(row.raw_json).ok())
                .collect::<Vec<ActivityChunk>>();
            if chunks.is_empty() {
                let chunks = brick_core::format_source_session_chunks(
                    &upsert.source_id,
                    &upsert.external_session_id,
                    upsert.source_path.as_deref(),
                )?;
                metadata_db.upsert_source_session_chunks(&SourceSessionChunksUpsert {
                    source_id: upsert.source_id.clone(),
                    external_session_id: upsert.external_session_id.clone(),
                    chunks: chunks.clone(),
                })?;
                chunks
            } else {
                chunks
            }
        } else {
            metadata_db.upsert_source_session(&upsert)?;
            let chunks = brick_core::format_source_session_chunks(
                &upsert.source_id,
                &upsert.external_session_id,
                upsert.source_path.as_deref(),
            )?;
            metadata_db.upsert_source_session_chunks(&SourceSessionChunksUpsert {
                source_id: upsert.source_id.clone(),
                external_session_id: upsert.external_session_id.clone(),
                chunks: chunks.clone(),
            })?;
            stats.reindexed += 1;
            chunks
        };
        if source_session_belongs_to_repo(&upsert, store.repo_root()) {
            append_source_session_event(store, Some(profile), &upsert, &chunks, known_event_ids)?;
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
    Ok((stats, max_updated_at))
}

fn source_session_belongs_to_repo(session: &SourceSessionUpsert, repo_root: &Path) -> bool {
    let Some(repo_path) = session.repo_path.as_ref() else {
        return false;
    };
    let want = std::fs::canonicalize(repo_root).unwrap_or_else(|_| repo_root.to_path_buf());
    let have = std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.clone());
    have == want
}

fn backfill_source_session_events_from_metadata(
    store: &LocalStore,
    metadata_db: &MetadataDb,
) -> Result<()> {
    let mut known_event_ids = store.known_event_ids()?;
    let mut offset = 0;
    loop {
        let records = metadata_db.list_source_sessions(&SourceSessionListQuery {
            source_id: None,
            limit: 500,
            offset,
        })?;
        if records.is_empty() {
            return Ok(());
        }
        for record in &records {
            let session = source_session_record_to_upsert(record);
            if source_session_belongs_to_repo(&session, store.repo_root()) {
                let chunks = metadata_db
                    .list_source_session_chunks(&session.source_id, &session.external_session_id)?
                    .into_iter()
                    .filter_map(|row| serde_json::from_value(row.raw_json).ok())
                    .collect::<Vec<ActivityChunk>>();
                append_source_session_event(store, None, &session, &chunks, &mut known_event_ids)?;
            }
        }
        offset += records.len();
    }
}

fn source_session_record_to_upsert(record: &SourceSessionRecord) -> SourceSessionUpsert {
    SourceSessionUpsert {
        source_id: record.source_id.clone(),
        external_session_id: record.external_session_id.clone(),
        title: record.title.clone(),
        name: record.name.clone(),
        source_path: record.source_path.clone(),
        source_uri: record.source_uri.clone(),
        source_mtime: record.source_mtime,
        source_size: record.source_size,
        source_fingerprint: record.source_fingerprint.clone(),
        parser_version: record.parser_version.clone(),
        session_created_at: record.session_created_at,
        session_updated_at: record.session_updated_at,
        model: record.model.clone(),
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        repo_path: record.repo_path.clone(),
        branch: record.branch.clone(),
        files_changed: record.files_changed,
        lines_added: record.lines_added,
        lines_removed: record.lines_removed,
        touched_files_json: record.touched_files_json.clone(),
        listable: record.listable,
        discovered_at: record.discovered_at,
        last_seen_at: record.last_seen_at,
        metadata_json: record.metadata_json.clone(),
    }
}

fn append_source_session_event(
    store: &LocalStore,
    profile: Option<&SourceProfile>,
    session: &SourceSessionUpsert,
    chunks: &[ActivityChunk],
    known_event_ids: &mut BTreeSet<Uuid>,
) -> Result<()> {
    let actor = ActorRef {
        actor_type: profile
            .and_then(|profile| profile.actor_type)
            .unwrap_or(ActorType::Agent),
        actor_id: profile
            .and_then(|profile| profile.actor_id.clone())
            .unwrap_or_else(|| session.source_id.clone()),
        display_name: None,
    };
    let source_path = session
        .source_path
        .as_ref()
        .map(|path| path.display().to_string());
    let source_uri = session.source_uri.clone();
    let payload = SourceSessionObservedPayload {
        source_id: session.source_id.clone(),
        external_session_id: session.external_session_id.clone(),
        title: session.title.clone(),
        source_path,
        source_uri,
        source_mtime: session.source_mtime.map(|time| time.to_rfc3339()),
        source_size: session.source_size,
        source_fingerprint: session.source_fingerprint.clone(),
        parser_version: session.parser_version.clone(),
        session_created_at: session.session_created_at.map(|time| time.to_rfc3339()),
        session_updated_at: session.session_updated_at.map(|time| time.to_rfc3339()),
        model: session.model.clone(),
        input_tokens: session.input_tokens,
        output_tokens: session.output_tokens,
        repo_path: session
            .repo_path
            .as_ref()
            .map(|path| path.display().to_string()),
        branch: session.branch.clone(),
        files_changed: session.files_changed,
        lines_added: session.lines_added,
        lines_removed: session.lines_removed,
        touched_files: session
            .touched_files_json
            .as_ref()
            .and_then(|value| value.as_array())
            .map(|files| {
                files
                    .iter()
                    .filter_map(|file| file.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        metadata_json: session.metadata_json.clone(),
        normalized_chunks: chunks
            .iter()
            .filter_map(|chunk| serde_json::to_value(chunk).ok())
            .collect(),
    };
    let event_id = source_session_event_id(session, &payload.normalized_chunks);
    let mut event = TraceEvent::source_session_observed(actor, payload)?;
    event.event_id = event_id;
    event.session_id = Some(session_id_for_source_session(session));
    event.repo_id = Some(brick_core::repo_id_for_root(store.repo_root()));
    if known_event_ids.insert(event.event_id) {
        store.append_event(&event)?;
    }
    Ok(())
}

fn session_id_for_source_session(session: &SourceSessionUpsert) -> SessionId {
    SessionId::from_str(&session.external_session_id).unwrap_or_else(|_| SessionId::new())
}

fn source_session_event_id(session: &SourceSessionUpsert, normalized_chunks: &[Value]) -> Uuid {
    let chunk_fingerprint = serde_json::to_string(normalized_chunks).unwrap_or_default();
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!(
            "brick:source-session-observed-v2:{}:{}:{}:{}:{}",
            session.source_id,
            session.external_session_id,
            session.source_fingerprint.as_deref().unwrap_or_default(),
            session.parser_version.as_deref().unwrap_or_default(),
            chunk_fingerprint
        )
        .as_bytes(),
    )
}

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

fn link_session_repo(
    metadata_db: &mut MetadataDb,
    source_id: &str,
    external_session_id: &str,
    repo_path: Option<&str>,
) -> Result<()> {
    let Some(repo_path) = repo_path.filter(|path| !path.is_empty()) else {
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

fn source_session_upsert(
    profile: &SourceProfile,
    session: NativeSourceSession,
) -> SourceSessionUpsert {
    let now = Utc::now();
    let source_mtime = session.modified_at.map(system_time_to_utc);
    let listable = session.listable;
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

fn profile_from_discovered_source(source: &DiscoveredSource) -> SourceProfile {
    let evidence_root = source
        .paths
        .iter()
        .find(|path| {
            matches!(
                path.kind,
                DiscoveredPathKind::EvidenceRoot | DiscoveredPathKind::SessionLogRoot
            )
        })
        .map(|path| path.path.clone());
    let cursor_state_db_path = source
        .paths
        .iter()
        .find(|path| path.kind == DiscoveredPathKind::CursorStateDatabase)
        .map(|path| path.path.clone());
    let session_log_path = source
        .paths
        .iter()
        .find(|path| path.kind == DiscoveredPathKind::SessionLogRoot)
        .map(|path| path.path.clone());
    let session_db_path = source
        .paths
        .iter()
        .find(|path| {
            matches!(
                path.kind,
                DiscoveredPathKind::SessionDatabase | DiscoveredPathKind::HistoryDatabase
            )
        })
        .map(|path| path.path.clone());

    SourceProfile {
        name: source.source.profile_name().to_string(),
        app_id: Some(source.source.app_id().to_string()),
        actor_id: None,
        actor_type: None,
        store_root: None,
        session_db_path,
        session_log_path,
        evidence_root,
        cursor_state_db_path,
        default_full_evidence_upload: None,
        notes: Some("Discovered by brick source scan".to_string()),
    }
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

fn system_time_to_utc(time: SystemTime) -> DateTime<Utc> {
    time.into()
}

fn format_actor_type(actor_type: ActorType) -> &'static str {
    match actor_type {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use brick_core::StorageOptions;

    use super::*;

    #[test]
    fn source_session_event_id_is_stable_for_same_payload_and_versions_chunks() {
        let session = test_source_session("session-1");
        let empty = Vec::new();
        let chunked = vec![json!({"chunk_id": "chunk-1"})];
        assert_eq!(
            source_session_event_id(&session, &empty),
            source_session_event_id(&session, &empty)
        );
        assert_ne!(
            source_session_event_id(&session, &empty),
            source_session_event_id(&test_source_session("session-2"), &empty)
        );
        assert_ne!(
            source_session_event_id(&session, &empty),
            source_session_event_id(&session, &chunked)
        );
    }

    #[test]
    fn append_source_session_event_dedupes_existing_event_ids() {
        let base = std::env::temp_dir().join(format!("source-session-dedupe-{}", Uuid::new_v4()));
        let repo = base.join("repo");
        let store_root = base.join("store");
        fs::create_dir_all(&repo).expect("repo dir");
        let store = LocalStore::with_options(
            &repo,
            StorageOptions::new().with_explicit_store_root(Some(store_root)),
        )
        .expect("store");
        let profile = test_profile();
        let mut session = test_source_session("session-1");
        session.repo_path = Some(repo);
        let mut known = store.known_event_ids().expect("known ids");

        let chunks = Vec::new();
        append_source_session_event(&store, Some(&profile), &session, &chunks, &mut known)
            .expect("append once");
        append_source_session_event(&store, Some(&profile), &session, &chunks, &mut known)
            .expect("append twice");

        let events = store.read_queued_events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_id, source_session_event_id(&session, &[]));
        assert_eq!(
            events[0].event_type,
            brick_protocol::EventType::SourceSessionObserved
        );
    }

    #[test]
    fn append_source_session_event_allows_chunk_backfill_after_empty_event() {
        let base = std::env::temp_dir().join(format!("source-session-backfill-{}", Uuid::new_v4()));
        let repo = base.join("repo");
        let store_root = base.join("store");
        fs::create_dir_all(&repo).expect("repo dir");
        let store = LocalStore::with_options(
            &repo,
            StorageOptions::new().with_explicit_store_root(Some(store_root)),
        )
        .expect("store");
        let profile = test_profile();
        let mut session = test_source_session("session-1");
        session.repo_path = Some(repo);
        let mut known = store.known_event_ids().expect("known ids");

        append_source_session_event(&store, Some(&profile), &session, &[], &mut known)
            .expect("append empty event");
        let mut chunk = ActivityChunk::new("session-1", "message", "assistant");
        chunk.result = json!({"content": "backfilled"});
        append_source_session_event(&store, Some(&profile), &session, &[chunk], &mut known)
            .expect("append chunked event");

        let events = store.read_queued_events().expect("events");
        assert_eq!(events.len(), 2);
        assert_ne!(events[0].event_id, events[1].event_id);
        assert!(events[0].payload["normalized_chunks"].as_array().is_none());
        assert_eq!(
            events[1].payload["normalized_chunks"][0]["result"]["content"],
            "backfilled"
        );
    }

    #[test]
    fn append_source_session_event_carries_normalized_chunks_when_present() {
        let base = std::env::temp_dir().join(format!("source-session-log-{}", Uuid::new_v4()));
        let repo = base.join("repo");
        let store_root = base.join("store");
        fs::create_dir_all(&repo).expect("repo dir");
        let source_log = base.join("session.jsonl");
        fs::write(&source_log, "{\"message\":\"hello\"}\n").expect("write source log");
        let store = LocalStore::with_options(
            &repo,
            StorageOptions::new().with_explicit_store_root(Some(store_root)),
        )
        .expect("store");
        let profile = test_profile();
        let mut session = test_source_session("session-1");
        session.repo_path = Some(repo);
        session.source_path = Some(source_log);
        let mut known = store.known_event_ids().expect("known ids");

        let mut chunk = ActivityChunk::new("session-1", "message", "assistant");
        chunk.result = json!({"content": "hello"});
        let chunks = vec![chunk];
        append_source_session_event(&store, Some(&profile), &session, &chunks, &mut known)
            .expect("append source session");

        let events = store.read_queued_events().expect("events");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0].event_type,
            brick_protocol::EventType::SourceSessionObserved
        );
        let normalized_chunks = events[0].payload["normalized_chunks"]
            .as_array()
            .expect("normalized chunks");
        assert_eq!(normalized_chunks.len(), 1);
        assert_eq!(normalized_chunks[0]["result"]["content"], "hello");
    }

    #[test]
    fn live_broadcast_none_when_no_sessions() {
        assert!(build_live_broadcast(&[], "src/lib.rs", None).is_none());
    }

    fn test_profile() -> SourceProfile {
        SourceProfile {
            name: "orgii".to_string(),
            app_id: Some("orgii".to_string()),
            actor_id: Some("agent-1".to_string()),
            actor_type: Some(ActorType::Agent),
            store_root: None,
            session_db_path: None,
            session_log_path: None,
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        }
    }

    fn test_source_session(external_session_id: &str) -> SourceSessionUpsert {
        let now = Utc::now();
        SourceSessionUpsert {
            source_id: "orgii".to_string(),
            external_session_id: external_session_id.to_string(),
            title: Some("Investigate sync".to_string()),
            name: Some("Investigate sync".to_string()),
            source_path: None,
            source_uri: None,
            source_mtime: None,
            source_size: None,
            source_fingerprint: Some(format!("fingerprint-{external_session_id}")),
            parser_version: Some("test".to_string()),
            session_created_at: Some(now),
            session_updated_at: Some(now),
            model: Some("test-model".to_string()),
            input_tokens: Some(1),
            output_tokens: Some(2),
            repo_path: None,
            branch: Some("main".to_string()),
            files_changed: Some(1),
            lines_added: Some(2),
            lines_removed: Some(0),
            touched_files_json: Some(json!(["src/lib.rs"])),
            listable: true,
            discovered_at: now,
            last_seen_at: now,
            metadata_json: None,
        }
    }
}
