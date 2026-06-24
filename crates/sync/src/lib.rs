//! Proprietary cross-server sync for Brick.
//!
//! This crate holds everything about talking to a remote trace server: the wire
//! message types and the push/pull/sync command handlers. It is intentionally
//! NOT part of the default open-source build — `brick` depends on it only under
//! the `sync` feature, and the open-source `brick-protocol` crate carries no
//! sync types. Keeping all of this here lets the directory be excised from the
//! public repository without touching the open build.
//!
//! Push is intentionally non-draining: local JSONL queue files remain the source
//! of truth after a successful remote append. Pull stores remote events in a
//! separate inbound log and deduplicates by event ID before writing.

pub mod identity;
pub mod supabase;
pub mod wire;

use anyhow::{Context, Result};
use brick_core::{
    repo_id_for_root, ActivityChunk, LocalStore, MetadataDb, SourceSessionChunksUpsert,
    SourceSessionUpsert,
};
use brick_protocol::{OrgId, SourceSessionObservedPayload, TraceEvent};
use chrono::Utc;
use std::path::PathBuf;
use std::str::FromStr;

pub use identity::{is_logged_in, Identity};
pub use wire::{EventCursor, ListEventsResponse, PushEventsRequest, PushEventsResponse};

const DEFAULT_REMOTE: &str = "http://127.0.0.1:7821";
const PULL_PAGE_LIMIT: usize = 500;
const AUTO_SYNC_REMOTE_ENV: &str = "BRICK_AUTO_SYNC_REMOTE";
const AUTO_SYNC_DISABLE_ENV: &str = "BRICK_AUTO_SYNC_DISABLE";

pub fn auto_pull_best_effort(store: &LocalStore) {
    if auto_sync_disabled() {
        return;
    }
    let _ = auto_pull(store);
}

pub fn auto_push_best_effort(store: &LocalStore) {
    if auto_sync_disabled() {
        return;
    }
    let _ = auto_push(store);
}

fn auto_pull(store: &LocalStore) -> Result<()> {
    let identity = identity::refresh_if_needed()?;
    let remote = auto_sync_remote();
    let repo_id = repo_id_for_root(store.repo_root());
    let response = if supabase::is_supabase_remote(&remote) {
        supabase::SupabaseRemote::from_env()?
            .get_all_events(Some(&repo_id), &identity.access_token)?
    } else {
        get_all_events_from_remote(&remote, Some(&repo_id), Some(&identity.access_token))?
    };
    let _ = pull_events(store, response.events, false)?;
    Ok(())
}

fn auto_push(store: &LocalStore) -> Result<()> {
    let events = store.read_queued_events()?;
    if events.is_empty() {
        return Ok(());
    }
    let events = hydrate_source_session_chunks(events)?;
    let identity = identity::refresh_if_needed()?;
    let remote = auto_sync_remote();
    let repo_id = repo_id_for_root(store.repo_root());
    let org_id = std::env::var("BRICK_SYNC_ORG_ID").ok();
    let request = PushEventsRequest {
        events: scoped_events(events, Some(&repo_id), org_id.as_deref())?,
    };
    if supabase::is_supabase_remote(&remote) {
        supabase::SupabaseRemote::from_env()?.push_events(
            Some(&repo_id),
            &request,
            &identity.access_token,
        )?;
    } else {
        push_events_to_remote(
            &remote,
            Some(&repo_id),
            &request,
            Some(&identity.access_token),
        )?;
    };
    Ok(())
}

fn auto_sync_remote() -> String {
    normalized_remote(std::env::var(AUTO_SYNC_REMOTE_ENV).ok())
}

fn auto_sync_disabled() -> bool {
    matches!(
        std::env::var(AUTO_SYNC_DISABLE_ENV).ok().as_deref(),
        Some("1") | Some("true") | Some("yes") | Some("on")
    )
}

