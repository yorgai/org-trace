//! Serializable data model for the rebuildable local trace index.
//!
//! These structs optimize local inspection commands by materializing graph edges
//! between Missions, Sessions, Artifacts, files, and repo contexts.

use std::collections::{BTreeMap, BTreeSet};

use brick_protocol::{
    ActorRef, ActorType, ArtifactKind, CausalRelation, DiffFileChangeKind, DiffHunk, DiffTarget,
    EvidenceAvailability, MissionStatus, SessionLogFormat, SessionSource,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Current schema version for the derived local cache index.
pub const INDEX_SCHEMA_VERSION: u16 = 1;

/// Filename for the derived index cache under `.brick/provenance/cache`.
pub const INDEX_CACHE_FILE: &str = "index.json";

/// Rebuildable local graph index projected from append-only trace events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TraceIndex {
    pub schema_version: u16,
    pub rebuilt_at: DateTime<Utc>,
    pub event_count: usize,
    pub orgs: BTreeMap<String, IndexedOrg>,
    pub projects: BTreeMap<String, IndexedProject>,
    pub missions: BTreeMap<String, IndexedMission>,
    pub sessions: BTreeMap<String, IndexedSession>,
    pub artifacts: BTreeMap<String, IndexedArtifact>,
    pub attachments: BTreeMap<String, IndexedAttachment>,
    pub diffs: BTreeMap<String, IndexedDiff>,
    pub session_logs: BTreeMap<String, IndexedSessionLog>,
    pub files: BTreeMap<String, IndexedFile>,
    pub repo_contexts: BTreeMap<String, IndexedRepoContext>,
    /// Causal adjacency: effect event-id → its direct cause edges (for backward
    /// `explain` traversal). The edges are built at index time; the *chains* are
    /// traversed at query time (chains are relative to an anchor + depth, so
    /// materializing them here would combinatorially explode).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub causes: BTreeMap<String, Vec<CausalEdge>>,
    /// Causal adjacency: cause event-id → effect event-ids it influenced (for
    /// forward traversal, e.g. "this fix has a test derived from it").
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effects: BTreeMap<String, Vec<String>>,
}

/// One causal edge from an effect event back to a single cause, carrying the
/// relation, optional rationale note, and the confidence of the attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalEdge {
    /// The upstream cause event-id, or `None` for a standalone `Rationale`.
    pub cause_event: Option<String>,
    pub relation: CausalRelation,
    pub note: Option<String>,
    /// Source event-id of the `causal.linked` event that recorded this edge.
    pub source_event_id: String,
    /// `explicit` (asserted), `observed` (hook-captured), or `inferred` (heuristic).
    pub confidence: String,
    pub recorded_at: DateTime<Utc>,
}

/// Indexed Org view with child Projects and repo contexts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedOrg {
    pub org_id: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub project_ids: BTreeSet<String>,
    pub repo_context_ids: BTreeSet<String>,
    pub created_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
}

impl IndexedOrg {
    pub(crate) fn blank(org_id: String, recorded_at: DateTime<Utc>) -> Self {
        Self {
            org_id,
            name: None,
            description: None,
            project_ids: BTreeSet::new(),
            repo_context_ids: BTreeSet::new(),
            created_at: recorded_at,
            last_event_at: recorded_at,
        }
    }
}

/// Indexed Project view with parent Org, child Missions, and repo contexts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedProject {
    pub project_id: String,
    pub org_id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub mission_ids: BTreeSet<String>,
    pub repo_context_ids: BTreeSet<String>,
    pub created_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
}

impl IndexedProject {
    pub(crate) fn blank(project_id: String, recorded_at: DateTime<Utc>) -> Self {
        Self {
            project_id,
            org_id: None,
            name: None,
            description: None,
            mission_ids: BTreeSet::new(),
            repo_context_ids: BTreeSet::new(),
            created_at: recorded_at,
            last_event_at: recorded_at,
        }
    }
}

/// Indexed Mission view with linked sessions, artifacts, and repo contexts.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedMission {
    pub mission_id: String,
    pub project_id: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: MissionStatus,
    pub session_ids: BTreeSet<String>,
    pub artifact_ids: BTreeSet<String>,
    pub repo_context_ids: BTreeSet<String>,
    pub created_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
}

impl IndexedMission {
    pub(crate) fn blank(mission_id: String, recorded_at: DateTime<Utc>) -> Self {
        Self {
            mission_id,
            project_id: None,
            title: None,
            description: None,
            status: MissionStatus::default(),
            session_ids: BTreeSet::new(),
            artifact_ids: BTreeSet::new(),
            repo_context_ids: BTreeSet::new(),
            created_at: recorded_at,
            last_event_at: recorded_at,
        }
    }
}

/// Indexed Session view including source-app identity and graph edges.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSession {
    pub session_id: String,
    pub session_name: Option<String>,
    pub actor_id: Option<String>,
    pub actor_type: Option<ActorType>,
    pub source: SessionSource,
    pub mission_ids: BTreeSet<String>,
    pub artifact_ids: BTreeSet<String>,
    pub log_ref_ids: BTreeSet<String>,
    pub repo_context_ids: BTreeSet<String>,
    pub started_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
}

