//! Filesystem-backed local store for Brick.
//!
//! The queue directory is the durable append-only write path. Cache files are
//! derived from the queue and may be rebuilt at any time.

use std::collections::BTreeSet;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use brick_protocol::TraceEvent;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    rebuild_sqlite_index, resolve_storage_root, sqlite_index_status, AttachmentStore,
    CurrentContext, IndexStatus, ResolvedStorageRoot, SqliteIndexStatus, StorageOptions,
    TraceIndex, CACHE_DIR, CURRENT_CONTEXT_FILE, EVENTS_DIR, INDEX_CACHE_FILE, PROVENANCE_DIR,
    QUEUE_DIR, REPO_CONFIG_FILE, SQLITE_INDEX_FILE, VIEWS_DIR,
};

/// Repository-local provenance configuration written during initialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoConfig {
    pub repo_root: String,
    pub created_at: String,
}

fn ensure_brick_gitignore(repo_root: &Path) -> Result<()> {
    let gitignore_path = repo_root.join(".gitignore");
    let existing = match fs::read_to_string(&gitignore_path) {
        Ok(contents) => contents,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(err) => {
            return Err(err).with_context(|| {
                format!("failed to read .gitignore at {}", gitignore_path.display())
            });
        }
    };

    if existing.lines().any(|line| line.trim() == ".brick/") {
        return Ok(());
    }

    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&gitignore_path)
        .with_context(|| format!("failed to open .gitignore at {}", gitignore_path.display()))?;
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file).context("failed to terminate existing .gitignore line")?;
    }
    writeln!(file, ".brick/").context("failed to append .brick/ to .gitignore")?;
    Ok(())
}

/// Queue health summary for status output and tests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueueStatus {
    pub initialized: bool,
    pub queued_event_count: usize,
    pub queue_files: usize,
}

/// Filesystem store for one Git repository and one effective storage root.
#[derive(Debug, Clone)]
pub struct LocalStore {
    repo_root: PathBuf,
    storage_root: PathBuf,
    storage_root_resolution: ResolvedStorageRoot,
}