/// Handles `push` by posting queued events without draining the local queue.
///
/// Upload is account-scoped: the queued events are sent with the logged-in
/// user's Supabase bearer token so the server can attribute them to the account
/// (and its org) for org-scope blame. Refuses to push when not logged in —
/// uploading is the registered-tier feature, distinct from the always-free local
/// recorder.
pub fn handle_push(
    store: &LocalStore,
    dry_run: bool,
    remote: Option<String>,
    repo_id: Option<String>,
    org_id: Option<String>,
) -> Result<()> {
    let events = store.read_queued_events()?;
    let remote = normalized_remote(remote);
    let repo_id = repo_id.unwrap_or_else(|| repo_id_for_root(store.repo_root()));
    if dry_run {
        println!("push_dry_run=true");
        println!("remote={remote}");
        println!("repo_id={repo_id}");
        println!("org_id={}", org_id.as_deref().unwrap_or(""));
        println!("queued_event_count={}", events.len());
        return Ok(());
    }

    // Account-scoped upload: refresh the token if needed and send it as the
    // bearer so the server attributes events to this user/org. Without this the
    // login → upload → org-blame pipeline is not actually connected.
    let identity = identity::refresh_if_needed()
        .context("upload requires a Brick account. Run `brick login` first")?;

    let events = hydrate_source_session_chunks(events)?;
    let request = PushEventsRequest {
        events: scoped_events(events, Some(&repo_id), org_id.as_deref())?,
    };
    let response = if supabase::is_supabase_remote(&remote) {
        supabase::SupabaseRemote::from_env()?.push_events(
            Some(&repo_id),
            &request,
            &identity.access_token,
        )?
    } else {
        push_events_to_remote(
            &remote,
            Some(&repo_id),
            &request,
            Some(&identity.access_token),
        )?
    };
    print_push_result(&response, request.events.len());
    Ok(())
}

/// Handles `pull` by storing previously unknown remote events in inbound logs.
pub fn handle_pull(
    store: &LocalStore,
    dry_run: bool,
    remote: Option<String>,
    repo_id: Option<String>,
) -> Result<()> {
    let remote = normalized_remote(remote);
    let identity = identity::refresh_if_needed()
        .context("pull requires a Brick account. Run `brick sync login` first")?;
    let repo_id_for_print = repo_id.clone().or_else(|| {
        supabase::is_supabase_remote(&remote).then(|| repo_id_for_root(store.repo_root()))
    });
    let response = if supabase::is_supabase_remote(&remote) {
        let repo_id = repo_id_for_print
            .clone()
            .context("Supabase sync requires --repo-id or a git repository root")?;
        supabase::SupabaseRemote::from_env()?
            .get_all_events(Some(&repo_id), &identity.access_token)?
    } else {
        get_all_events_from_remote(&remote, repo_id.as_deref(), Some(&identity.access_token))?
    };
    let outcome = pull_events(store, response.events, dry_run)?;
    print_pull_result(&remote, repo_id_for_print.as_deref(), dry_run, &outcome);
    Ok(())
}

/// Handles `sync` as pull followed by non-draining push against the same remote.
pub fn handle_sync(
    store: &LocalStore,
    dry_run: bool,
    remote: Option<String>,
    repo_id: Option<String>,
    org_id: Option<String>,
) -> Result<()> {
    let remote = normalized_remote(remote);
    handle_pull(store, dry_run, Some(remote.clone()), repo_id.clone())?;
    handle_push(store, dry_run, Some(remote), repo_id, org_id)?;
    Ok(())
}

/// Creates a Supabase-backed Brick org and makes the logged-in user owner.
pub fn handle_create_org(org_id: String) -> Result<()> {
    let identity = identity::refresh_if_needed()
        .context("create-org requires a Brick account. Run `brick sync login` first")?;
    supabase::SupabaseRemote::from_env()?.create_org(&org_id, &identity.access_token)?;
    println!("org_created=true");
    println!("org_id={org_id}");
    println!("owner_user_id={}", identity.user_id);
    Ok(())
}

