//! Append-only filesystem store for the self-hosted server prototype.
//!
//! This store keeps one JSONL event log as the server source of truth. Repo
//! scoping, pagination, and duplicate checks are all projections over that log;
//! authorization and tenant policy remain future concerns.
//!
//! One derived projection is persisted alongside the log: a repo→org map
//! (`repo_org.json`) maintained incrementally on append and cached in memory, so
//! org-scoped authorization resolves a repo's owning org in O(1) without
//! rescanning the event log per request.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};

use anyhow::{anyhow, Context, Result};
use brick_protocol::{EventCursor, ListEventsResponse, PushEventsResponse, TraceEvent};
use uuid::Uuid;

const SERVER_EVENTS_FILE: &str = "events.jsonl";
const REPO_ORG_FILE: &str = "repo_org.json";
const DEFAULT_EVENT_LIMIT: usize = 100;
const MAX_EVENT_LIMIT: usize = 1000;

/// Filesystem-backed event store for the self-hosted trace server.
///
/// Cloning shares the same in-memory repo→org projection cache (via `Arc`), so
/// all handlers and the auth gate that hold clones of one store observe a single
/// consistent view.
#[derive(Debug, Clone)]
pub struct ServerStore {
    data_dir: PathBuf,
    repo_org: Arc<RwLock<RepoOrgProjection>>,
}

