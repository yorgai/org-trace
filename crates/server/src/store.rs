//! Append-only filesystem store for the self-hosted server prototype.
//!
//! This store keeps one JSONL event log as the server source of truth. Repo
//! scoping, pagination, and duplicate checks are all projections over that log;
//! authorization and tenant policy remain future concerns.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use brick_protocol::{EventCursor, ListEventsResponse, PushEventsResponse, TraceEvent};
use uuid::Uuid;

const SERVER_EVENTS_FILE: &str = "events.jsonl";
const DEFAULT_EVENT_LIMIT: usize = 100;
const MAX_EVENT_LIMIT: usize = 1000;

/// Filesystem-backed event store for the self-hosted trace server.
#[derive(Debug, Clone)]
pub struct ServerStore {
    data_dir: PathBuf,
}

#[derive(Debug, Clone)]
struct SequencedEvent {
    sequence: u64,
    event: TraceEvent,
}

impl ServerStore {
    /// Creates a store handle rooted at `data_dir` without touching disk.
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
        }
    }

    /// Ensures the server data directory exists.
    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(&self.data_dir).with_context(|| {
            format!(
                "failed to create server data directory at {}",
                self.data_dir.display()
            )
        })?;
        Ok(())
    }

    /// Returns all events currently stored by the server.
    pub fn read_events(&self) -> Result<Vec<TraceEvent>> {
        Ok(self
            .read_sequenced_events()?
            .into_iter()
            .map(|entry| entry.event)
            .collect())
    }

    /// Returns events for one repo, or all events when `repo_id` is `None`.
    pub fn read_events_for_repo(&self, repo_id: Option<&str>) -> Result<Vec<TraceEvent>> {
        Ok(self
            .read_sequenced_events()?
            .into_iter()
            .filter(|entry| repo_matches(entry.event.repo_id.as_deref(), repo_id))
            .map(|entry| entry.event)
            .collect())
    }

    /// Returns a cursor page scoped to the requested repo.
    pub fn list_events_page(
        &self,
        repo_id: Option<&str>,
        after: Option<&str>,
        limit: Option<usize>,
    ) -> Result<ListEventsResponse> {
        let after_sequence = parse_cursor(after)?;
        let normalized_limit = limit
            .unwrap_or(DEFAULT_EVENT_LIMIT)
            .clamp(1, MAX_EVENT_LIMIT);
        let mut page = Vec::new();
        let mut last_sequence = None;
        let mut has_more = false;

        for entry in self.read_sequenced_events()? {
            if entry.sequence <= after_sequence {
                continue;
            }
            if !repo_matches(entry.event.repo_id.as_deref(), repo_id) {
                continue;
            }
            if page.len() == normalized_limit {
                has_more = true;
                break;
            }
            last_sequence = Some(entry.sequence);
            page.push(entry.event);
        }

        let next_cursor = if has_more {
            last_sequence.map(|sequence| sequence.to_string())
        } else {
            None
        };

        Ok(ListEventsResponse::page(
            page,
            after.map(ToString::to_string),
            next_cursor,
        ))
    }

    /// Appends events that are not already present by event ID.
    pub fn append_events(&self, events: &[TraceEvent]) -> Result<PushEventsResponse> {
        self.append_events_for_repo(None, events)
    }

    /// Appends events under an optional route repo boundary.
    ///
    /// When a repo route is used, events with no `repo_id` are filled with the
    /// route value. Events with a different `repo_id` are rejected before any
    /// append occurs so callers cannot accidentally cross repo boundaries.
    pub fn append_events_for_repo(
        &self,
        repo_id: Option<&str>,
        events: &[TraceEvent],
    ) -> Result<PushEventsResponse> {
        self.init()?;
        let events = normalize_repo_events(repo_id, events)?;
        let existing_event_ids = self
            .read_events()?
            .into_iter()
            .map(|event| event.event_id)
            .collect::<BTreeSet<_>>();
        let path = self.events_path();
        let mut accepted_event_ids = Vec::new();
        let mut duplicate_event_ids = Vec::new();
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open server event log at {}", path.display()))?;

        let mut seen_in_request = BTreeSet::<Uuid>::new();
        for event in events {
            if existing_event_ids.contains(&event.event_id)
                || !seen_in_request.insert(event.event_id)
            {
                duplicate_event_ids.push(event.event_id);
                continue;
            }
            let serialized =
                serde_json::to_string(&event).context("failed to serialize pushed event")?;
            writeln!(file, "{serialized}").context("failed to append pushed event")?;
            accepted_event_ids.push(event.event_id);
        }

        Ok(PushEventsResponse {
            accepted_event_ids,
            duplicate_event_ids,
        })
    }

    fn read_sequenced_events(&self) -> Result<Vec<SequencedEvent>> {
        let path = self.events_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read server event log at {}", path.display()))?;
        let mut events = Vec::new();
        for (line_index, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event = serde_json::from_str::<TraceEvent>(line).with_context(|| {
                format!(
                    "failed to parse server event log at {} line {}",
                    path.display(),
                    line_index + 1
                )
            })?;
            events.push(SequencedEvent {
                sequence: (line_index + 1) as u64,
                event,
            });
        }
        Ok(events)
    }

    fn events_path(&self) -> PathBuf {
        self.data_dir.join(SERVER_EVENTS_FILE)
    }
}

