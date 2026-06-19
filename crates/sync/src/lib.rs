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

pub mod wire;

use anyhow::{Context, Result};
use brick_core::LocalStore;
use brick_protocol::TraceEvent;

pub use wire::{EventCursor, ListEventsResponse, PushEventsRequest, PushEventsResponse};

const DEFAULT_REMOTE: &str = "http://127.0.0.1:7821";
const PULL_PAGE_LIMIT: usize = 500;

/// Handles `push` by posting queued events without draining the local queue.
pub fn handle_push(
    store: &LocalStore,
    dry_run: bool,
    remote: Option<String>,
    repo_id: Option<String>,
) -> Result<()> {
    let events = store.read_queued_events()?;
    let remote = normalized_remote(remote);
    if dry_run {
        println!("push_dry_run=true");
        println!("remote={remote}");
        println!("repo_id={}", repo_id.as_deref().unwrap_or(""));
        println!("queued_event_count={}", events.len());
        return Ok(());
    }

    let request = PushEventsRequest { events };
    let response = push_events_to_remote(&remote, repo_id.as_deref(), &request)?;
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
    let response = get_all_events_from_remote(&remote, repo_id.as_deref())?;
    let outcome = pull_events(store, response.events, dry_run)?;
    print_pull_result(&remote, repo_id.as_deref(), dry_run, &outcome);
    Ok(())
}

/// Handles `sync` as pull followed by non-draining push against the same remote.
pub fn handle_sync(
    store: &LocalStore,
    dry_run: bool,
    remote: Option<String>,
    repo_id: Option<String>,
) -> Result<()> {
    let remote = normalized_remote(remote);
    handle_pull(store, dry_run, Some(remote.clone()), repo_id.clone())?;
    handle_push(store, dry_run, Some(remote), repo_id)?;
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
    let pulled_event_count = pulled_events.len();
    let duplicate_count = remote_event_count.saturating_sub(pulled_event_count);
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
) -> Result<PushEventsResponse> {
    let url = events_url(remote, repo_id);
    let mut response = ureq::post(&url)
        .header("content-type", "application/json")
        .send_json(request)
        .with_context(|| format!("failed to POST events to {url}"))?;
    response
        .body_mut()
        .read_json::<PushEventsResponse>()
        .with_context(|| format!("failed to decode push response from {url}"))
}

fn get_all_events_from_remote(remote: &str, repo_id: Option<&str>) -> Result<ListEventsResponse> {
    let mut events = Vec::new();
    let mut after: Option<EventCursor> = None;
    loop {
        let response = get_events_page_from_remote(remote, repo_id, after.as_deref())?;
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
) -> Result<ListEventsResponse> {
    let url = events_page_url(remote, repo_id, after);
    let mut response = ureq::get(&url)
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
}