/// In-memory cache of the persisted repo→org projection. `loaded` is `None`
/// until first populated (lazily, from `repo_org.json` or by rebuilding from the
/// event log), then holds the full map for the process lifetime.
#[derive(Debug, Default)]
struct RepoOrgProjection {
    loaded: Option<BTreeMap<String, String>>,
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
            repo_org: Arc::new(RwLock::new(RepoOrgProjection::default())),
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
        let mut accepted_repo_orgs: Vec<(String, String)> = Vec::new();
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
            if let (Some(repo), Some(org)) = (event.repo_id.as_deref(), event.org_id.as_ref()) {
                accepted_repo_orgs.push((repo.to_string(), org.to_string()));
            }
            accepted_event_ids.push(event.event_id);
        }

        // Keep the persisted repo→org projection in step with the log so
        // org-scoped authorization never has to rescan events at request time.
        if !accepted_repo_orgs.is_empty() {
            self.record_repo_orgs(accepted_repo_orgs)?;
        }

        Ok(PushEventsResponse {
            accepted_event_ids,
            duplicate_event_ids,
        })
    }

    /// Resolves a repo's owning org id from the persisted repo→org projection.
    ///
    /// The projection is loaded once (from `repo_org.json`, or rebuilt from the
    /// event log if the file is missing) and cached for the process lifetime, so
    /// steady-state resolution is an O(1) in-memory lookup. Returns `None` when
    /// no stored event ties the repo to an org. Best-effort: any I/O error
    /// yields `None` rather than failing, since this feeds an auth decision that
    /// must stay deny-by-default.
    pub fn resolve_repo_org(&self, repo_id: &str) -> Option<String> {
        // Fast path: cache already populated.
        if let Ok(guard) = self.repo_org.read() {
            if let Some(map) = guard.loaded.as_ref() {
                return map.get(repo_id).cloned();
            }
        }
        // Slow path: populate the cache once, then read from it.
        self.ensure_repo_org_loaded().ok()?;
        let guard = self.repo_org.read().ok()?;
        guard.loaded.as_ref()?.get(repo_id).cloned()
    }

    /// Populates the in-memory repo→org cache if it has not been loaded yet,
    /// preferring the persisted file and falling back to rebuilding from the log
    /// (which also writes the file so subsequent starts are cheap).
    fn ensure_repo_org_loaded(&self) -> Result<()> {
        {
            let guard = self
                .repo_org
                .read()
                .map_err(|_| anyhow!("repo→org cache lock poisoned"))?;
            if guard.loaded.is_some() {
                return Ok(());
            }
        }
        let map = match self.load_repo_org_file()? {
            Some(map) => map,
            None => {
                let rebuilt = self.rebuild_repo_org_from_log()?;
                self.write_repo_org_file(&rebuilt)?;
                rebuilt
            }
        };
        let mut guard = self
            .repo_org
            .write()
            .map_err(|_| anyhow!("repo→org cache lock poisoned"))?;
        // Another thread may have loaded it while we read the disk; only set if
        // still empty so we don't clobber a freshly-recorded entry.
        if guard.loaded.is_none() {
            guard.loaded = Some(map);
        }
        Ok(())
    }

    /// Records repo→org bindings for newly accepted events into both the cache
    /// (if loaded) and the persisted file. First binding for a repo wins, mirror
    /// of the historical "first event ties the repo to an org" semantics.
    fn record_repo_orgs(&self, bindings: Vec<(String, String)>) -> Result<()> {
        self.ensure_repo_org_loaded()?;
        let mut guard = self
            .repo_org
            .write()
            .map_err(|_| anyhow!("repo→org cache lock poisoned"))?;
        let map = guard.loaded.get_or_insert_with(BTreeMap::new);
        let mut changed = false;
        for (repo, org) in bindings {
            map.entry(repo).or_insert_with(|| {
                changed = true;
                org
            });
        }
        if changed {
            self.write_repo_org_file(map)?;
        }
        Ok(())
    }

    /// Reads the persisted repo→org file, returning `None` when it is absent so
    /// the caller can rebuild from the log.
    fn load_repo_org_file(&self) -> Result<Option<BTreeMap<String, String>>> {
        let path = self.repo_org_path();
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read repo→org map at {}", path.display()))?;
        if contents.trim().is_empty() {
            return Ok(Some(BTreeMap::new()));
        }
        let map = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse repo→org map at {}", path.display()))?;
        Ok(Some(map))
    }

    /// Writes the repo→org file atomically (temp file + rename) so a crash
    /// mid-write cannot leave a truncated map.
    fn write_repo_org_file(&self, map: &BTreeMap<String, String>) -> Result<()> {
        self.init()?;
        let path = self.repo_org_path();
        let tmp = path.with_extension("json.tmp");
        let serialized =
            serde_json::to_string_pretty(map).context("failed to serialize repo→org map")?;
        fs::write(&tmp, serialized)
            .with_context(|| format!("failed to write repo→org map at {}", tmp.display()))?;
        fs::rename(&tmp, &path)
            .with_context(|| format!("failed to commit repo→org map at {}", path.display()))?;
        Ok(())
    }

    /// Rebuilds the repo→org map from the full event log (first org per repo
    /// wins), used once when no persisted file exists yet.
    fn rebuild_repo_org_from_log(&self) -> Result<BTreeMap<String, String>> {
        let mut map = BTreeMap::new();
        for entry in self.read_sequenced_events()? {
            if let (Some(repo), Some(org)) =
                (entry.event.repo_id.as_deref(), entry.event.org_id.as_ref())
            {
                map.entry(repo.to_string())
                    .or_insert_with(|| org.to_string());
            }
        }
        Ok(map)
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

    fn repo_org_path(&self) -> PathBuf {
        self.data_dir.join(REPO_ORG_FILE)
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
    fn resolve_repo_org_finds_owning_org_for_repo() {
        use brick_protocol::OrgId;
        let store = ServerStore::new(temp_data_dir("repo-org"));
        let mut tagged = event("tagged", Some("repo-a"));
        tagged.org_id = Some(OrgId::new());
        let expected = tagged.org_id.as_ref().map(ToString::to_string);
        let untagged = event("untagged", Some("repo-b"));
        store
            .append_events(&[tagged, untagged])
            .expect("append events");

        assert_eq!(store.resolve_repo_org("repo-a"), expected);
        assert_eq!(store.resolve_repo_org("repo-b"), None);
        assert_eq!(store.resolve_repo_org("repo-missing"), None);
    }

    #[test]
    fn repo_org_projection_persists_and_reloads_without_rescanning_log() {
        use brick_protocol::OrgId;
        let dir = temp_data_dir("repo-org-persist");
        let store = ServerStore::new(&dir);
        let mut tagged = event("tagged", Some("repo-a"));
        let org = OrgId::new();
        tagged.org_id = Some(org.clone());
        store.append_events(&[tagged]).expect("append");

        // The projection is written to disk on append.
        assert!(dir.join(REPO_ORG_FILE).exists());

        // A brand-new store handle (cold process restart) loads the persisted
        // file. Delete the event log first to prove resolution does not rescan
        // events — it must answer purely from the persisted projection.
        std::fs::remove_file(dir.join(SERVER_EVENTS_FILE)).expect("remove log");
        let reopened = ServerStore::new(&dir);
        assert_eq!(reopened.resolve_repo_org("repo-a"), Some(org.to_string()));
    }

    #[test]
    fn repo_org_projection_rebuilds_from_log_when_file_absent() {
        use brick_protocol::OrgId;
        let dir = temp_data_dir("repo-org-rebuild");
        // Seed an event log directly, with no projection file (legacy data dir).
        let store = ServerStore::new(&dir);
        let mut tagged = event("tagged", Some("repo-a"));
        let org = OrgId::new();
        tagged.org_id = Some(org.clone());
        store.append_events(&[tagged]).expect("append");
        std::fs::remove_file(dir.join(REPO_ORG_FILE)).expect("remove projection");

        // A fresh handle with only the log present rebuilds the projection and
        // writes it back so later starts are cheap.
        let reopened = ServerStore::new(&dir);
        assert_eq!(reopened.resolve_repo_org("repo-a"), Some(org.to_string()));
        assert!(dir.join(REPO_ORG_FILE).exists());
    }

    #[test]
    fn repo_org_projection_keeps_first_org_per_repo() {
        use brick_protocol::OrgId;
        let store = ServerStore::new(temp_data_dir("repo-org-first-wins"));
        let mut first = event("first", Some("repo-a"));
        let first_org = OrgId::new();
        first.org_id = Some(first_org.clone());
        let mut second = event("second", Some("repo-a"));
        second.org_id = Some(OrgId::new());
        store.append_events(&[first]).expect("append first");
        store.append_events(&[second]).expect("append second");

        assert_eq!(
            store.resolve_repo_org("repo-a"),
            Some(first_org.to_string())
        );
    }

    #[test]
    fn cloned_store_shares_repo_org_cache() {
        use brick_protocol::OrgId;
        let store = ServerStore::new(temp_data_dir("repo-org-clone"));
        let clone = store.clone();
        let mut tagged = event("tagged", Some("repo-a"));
        let org = OrgId::new();
        tagged.org_id = Some(org.clone());
        // Append through one handle; the clone observes it via the shared cache.
        store.append_events(&[tagged]).expect("append");
        assert_eq!(clone.resolve_repo_org("repo-a"), Some(org.to_string()));
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