/// Invites an email address into an org through Supabase-native membership RPC.
pub fn handle_invite(org_id: String, email: String) -> Result<()> {
    let identity = identity::refresh_if_needed()
        .context("invite requires a Brick account. Run `brick sync login` first")?;
    supabase::SupabaseRemote::from_env()?.invite_org_member(
        &org_id,
        &email,
        &identity.access_token,
    )?;
    println!("invite_sent=true");
    println!("org_id={org_id}");
    println!("email={email}");
    Ok(())
}

/// Accepts pending email invites for the logged-in Supabase account.
pub fn handle_accept_invites() -> Result<()> {
    let identity = identity::refresh_if_needed()
        .context("accept-invites requires a Brick account. Run `brick sync login` first")?;
    supabase::SupabaseRemote::from_env()?.accept_invites(&identity.access_token)?;
    println!("accepted_invites=true");
    println!("user_id={}", identity.user_id);
    println!("email={}", identity.email.as_deref().unwrap_or(""));
    Ok(())
}

fn normalized_remote(remote: Option<String>) -> String {
    remote
        .unwrap_or_else(|| DEFAULT_REMOTE.to_string())
        .trim_end_matches('/')
        .to_string()
}

fn events_url(remote: &str, repo_id: Option<&str>) -> String {
    match repo_id {
        Some(repo_id) if !repo_id.is_empty() => format!("{remote}/v1/repos/{repo_id}/events"),
        _ => format!("{remote}/v1/events"),
    }
}

fn events_page_url(remote: &str, repo_id: Option<&str>, after: Option<&str>) -> String {
    let mut url = format!("{}?limit={PULL_PAGE_LIMIT}", events_url(remote, repo_id));
    if let Some(after) = after {
        url.push_str("&after=");
        url.push_str(after);
    }
    url
}