impl LocalStore {
    /// Creates a store handle with the default `.brick/provenance` storage root.
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        let repo_root = repo_root.into();
        let storage_root = repo_root.join(PROVENANCE_DIR);
        Self {
            repo_root,
            storage_root: storage_root.clone(),
            storage_root_resolution: ResolvedStorageRoot {
                path: storage_root,
                source: crate::StorageRootSource::RepoDefault,
            },
        }
    }

    /// Creates a store handle after resolving storage options against the repository.
    pub fn with_options(repo_root: impl Into<PathBuf>, options: StorageOptions) -> Result<Self> {
        let repo_root = repo_root.into();
        let storage_root_resolution = resolve_storage_root(&repo_root, &options)?;
        Ok(Self {
            repo_root,
            storage_root: storage_root_resolution.path.clone(),
            storage_root_resolution,
        })
    }

    /// Returns the Git repository root backing this store.
    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    /// Returns the effective storage root for provenance data.
    pub fn storage_root(&self) -> &Path {
        &self.storage_root
    }

    /// Returns how the effective storage root was selected.
    pub fn storage_root_resolution(&self) -> &ResolvedStorageRoot {
        &self.storage_root_resolution
    }

    /// Returns the effective provenance directory for this store.
    pub fn provenance_dir(&self) -> PathBuf {
        self.storage_root.clone()
    }

    /// Returns the durable JSONL event queue directory.
    pub fn queue_dir(&self) -> PathBuf {
        self.provenance_dir().join(QUEUE_DIR)
    }

    /// Returns the directory containing inbound events pulled from remotes.
    pub fn inbound_events_dir(&self) -> PathBuf {
        self.provenance_dir().join(EVENTS_DIR).join("inbound")
    }

    /// Returns the directory for rebuildable local cache files.
    pub fn cache_dir(&self) -> PathBuf {
        self.provenance_dir().join(CACHE_DIR)
    }

    /// Returns the path to the derived inspection index cache.
    pub fn index_path(&self) -> PathBuf {
        self.cache_dir().join(INDEX_CACHE_FILE)
    }

    /// Returns the directory containing rebuildable agent-readable views.
    pub fn views_dir(&self) -> PathBuf {
        self.provenance_dir().join(VIEWS_DIR)
    }

    /// Returns the path to the rebuildable SQLite query cache.
    pub fn sqlite_index_path(&self) -> PathBuf {
        self.cache_dir().join(SQLITE_INDEX_FILE)
    }

    /// Returns the content-addressed attachment store for this storage root.
    pub fn attachment_store(&self) -> AttachmentStore {
        AttachmentStore::new(self.storage_root.clone())
    }

    /// Returns the session log store backed by shared content-addressed blobs.
    pub fn log_store(&self) -> crate::LogStore {
        crate::LogStore::new(self.storage_root.clone())
    }

    /// Creates local provenance directories and repo metadata if missing.
    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(self.queue_dir())
            .context("failed to create provenance queue directory")?;
        fs::create_dir_all(self.inbound_events_dir())
            .context("failed to create provenance inbound events directory")?;
        fs::create_dir_all(self.provenance_dir().join(CACHE_DIR))
            .context("failed to create provenance cache directory")?;

        let repo_config_path = self.provenance_dir().join(REPO_CONFIG_FILE);
        if !repo_config_path.exists() {
            let config = RepoConfig {
                repo_root: self.repo_root.display().to_string(),
                created_at: Utc::now().to_rfc3339(),
            };
            let serialized =
                serde_json::to_string_pretty(&config).context("failed to serialize repo config")?;
            fs::write(&repo_config_path, serialized).with_context(|| {
                format!(
                    "failed to write repo config at {}",
                    repo_config_path.display()
                )
            })?;
        }

        ensure_brick_gitignore(&self.repo_root)?;

        Ok(())
    }

    /// Appends one event to today's JSONL queue file.
    pub fn append_event(&self, event: &TraceEvent) -> Result<PathBuf> {
        self.init()?;

        let date = Utc::now().format("%Y-%m-%d");
        let path = self.queue_dir().join(format!("{date}.jsonl"));
        let serialized = serde_json::to_string(event).context("failed to serialize trace event")?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open event queue at {}", path.display()))?;
        writeln!(file, "{serialized}").context("failed to append trace event")?;
        Ok(path)
    }

    /// Reads all queued JSONL events in deterministic file order.
    pub fn read_queued_events(&self) -> Result<Vec<TraceEvent>> {
        read_jsonl_events(&self.queue_dir(), "queue")
    }

    /// Reads inbound remote JSONL events in deterministic file order.
    pub fn read_inbound_events(&self) -> Result<Vec<TraceEvent>> {
        read_jsonl_events(&self.inbound_events_dir(), "inbound events")
    }

    /// Reads local queued events and inbound remote events as a deduped stream.
    pub fn read_all_events(&self) -> Result<Vec<TraceEvent>> {
        let mut events = self.read_queued_events()?;
        let mut known_event_ids = event_id_set(&events);
        for event in self.read_inbound_events()? {
            if known_event_ids.insert(event.event_id) {
                events.push(event);
            }
        }
        Ok(events)
    }

    /// Returns all locally known event IDs across queued and inbound events.
    pub fn known_event_ids(&self) -> Result<BTreeSet<Uuid>> {
        Ok(event_id_set(&self.read_all_events()?))
    }

    /// Returns remote events whose IDs are not already known locally.
    pub fn dedupe_remote_events(&self, remote_events: Vec<TraceEvent>) -> Result<Vec<TraceEvent>> {
        let mut known_event_ids = self.known_event_ids()?;
        Ok(remote_events
            .into_iter()
            .filter(|event| known_event_ids.insert(event.event_id))
            .collect())
    }

    /// Appends inbound remote events to a separate JSONL log without touching the local queue.
    pub fn append_inbound_events(&self, events: &[TraceEvent]) -> Result<PathBuf> {
        self.init()?;
        let date = Utc::now().format("%Y-%m-%d");
        let path = self.inbound_events_dir().join(format!("{date}.jsonl"));
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open inbound event log at {}", path.display()))?;
        for event in events {
            let serialized =
                serde_json::to_string(event).context("failed to serialize inbound trace event")?;
            writeln!(file, "{serialized}").context("failed to append inbound trace event")?;
        }
        Ok(path)
    }

    /// Returns the newest known events after loading local and inbound event logs.
    pub fn recent_events(&self, limit: usize) -> Result<Vec<TraceEvent>> {
        let mut events = self.read_all_events()?;
        let keep = limit.min(events.len());
        let start = events.len().saturating_sub(keep);
        Ok(events.drain(start..).collect())
    }

    /// Summarizes initialization and pending queue state.
    pub fn queue_status(&self) -> Result<QueueStatus> {
        let initialized = self.provenance_dir().exists();
        if !initialized {
            return Ok(QueueStatus {
                initialized,
                queued_event_count: 0,
                queue_files: 0,
            });
        }

        let paths = jsonl_paths(&self.queue_dir(), "queue")?;
        let queue_files = paths.len();
        let queued_event_count = self.read_queued_events()?.len();

        Ok(QueueStatus {
            initialized,
            queued_event_count,
            queue_files,
        })
    }

    /// Reads persisted identity context for commands that omit explicit flags.
    pub fn read_current_context(&self) -> Result<Option<CurrentContext>> {
        let path = self.provenance_dir().join(CURRENT_CONTEXT_FILE);
        if !path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read current context at {}", path.display()))?;
        let context = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse current context at {}", path.display()))?;
        Ok(Some(context))
    }

    /// Persists identity context for follow-up local commands.
    pub fn write_current_context(&self, context: &CurrentContext) -> Result<()> {
        self.init()?;
        let path = self.provenance_dir().join(CURRENT_CONTEXT_FILE);
        let serialized =
            serde_json::to_string_pretty(context).context("failed to serialize current context")?;
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write current context at {}", path.display()))?;
        Ok(())
    }

    /// Rebuilds the derived inspection index from local and inbound events.
    pub fn rebuild_index(&self) -> Result<TraceIndex> {
        self.init()?;
        let events = self.read_all_events()?;
        let index = TraceIndex::build(&events)?;
        let serialized =
            serde_json::to_string_pretty(&index).context("failed to serialize trace index")?;
        let path = self.index_path();
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write trace index at {}", path.display()))?;
        self.write_agent_views(&index)?;
        Ok(index)
    }

    /// Rebuilds Markdown views intended for direct human and agent inspection.
    pub fn write_agent_views(&self, index: &TraceIndex) -> Result<()> {
        let views_dir = self.views_dir();
        if views_dir.exists() {
            fs::remove_dir_all(&views_dir).with_context(|| {
                format!("failed to reset views directory {}", views_dir.display())
            })?;
        }
        fs::create_dir_all(&views_dir)
            .with_context(|| format!("failed to create views directory {}", views_dir.display()))?;
        fs::create_dir_all(views_dir.join("orgs"))?;
        fs::create_dir_all(views_dir.join("projects"))?;
        fs::create_dir_all(views_dir.join("missions"))?;
        fs::create_dir_all(views_dir.join("sessions"))?;
        fs::create_dir_all(views_dir.join("artifacts"))?;

        fs::write(
            views_dir.join("README.md"),
            format!(
                "# Brick Views\n\nSchema version: {}\nEvents: {}\nRebuilt at: {}\n\nThese files are derived from the JSONL event queue. Delete and rebuild them at any time.\n",
                index.schema_version,
                index.event_count,
                index.rebuilt_at.to_rfc3339()
            ),
        )?;

        for org in index.orgs.values() {
            fs::write(
                views_dir.join("orgs").join(format!("{}.md", org.org_id)),
                format!(
                    "# {}\n\nOrg ID: {}\nDescription: {}\nProjects: {}\nRepo contexts: {}\nUpdated: {}\n",
                    org.name.as_deref().unwrap_or("Untitled org"),
                    org.org_id,
                    org.description.as_deref().unwrap_or(""),
                    join_set(&org.project_ids),
                    join_set(&org.repo_context_ids),
                    org.last_event_at.to_rfc3339()
                ),
            )?;
        }
        for project in index.projects.values() {
            fs::write(
                views_dir
                    .join("projects")
                    .join(format!("{}.md", project.project_id)),
                format!(
                    "# {}\n\nProject ID: {}\nOrg ID: {}\nDescription: {}\nMissions: {}\nRepo contexts: {}\nUpdated: {}\n",
                    project.name.as_deref().unwrap_or("Untitled project"),
                    project.project_id,
                    project.org_id.as_deref().unwrap_or(""),
                    project.description.as_deref().unwrap_or(""),
                    join_set(&project.mission_ids),
                    join_set(&project.repo_context_ids),
                    project.last_event_at.to_rfc3339()
                ),
            )?;
        }
        for mission in index.missions.values() {
            fs::write(
                views_dir
                    .join("missions")
                    .join(format!("{}.md", mission.mission_id)),
                format!(
                    "# {}\n\nMission ID: {}\nProject ID: {}\nStatus: {:?}\nDescription: {}\nSessions: {}\nArtifacts: {}\nRepo contexts: {}\nUpdated: {}\n",
                    mission.title.as_deref().unwrap_or("Untitled mission"),
                    mission.mission_id,
                    mission.project_id.as_deref().unwrap_or(""),
                    mission.status,
                    mission.description.as_deref().unwrap_or(""),
                    join_set(&mission.session_ids),
                    join_set(&mission.artifact_ids),
                    join_set(&mission.repo_context_ids),
                    mission.last_event_at.to_rfc3339()
                ),
            )?;
        }
        for session in index.sessions.values() {
            fs::write(
                views_dir
                    .join("sessions")
                    .join(format!("{}.md", session.session_id)),
                format!(
                    "# {}\n\nSession ID: {}\nActor: {} ({:?})\nApp: {}\nApp session ID: {}\nMissions: {}\nArtifacts: {}\nLogs: {}\nRepo contexts: {}\nUpdated: {}\n",
                    session.session_name.as_deref().unwrap_or("Untitled session"),
                    session.session_id,
                    session.actor_id.as_deref().unwrap_or(""),
                    session.actor_type,
                    session.source.app_id.as_deref().unwrap_or(""),
                    session.source.app_session_id.as_deref().unwrap_or(""),
                    join_set(&session.mission_ids),
                    join_set(&session.artifact_ids),
                    join_set(&session.log_ref_ids),
                    join_set(&session.repo_context_ids),
                    session.last_event_at.to_rfc3339()
                ),
            )?;
        }
        for artifact in index.artifacts.values() {
            fs::write(
                views_dir
                    .join("artifacts")
                    .join(format!("{}.md", artifact.artifact_id)),
                format!(
                    "# {}\n\nArtifact ID: {}\nKind: {:?}\nMissions: {}\nSessions: {}\nFiles: {}\nAttachments: {}\nDiffs: {}\nRepo contexts: {}\nUpdated: {}\n\n{}\n",
                    artifact.title.as_deref().unwrap_or("Untitled artifact"),
                    artifact.artifact_id,
                    artifact.artifact_kind,
                    join_set(&artifact.mission_ids),
                    join_set(&artifact.session_ids),
                    join_set(&artifact.file_paths),
                    join_set(&artifact.attachment_ids),
                    join_set(&artifact.diff_ids),
                    join_set(&artifact.repo_context_ids),
                    artifact.last_event_at.to_rfc3339(),
                    artifact.body.as_deref().unwrap_or("")
                ),
            )?;
        }
        Ok(())
    }

    /// Reads the cached inspection index if it has already been built.
    pub fn read_index(&self) -> Result<Option<TraceIndex>> {
        let path = self.index_path();
        if !path.exists() {
            return Ok(None);
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read trace index at {}", path.display()))?;
        let index = serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse trace index at {}", path.display()))?;
        Ok(Some(index))
    }

    /// Loads the inspection index, rebuilding it when the cache is absent.
    pub fn load_or_rebuild_index(&self) -> Result<TraceIndex> {
        match self.read_index()? {
            Some(index) => Ok(index),
            None => self.rebuild_index(),
        }
    }

    /// Returns cache metadata without rebuilding a missing index.
    pub fn index_status(&self) -> Result<IndexStatus> {
        let Some(index) = self.read_index()? else {
            return Ok(IndexStatus {
                exists: false,
                event_count: 0,
                rebuilt_at: None,
            });
        };
        Ok(IndexStatus {
            exists: true,
            event_count: index.event_count,
            rebuilt_at: Some(index.rebuilt_at),
        })
    }

    /// Rebuilds the SQLite query cache from local and inbound events.
    pub fn rebuild_sqlite_index(&self) -> Result<SqliteIndexStatus> {
        self.init()?;
        let events = self.read_all_events()?;
        let index = TraceIndex::build(&events)?;
        rebuild_sqlite_index(&self.sqlite_index_path(), &events, &index)?;
        self.sqlite_index_status()
    }

    /// Returns SQLite query cache metadata without rebuilding a missing database.
    pub fn sqlite_index_status(&self) -> Result<SqliteIndexStatus> {
        sqlite_index_status(&self.sqlite_index_path())
    }
}