impl IndexedSession {
    /// Creates an empty indexed session shell for index projections and tests.
    pub fn blank(session_id: String, recorded_at: DateTime<Utc>, actor: &ActorRef) -> Self {
        Self {
            session_id,
            session_name: None,
            actor_id: Some(actor.actor_id.clone()),
            actor_type: Some(actor.actor_type),
            source: SessionSource::default(),
            mission_ids: BTreeSet::new(),
            artifact_ids: BTreeSet::new(),
            log_ref_ids: BTreeSet::new(),
            repo_context_ids: BTreeSet::new(),
            started_at: recorded_at,
            last_event_at: recorded_at,
        }
    }
}

/// Indexed Artifact view with linked Missions, Sessions, and file paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedArtifact {
    pub artifact_id: String,
    pub artifact_kind: Option<ArtifactKind>,
    pub title: Option<String>,
    pub body: Option<String>,
    pub mission_ids: BTreeSet<String>,
    pub session_ids: BTreeSet<String>,
    pub file_paths: BTreeSet<String>,
    pub attachment_ids: BTreeSet<String>,
    pub diff_ids: BTreeSet<String>,
    pub repo_context_ids: BTreeSet<String>,
    pub created_at: DateTime<Utc>,
    pub last_event_at: DateTime<Utc>,
}

impl IndexedArtifact {
    pub(crate) fn blank(artifact_id: String, recorded_at: DateTime<Utc>) -> Self {
        Self {
            artifact_id,
            artifact_kind: None,
            title: None,
            body: None,
            mission_ids: BTreeSet::new(),
            session_ids: BTreeSet::new(),
            file_paths: BTreeSet::new(),
            attachment_ids: BTreeSet::new(),
            diff_ids: BTreeSet::new(),
            repo_context_ids: BTreeSet::new(),
            created_at: recorded_at,
            last_event_at: recorded_at,
        }
    }
}

/// Indexed attachment view for content-addressed artifact bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedAttachment {
    pub attachment_id: String,
    pub artifact_id: String,
    pub session_id: Option<String>,
    pub name: String,
    pub original_path: String,
    pub content_type: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
    pub storage_uri: String,
    pub external_uri: Option<String>,
    pub availability: EvidenceAvailability,
    pub repo_context_id: Option<String>,
    pub uploaded_at: DateTime<Utc>,
}

/// Indexed diff capture view for patch artifact metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedDiff {
    pub diff_id: String,
    pub artifact_id: String,
    pub session_id: Option<String>,
    pub mission_id: Option<String>,
    pub diff_target: DiffTarget,
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
    pub patch_id: Option<String>,
    pub summary_hash: String,
    pub file_count: usize,
    pub additions: u64,
    pub deletions: u64,
    pub binary_file_count: usize,
    pub repo_context_id: Option<String>,
    pub captured_at: DateTime<Utc>,
    pub file_changes: Vec<IndexedDiffFileChange>,
}

/// File-level statistics associated with an indexed diff capture.
///
/// NOTE: `patch_id` is intentionally OMITTED here even though the source-of-truth
/// payload (`brick_protocol::DiffFileChange`) carries it. Line-level owner blame
/// is events-authoritative: it computes attribution by reading `patch_id`
/// straight from the JSONL event stream (`read_all_events`), never from this
/// derived index or the SQLite cache. Mirroring `patch_id` into the cache would
/// invite code to "blame from the cache", which would silently break attribution
/// whenever the cache lagged the queue. Keep blame reading events only — see
/// `crate::blame::blame_file` (it reads `change.patch_id` off the event payload).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedDiffFileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub change_kind: DiffFileChangeKind,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<DiffHunk>,
}

/// Indexed session log view for content-addressed log bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedSessionLog {
    pub log_ref_id: String,
    pub session_id: String,
    pub original_path: String,
    pub format: SessionLogFormat,
    pub source: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub storage_uri: String,
    pub local_path: String,
    pub external_uri: Option<String>,
    pub availability: EvidenceAvailability,
    pub repo_context_id: Option<String>,
    pub uploaded_at: DateTime<Utc>,
}

/// Indexed file view listing artifact references for a path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedFile {
    pub path: String,
    pub file_refs: Vec<IndexedFileRef>,
}

impl IndexedFile {
    pub(crate) fn new(path: String) -> Self {
        Self {
            path,
            file_refs: Vec::new(),
        }
    }
}

/// File-level provenance edge connecting a path to an artifact and session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedFileRef {
    pub file_ref_id: String,
    pub artifact_id: String,
    pub session_id: Option<String>,
    pub repo_context_id: Option<String>,
    pub recorded_at: DateTime<Utc>,
}

/// Compact repo context summary used by local inspection commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexedRepoContext {
    pub repo_context_id: String,
    pub branch: Option<String>,
    pub head_commit: Option<String>,
    pub dirty: bool,
    pub captured_at: DateTime<Utc>,
}

/// Status metadata for the local derived index cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexStatus {
    pub exists: bool,
    pub event_count: usize,
    pub rebuilt_at: Option<DateTime<Utc>>,
}