fn scoped_events(
    events: Vec<TraceEvent>,
    repo_id: Option<&str>,
    org_id: Option<&str>,
) -> Result<Vec<TraceEvent>> {
    let org_id = org_id
        .filter(|value| !value.trim().is_empty())
        .map(OrgId::from_str)
        .transpose()
        .context("invalid --org-id")?;
    events
        .into_iter()
        .map(|mut event| {
            if let Some(repo_id) = repo_id {
                match event.repo_id.as_deref() {
                    Some(existing) if existing != repo_id => {
                        anyhow::bail!(
                            "event {} repo_id {existing:?} does not match push repo_id {repo_id:?}",
                            event.event_id
                        );
                    }
                    Some(_) => {}
                    None => event.repo_id = Some(repo_id.to_string()),
                }
            }
            if event.org_id.is_none() {
                event.org_id = org_id.clone();
            }
            Ok(event)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PullOutcome {
    remote_event_count: usize,
    pulled_event_count: usize,
    duplicate_count: usize,
    inbound_path: Option<String>,
}

fn pull_events(
    store: &LocalStore,
    remote_events: Vec<TraceEvent>,
    dry_run: bool,
) -> Result<PullOutcome> {
    let remote_event_count = remote_events.len();
    let pulled_events = store.dedupe_remote_events(remote_events)?;
    let duplicate_count = remote_event_count.saturating_sub(pulled_events.len());
    let pulled_events = if dry_run {
        pulled_events
    } else {
        ingest_pulled_source_sessions(pulled_events)?
    };
    let pulled_event_count = pulled_events.len();
    let inbound_path = if dry_run || pulled_events.is_empty() {
        None
    } else {
        Some(
            store
                .append_inbound_events(&pulled_events)?
                .display()
                .to_string(),
        )
    };

    Ok(PullOutcome {
        remote_event_count,
        pulled_event_count,
        duplicate_count,
        inbound_path,
    })
}

fn ingest_pulled_source_sessions(mut events: Vec<TraceEvent>) -> Result<Vec<TraceEvent>> {
    let mut metadata_db = MetadataDb::open_global()?;
    ingest_source_sessions_into_metadata(&mut metadata_db, &mut events)?;
    Ok(events)
}

/// Refills `normalized_chunks` on outbound `source.session_observed` events from
/// the local metadata index before upload.
///
/// Canonical events in the JSONL queue deliberately carry no chunks — the
/// session-content snapshot is a derivative of the provider's own log, indexed
/// into `metadata.sqlite`, not part of the append-only causal record. Push is
/// the one place that needs the chunks inline (so the remote can split them into
/// `brick_event_chunks`), so we hydrate them here keyed by
/// `(source_id, external_session_id)`. Best-effort: a session whose chunks are no
/// longer indexed simply uploads without them.
fn hydrate_source_session_chunks(events: Vec<TraceEvent>) -> Result<Vec<TraceEvent>> {
    if !events
        .iter()
        .any(|event| event.event_type == brick_protocol::EventType::SourceSessionObserved)
    {
        return Ok(events);
    }
    let metadata_db = MetadataDb::open_global()?;
    hydrate_source_session_chunks_with(&metadata_db, events)
}

fn hydrate_source_session_chunks_with(
    metadata_db: &MetadataDb,
    events: Vec<TraceEvent>,
) -> Result<Vec<TraceEvent>> {
    events
        .into_iter()
        .map(|mut event| {
            if event.event_type != brick_protocol::EventType::SourceSessionObserved {
                return Ok(event);
            }
            let source_id = event
                .payload
                .get("source_id")
                .and_then(serde_json::Value::as_str);
            let external_session_id = event
                .payload
                .get("external_session_id")
                .and_then(serde_json::Value::as_str);
            let (Some(source_id), Some(external_session_id)) = (source_id, external_session_id)
            else {
                return Ok(event);
            };
            let chunks: Vec<serde_json::Value> = metadata_db
                .list_source_session_chunks(source_id, external_session_id)?
                .into_iter()
                .map(|row| row.raw_json)
                .collect();
            if let Some(payload) = event.payload.as_object_mut() {
                payload.insert(
                    "normalized_chunks".to_string(),
                    serde_json::Value::Array(chunks),
                );
            }
            Ok(event)
        })
        .collect()
}

fn ingest_source_sessions_into_metadata(
    metadata_db: &mut MetadataDb,
    events: &mut [TraceEvent],
) -> Result<()> {
    for event in events {
        if event.event_type != brick_protocol::EventType::SourceSessionObserved {
            continue;
        }
        let payload: SourceSessionObservedPayload =
            serde_json::from_value(event.payload.clone())
                .context("failed to decode source session observed payload")?;
        metadata_db.upsert_source_session(&source_session_upsert_from_payload(&payload)?)?;
        if !payload.normalized_chunks.is_empty() {
            let chunks: Vec<ActivityChunk> = payload
                .normalized_chunks
                .iter()
                .cloned()
                .map(serde_json::from_value)
                .collect::<serde_json::Result<Vec<_>>>()
                .context("failed to decode normalized source-session chunks")?;
            metadata_db.upsert_source_session_chunks(&SourceSessionChunksUpsert {
                source_id: payload.source_id.clone(),
                external_session_id: payload.external_session_id.clone(),
                chunks,
            })?;
        }
    }
    Ok(())
}

fn source_session_upsert_from_payload(
    payload: &SourceSessionObservedPayload,
) -> Result<SourceSessionUpsert> {
    let now = Utc::now();
    Ok(SourceSessionUpsert {
        source_id: payload.source_id.clone(),
        external_session_id: payload.external_session_id.clone(),
        title: payload.title.clone(),
        name: payload.title.clone(),
        source_path: payload.source_path.as_ref().map(PathBuf::from),
        source_uri: payload.source_uri.clone(),
        source_mtime: parse_rfc3339(payload.source_mtime.as_deref()),
        source_size: payload.source_size,
        source_fingerprint: payload.source_fingerprint.clone(),
        parser_version: payload.parser_version.clone(),
        session_created_at: parse_rfc3339(payload.session_created_at.as_deref()),
        session_updated_at: parse_rfc3339(payload.session_updated_at.as_deref()),
        model: payload.model.clone(),
        input_tokens: payload.input_tokens,
        output_tokens: payload.output_tokens,
        repo_path: payload.repo_path.as_ref().map(PathBuf::from),
        branch: payload.branch.clone(),
        files_changed: payload.files_changed,
        lines_added: payload.lines_added,
        lines_removed: payload.lines_removed,
        touched_files_json: Some(serde_json::to_value(&payload.touched_files)?),
        listable: true,
        discovered_at: now,
        last_seen_at: now,
        metadata_json: payload.metadata_json.clone(),
    })
}

fn parse_rfc3339(value: Option<&str>) -> Option<chrono::DateTime<Utc>> {
    value
        .and_then(|value| chrono::DateTime::parse_from_rfc3339(value).ok())
        .map(|value| value.with_timezone(&Utc))
}

fn print_push_result(response: &PushEventsResponse, queued_event_count: usize) {
    println!("accepted_count={}", response.accepted_count());
    println!("duplicate_count={}", response.duplicate_count());
    println!("queued_event_count={queued_event_count}");
}

fn print_pull_result(remote: &str, repo_id: Option<&str>, dry_run: bool, outcome: &PullOutcome) {
    println!("pull_dry_run={dry_run}");
    println!("remote={remote}");
    println!("repo_id={}", repo_id.unwrap_or(""));
    println!("remote_event_count={}", outcome.remote_event_count);
    println!("pulled_event_count={}", outcome.pulled_event_count);
    println!("duplicate_count={}", outcome.duplicate_count);
    if !dry_run {
        println!(
            "inbound_path={}",
            outcome.inbound_path.as_deref().unwrap_or("")
        );
    }
}

fn push_events_to_remote(
    remote: &str,
    repo_id: Option<&str>,
    request: &PushEventsRequest,
    bearer: Option<&str>,
) -> Result<PushEventsResponse> {
    let url = events_url(remote, repo_id);
    let mut builder = ureq::post(&url).header("content-type", "application/json");
    if let Some(token) = bearer {
        builder = builder.header("authorization", &format!("Bearer {token}"));
    }
    let mut response = builder
        .send_json(request)
        .with_context(|| format!("failed to POST events to {url}"))?;
    response
        .body_mut()
        .read_json::<PushEventsResponse>()
        .with_context(|| format!("failed to decode push response from {url}"))
}

fn get_all_events_from_remote(
    remote: &str,
    repo_id: Option<&str>,
    bearer: Option<&str>,
) -> Result<ListEventsResponse> {
    let mut events = Vec::new();
    let mut after: Option<EventCursor> = None;
    loop {
        let response = get_events_page_from_remote(remote, repo_id, after.as_deref(), bearer)?;
        events.extend(response.events);
        match response.next_cursor {
            Some(next_cursor) => after = Some(next_cursor),
            None => return Ok(ListEventsResponse::all(events)),
        }
    }
}

fn get_events_page_from_remote(
    remote: &str,
    repo_id: Option<&str>,
    after: Option<&str>,
    bearer: Option<&str>,
) -> Result<ListEventsResponse> {
    let url = events_page_url(remote, repo_id, after);
    let mut builder = ureq::get(&url);
    if let Some(token) = bearer {
        builder = builder.header("authorization", &format!("Bearer {token}"));
    }
    let mut response = builder
        .call()
        .with_context(|| format!("failed to GET events from {url}"))?;
    response
        .body_mut()
        .read_json::<ListEventsResponse>()
        .with_context(|| format!("failed to decode event list from {url}"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use brick_protocol::{
        ActorRef, ActorType, MissionCreatedPayload, MissionId, MissionStatus, ProjectId,
    };
    use chrono::Utc;

    use super::*;

    fn temp_repo_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-cli-sync-{name}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(path.join(".git")).expect("create fake git dir");
        path
    }

    fn event(title: &str) -> TraceEvent {
        TraceEvent::mission_created(
            ActorRef {
                actor_type: ActorType::Human,
                actor_id: "tester".to_string(),
                display_name: None,
            },
            MissionId::new(),
            MissionCreatedPayload {
                project_id: ProjectId::new(),
                title: title.to_string(),
                description: None,
                status: MissionStatus::Planned,
                repo_context_id: None,
            },
        )
        .expect("build event")
    }

    #[test]
    fn normalized_remote_removes_trailing_slashes() {
        assert_eq!(
            normalized_remote(Some("http://127.0.0.1:7821///".to_string())),
            "http://127.0.0.1:7821"
        );
    }

    #[test]
    fn repo_scoped_events_url_uses_repo_path() {
        assert_eq!(
            events_url("http://127.0.0.1:7821", Some("repo-a")),
            "http://127.0.0.1:7821/v1/repos/repo-a/events"
        );
        assert_eq!(
            events_page_url("http://127.0.0.1:7821", Some("repo-a"), Some("10")),
            "http://127.0.0.1:7821/v1/repos/repo-a/events?limit=500&after=10"
        );
    }

    #[test]
    fn source_session_payload_materializes_to_metadata_upsert() {
        let payload = source_session_payload();

        let upsert = source_session_upsert_from_payload(&payload).expect("upsert");

        assert_eq!(upsert.source_id, "codex");
        assert_eq!(upsert.external_session_id, "session-1");
        assert_eq!(upsert.source_path, Some(PathBuf::from("/local/blob")));
        assert_eq!(
            upsert.touched_files_json,
            Some(serde_json::json!(["src/lib.rs"]))
        );
        assert_eq!(payload.normalized_chunks.len(), 1);
    }

    #[test]
    fn pulled_source_session_materializes_normalized_chunks_to_metadata_db() {
        let path = temp_repo_root("pulled-chunks").join("metadata.sqlite");
        let mut metadata_db = MetadataDb::open_path(&path).expect("open metadata DB");
        let mut events = vec![TraceEvent::source_session_observed(
            ActorRef {
                actor_type: ActorType::Agent,
                actor_id: "codex".to_string(),
                display_name: None,
            },
            source_session_payload(),
        )
        .expect("build source session event")];

        ingest_source_sessions_into_metadata(&mut metadata_db, &mut events)
            .expect("ingest source session");

        let sessions = metadata_db
            .list_source_sessions(&brick_core::SourceSessionListQuery {
                source_id: Some("codex".to_string()),
                limit: 10,
                offset: 0,
            })
            .expect("list sessions");
        assert_eq!(sessions.len(), 1);
        let chunks = metadata_db
            .list_source_session_chunks("codex", "session-1")
            .expect("list chunks");
        assert_eq!(chunks.len(), 1);
        assert_eq!(
            chunks[0].result_json,
            serde_json::json!({"content": "done"})
        );
    }

    #[test]
    fn push_hydrates_normalized_chunks_from_metadata() {
        let path = temp_repo_root("push-hydrate").join("metadata.sqlite");
        let mut metadata_db = MetadataDb::open_path(&path).expect("open metadata DB");
        let payload = source_session_payload();
        metadata_db
            .upsert_source_session(&source_session_upsert_from_payload(&payload).expect("upsert"))
            .expect("upsert session");
        let chunks: Vec<ActivityChunk> = payload
            .normalized_chunks
            .iter()
            .cloned()
            .map(serde_json::from_value)
            .collect::<serde_json::Result<Vec<_>>>()
            .expect("decode chunks");
        metadata_db
            .upsert_source_session_chunks(&SourceSessionChunksUpsert {
                source_id: payload.source_id.clone(),
                external_session_id: payload.external_session_id.clone(),
                chunks,
            })
            .expect("upsert chunks");

        // Canonical event as stored in the local JSONL queue: no chunks inline.
        let mut stripped = payload.clone();
        stripped.normalized_chunks = Vec::new();
        let event = TraceEvent::source_session_observed(
            ActorRef {
                actor_type: ActorType::Agent,
                actor_id: "codex".to_string(),
                display_name: None,
            },
            stripped,
        )
        .expect("build event");

        let hydrated =
            hydrate_source_session_chunks_with(&metadata_db, vec![event]).expect("hydrate chunks");

        let normalized = hydrated[0].payload["normalized_chunks"]
            .as_array()
            .expect("normalized chunks present after hydrate");
        assert_eq!(normalized.len(), 1);
        assert_eq!(normalized[0]["result"]["content"], "done");
    }

    fn source_session_payload() -> SourceSessionObservedPayload {
        SourceSessionObservedPayload {
            source_id: "codex".to_string(),
            external_session_id: "session-1".to_string(),
            title: Some("Investigate".to_string()),
            source_path: Some("/local/blob".to_string()),
            source_uri: Some("file:///local/blob".to_string()),
            source_mtime: None,
            source_size: Some(5),
            source_fingerprint: Some("fingerprint".to_string()),
            parser_version: Some("parser".to_string()),
            session_created_at: None,
            session_updated_at: None,
            model: Some("model".to_string()),
            input_tokens: Some(1),
            output_tokens: Some(2),
            repo_path: Some("/repo".to_string()),
            branch: Some("main".to_string()),
            files_changed: Some(1),
            lines_added: Some(2),
            lines_removed: Some(0),
            touched_files: vec!["src/lib.rs".to_string()],
            metadata_json: None,
            normalized_chunks: vec![serde_json::json!({
                "chunk_id": "chunk-1",
                "session_id": "session-1",
                "action_type": "message",
                "function": "assistant",
                "args": {},
                "result": {"content": "done"},
                "created_at": "2026-01-01T00:00:00Z"
            })],
        }
    }

    #[test]
    fn dry_run_pull_dedupes_without_writing_inbound_events() {
        let repo_root = temp_repo_root("dry-run-pull");
        let store = LocalStore::new(&repo_root);
        let local_event = event("local");
        let remote_event = event("remote");
        store
            .append_event(&local_event)
            .expect("append local event");

        let outcome = pull_events(&store, vec![local_event, remote_event], true)
            .expect("pull dry run events");

        assert_eq!(outcome.remote_event_count, 2);
        assert_eq!(outcome.pulled_event_count, 1);
        assert_eq!(outcome.duplicate_count, 1);
        assert_eq!(outcome.inbound_path, None);
        assert!(store
            .read_inbound_events()
            .expect("read inbound")
            .is_empty());
    }

    #[test]
    fn auto_sync_env_controls_remote_and_disable_flag() {
        std::env::set_var(AUTO_SYNC_REMOTE_ENV, "http://127.0.0.1:7821///");
        assert_eq!(auto_sync_remote(), "http://127.0.0.1:7821");
        std::env::remove_var(AUTO_SYNC_REMOTE_ENV);

        std::env::remove_var(AUTO_SYNC_DISABLE_ENV);
        assert!(!auto_sync_disabled());
        std::env::set_var(AUTO_SYNC_DISABLE_ENV, "1");
        assert!(auto_sync_disabled());
        std::env::set_var(AUTO_SYNC_DISABLE_ENV, "true");
        assert!(auto_sync_disabled());
        std::env::set_var(AUTO_SYNC_DISABLE_ENV, "0");
        assert!(!auto_sync_disabled());
        std::env::remove_var(AUTO_SYNC_DISABLE_ENV);
    }

    #[test]
    fn scoped_events_tags_repo_and_org_for_upload() {
        let scoped = scoped_events(vec![event("queued")], Some("repo-a"), Some("org_shared"))
            .expect("scope events");

        assert_eq!(scoped[0].repo_id.as_deref(), Some("repo-a"));
        assert_eq!(
            scoped[0]
                .org_id
                .as_ref()
                .map(ToString::to_string)
                .as_deref(),
            Some("org_shared")
        );
    }

    #[test]
    fn push_dry_run_does_not_require_login() {
        // A dry-run push must work for anyone (it makes no network call and sends
        // no token), so it returns Ok even with no identity on disk.
        let repo_root = temp_repo_root("dry-run-push");
        let store = LocalStore::new(&repo_root);
        store.append_event(&event("queued")).expect("append");
        let result = handle_push(
            &store,
            true,
            Some("http://127.0.0.1:7821".to_string()),
            None,
            None,
        );
        assert!(
            result.is_ok(),
            "dry-run push must not require login: {result:?}"
        );
    }
}