fn read_jsonl_events(root: &Path, label: &str) -> Result<Vec<TraceEvent>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut paths = jsonl_paths(root, label)?;
    paths.sort();

    let mut events = Vec::new();
    for path in paths {
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read {label} file {}", path.display()))?;
        for (line_index, line) in contents.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let event = serde_json::from_str::<TraceEvent>(line).with_context(|| {
                format!(
                    "failed to parse event in {} at line {}",
                    path.display(),
                    line_index + 1
                )
            })?;
            events.push(event);
        }
    }
    Ok(events)
}

fn jsonl_paths(root: &Path, label: &str) -> Result<Vec<PathBuf>> {
    if !root.exists() {
        return Ok(Vec::new());
    }

    let mut paths = Vec::new();
    for entry in fs::read_dir(root)
        .with_context(|| format!("failed to read {label} directory {}", root.display()))?
    {
        let entry = entry.context("failed to read event directory entry")?;
        let path = entry.path();
        if path.is_dir() {
            paths.extend(jsonl_paths(&path, label)?);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "jsonl")
        {
            paths.push(path);
        }
    }
    Ok(paths)
}

fn join_set(values: &BTreeSet<String>) -> String {
    values.iter().cloned().collect::<Vec<_>>().join(", ")
}

fn event_id_set(events: &[TraceEvent]) -> BTreeSet<Uuid> {
    events.iter().map(|event| event.event_id).collect()
}