fn parse_cursor(cursor: Option<&str>) -> Result<u64> {
    match cursor {
        Some(value) if !value.trim().is_empty() => value
            .parse::<u64>()
            .with_context(|| format!("invalid event cursor {value:?}")),
        _ => Ok(0),
    }
}

fn normalize_repo_events(repo_id: Option<&str>, events: &[TraceEvent]) -> Result<Vec<TraceEvent>> {
    let Some(route_repo_id) = repo_id else {
        return Ok(events.to_vec());
    };

    events
        .iter()
        .map(|event| {
            let mut scoped = event.clone();
            match scoped.repo_id.as_deref() {
                Some(event_repo_id) if event_repo_id != route_repo_id => Err(anyhow!(
                    "event {} repo_id {event_repo_id:?} does not match route repo_id {route_repo_id:?}",
                    event.event_id
                )),
                Some(_) => Ok(scoped),
                None => {
                    scoped.repo_id = Some(route_repo_id.to_string());
                    Ok(scoped)
                }
            }
        })
        .collect()
}

fn repo_matches(event_repo_id: Option<&str>, filter_repo_id: Option<&str>) -> bool {
    match filter_repo_id {
        Some(repo_id) => event_repo_id == Some(repo_id),
        None => true,
    }
}

#[allow(dead_code)]
fn _cursor_type_boundary(_: EventCursor) {}

#[cfg(test)]
mod tests {
    use brick_protocol::{
        ActorRef, ActorType, MissionCreatedPayload, MissionId, MissionStatus, ProjectId,
    };
    use chrono::Utc;

    use super::*;

    fn temp_data_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "brick-server-{name}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn event(title: &str, repo_id: Option<&str>) -> TraceEvent {
        let mut event = TraceEvent::mission_created(
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
        .expect("mission event");
        event.repo_id = repo_id.map(ToString::to_string);
        event
    }

    #[test]
    fn append_events_deduplicates_by_event_id() {
        let store = ServerStore::new(temp_data_dir("dedupe"));
        let event = event("one", None);
        let first = store
            .append_events(&[event.clone()])
            .expect("append first event");
        let second = store
            .append_events(&[event])
            .expect("append duplicate event");

        assert_eq!(first.accepted_count(), 1);
        assert_eq!(first.duplicate_count(), 0);
        assert_eq!(second.accepted_count(), 0);
        assert_eq!(second.duplicate_count(), 1);
        assert_eq!(store.read_events().expect("read events").len(), 1);
    }

    #[test]
    fn repo_scoped_append_fills_missing_repo_and_rejects_mismatch() {
        let store = ServerStore::new(temp_data_dir("repo-fill"));
        let unscoped = event("unscoped", None);
        let mismatched = event("mismatch", Some("other"));

        let response = store
            .append_events_for_repo(Some("repo-a"), &[unscoped])
            .expect("append scoped event");
        let events = store.read_events().expect("read events");

        assert_eq!(response.accepted_count(), 1);
        assert_eq!(events[0].repo_id.as_deref(), Some("repo-a"));
        assert!(store
            .append_events_for_repo(Some("repo-a"), &[mismatched])
            .is_err());
    }

    #[test]
    fn list_events_page_filters_by_repo_and_cursor() {
        let store = ServerStore::new(temp_data_dir("pagination"));
        let first = event("first", Some("repo-a"));
        let second = event("second", Some("repo-b"));
        let third = event("third", Some("repo-a"));
        let fourth = event("fourth", Some("repo-a"));
        store
            .append_events(&[first.clone(), second, third.clone(), fourth.clone()])
            .expect("append events");

        let first_page = store
            .list_events_page(Some("repo-a"), None, Some(2))
            .expect("read first page");
        let second_page = store
            .list_events_page(Some("repo-a"), first_page.next_cursor.as_deref(), Some(2))
            .expect("read second page");

        assert_eq!(first_page.events, vec![first, third]);
        assert_eq!(first_page.next_cursor.as_deref(), Some("3"));
        assert_eq!(second_page.events, vec![fourth]);
        assert_eq!(second_page.next_cursor, None);
    }
}