#[cfg(test)]
mod tests {
    use brick_protocol::{
        ActorRef, ActorType, ArtifactCreatedPayload, ArtifactId, ArtifactKind, EventType,
        MissionCreatedPayload, MissionId, MissionStatus, ProjectId, SessionId, SessionSource,
        SessionStartedPayload,
    };

    use super::*;

    fn temp_repo_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-test-{name}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(path.join(".git")).expect("create fake git dir");
        path
    }

    #[test]
    fn init_adds_brick_to_gitignore_once() {
        let repo_root = temp_repo_root("gitignore-init");
        fs::write(repo_root.join(".gitignore"), "target\n").expect("write gitignore");
        let store = LocalStore::new(&repo_root);

        store.init().expect("init store");
        store.init().expect("init store again");

        let gitignore = fs::read_to_string(repo_root.join(".gitignore")).expect("read gitignore");
        assert!(gitignore.lines().any(|line| line == ".brick/"));
        assert_eq!(
            gitignore.lines().filter(|line| *line == ".brick/").count(),
            1
        );
    }

    #[test]
    fn init_creates_gitignore_when_missing() {
        let repo_root = temp_repo_root("gitignore-missing");
        let store = LocalStore::new(&repo_root);

        store.init().expect("init store");

        let gitignore = fs::read_to_string(repo_root.join(".gitignore")).expect("read gitignore");
        assert!(gitignore.lines().any(|line| line == ".brick/"));
    }

    fn mission_event(title: &str) -> TraceEvent {
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
        .expect("build mission event")
    }

    #[test]
    fn local_store_appends_and_reads_events() {
        let repo_root = temp_repo_root("append-read");
        let store = LocalStore::new(&repo_root);
        let mission_id = MissionId::new();
        let event = TraceEvent::mission_created(
            ActorRef {
                actor_type: ActorType::Human,
                actor_id: "tester".to_string(),
                display_name: None,
            },
            mission_id,
            MissionCreatedPayload {
                project_id: ProjectId::new(),
                title: "Test mission".to_string(),
                description: None,
                status: MissionStatus::Planned,
                repo_context_id: None,
            },
        )
        .expect("build event");

        store.append_event(&event).expect("append event");
        let events = store.read_queued_events().expect("read events");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].event_type, EventType::MissionCreated);
    }

    #[test]
    fn inbound_events_are_stored_separately_and_read_with_queue() {
        let repo_root = temp_repo_root("inbound-read");
        let store = LocalStore::new(&repo_root);
        let local_event = mission_event("Local mission");
        let inbound_event = mission_event("Inbound mission");

        store
            .append_event(&local_event)
            .expect("append local event");
        let inbound_path = store
            .append_inbound_events(&[inbound_event.clone()])
            .expect("append inbound event");

        assert!(inbound_path.starts_with(store.inbound_events_dir()));
        assert_eq!(store.read_queued_events().expect("read queue").len(), 1);
        assert_eq!(store.read_inbound_events().expect("read inbound").len(), 1);
        assert_eq!(store.read_all_events().expect("read all").len(), 2);
    }

    #[test]
    fn remote_dedupe_checks_local_and_inbound_events() {
        let repo_root = temp_repo_root("remote-dedupe");
        let store = LocalStore::new(&repo_root);
        let local_event = mission_event("Local mission");
        let inbound_event = mission_event("Inbound mission");
        let new_event = mission_event("New mission");

        store
            .append_event(&local_event)
            .expect("append local event");
        store
            .append_inbound_events(&[inbound_event.clone()])
            .expect("append inbound event");
        let deduped = store
            .dedupe_remote_events(vec![
                local_event,
                inbound_event,
                new_event.clone(),
                new_event,
            ])
            .expect("dedupe remote events");

        assert_eq!(deduped.len(), 1);
    }

    #[test]
    fn current_context_round_trips() {
        let repo_root = temp_repo_root("current-context");
        let store = LocalStore::new(&repo_root);
        let context = CurrentContext {
            actor: Some(ActorRef {
                actor_type: ActorType::Agent,
                actor_id: "agent-1".to_string(),
                display_name: None,
            }),
            session_id: Some(SessionId::new()),
            ..CurrentContext::default()
        };

        store
            .write_current_context(&context)
            .expect("write context");
        let read = store
            .read_current_context()
            .expect("read context")
            .expect("context exists");
        assert_eq!(context, read);
    }

    #[test]
    fn rebuild_index_writes_cache_file() {
        let repo_root = temp_repo_root("index-cache");
        let store = LocalStore::new(&repo_root);
        let mission_id = MissionId::new();
        let event = TraceEvent::mission_created(
            ActorRef {
                actor_type: ActorType::Human,
                actor_id: "tester".to_string(),
                display_name: None,
            },
            mission_id.clone(),
            MissionCreatedPayload {
                project_id: ProjectId::new(),
                title: "Index mission".to_string(),
                description: None,
                status: MissionStatus::Planned,
                repo_context_id: None,
            },
        )
        .expect("build event");

        store.append_event(&event).expect("append event");
        let index = store.rebuild_index().expect("rebuild index");
        assert!(store.index_path().exists());
        assert!(index.missions.contains_key(mission_id.as_str()));
        assert_eq!(store.index_status().expect("index status").event_count, 1);
    }

    #[test]
    fn rebuild_sqlite_index_writes_query_cache() {
        let repo_root = temp_repo_root("sqlite-cache");
        let store = LocalStore::new(&repo_root);
        let actor = ActorRef {
            actor_type: ActorType::Agent,
            actor_id: "agent-1".to_string(),
            display_name: None,
        };
        let mission_id = MissionId::new();
        let session_id = SessionId::new();
        let artifact_id = ArtifactId::new();
        let mission = TraceEvent::mission_created(
            actor.clone(),
            mission_id.clone(),
            MissionCreatedPayload {
                project_id: ProjectId::new(),
                title: "SQLite mission".to_string(),
                description: None,
                status: MissionStatus::Planned,
                repo_context_id: None,
            },
        )
        .expect("build mission event");
        let session = TraceEvent::session_started(
            actor.clone(),
            session_id.clone(),
            Some(mission_id.clone()),
            SessionStartedPayload {
                session_name: Some("SQLite session".to_string()),
                source: SessionSource {
                    app_id: Some("cursor".to_string()),
                    app_session_id: Some("native-1".to_string()),
                    app_session_name: None,
                    runtime_id: Some("runtime-1".to_string()),
                },
                repo_context_id: None,
            },
        )
        .expect("build session event");
        let artifact = TraceEvent::artifact_created(
            actor,
            artifact_id,
            Some(mission_id),
            Some(session_id.clone()),
            ArtifactCreatedPayload {
                artifact_kind: ArtifactKind::Decision,
                title: "SQLite artifact".to_string(),
                body: None,
                repo_context_id: None,
            },
        )
        .expect("build artifact event");

        store.append_event(&mission).expect("append mission");
        store.append_event(&session).expect("append session");
        store.append_event(&artifact).expect("append artifact");
        let status = store.rebuild_sqlite_index().expect("rebuild sqlite index");
        assert!(store.sqlite_index_path().exists());
        assert_eq!(status.session_count, 1);
        let sessions = crate::query_sqlite_sessions(
            &store.sqlite_index_path(),
            &crate::SqliteSessionQuery {
                app_id: Some("cursor".to_string()),
                actor_id: Some("agent-1".to_string()),
                runtime_id: Some("runtime-1".to_string()),
                limit: 20,
            },
        )
        .expect("query sqlite sessions");
        assert_eq!(sessions[0].session_id, session_id.to_string());
    }
}
