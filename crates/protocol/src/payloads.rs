//! Typed payloads for each provenance event family.
//!
//! `TraceEvent` stores payloads as JSON for append-only flexibility, but event
//! constructors and index rebuilding use these structs so producers and
//! consumers share the same schema.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ArtifactKind, AttachmentId, CausalRelation, ContextMode, DiffFileChangeKind, DiffTarget,
    ExternalRefId, FileRefId, LogRefId, MissionStatus, OrgId, ProjectId, RepoContextId,
};

/// Source-application identity for a canonical Brick session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct SessionSource {
    pub app_id: Option<String>,
    pub app_session_id: Option<String>,
    pub app_session_name: Option<String>,
    pub runtime_id: Option<String>,
}

/// Payload recorded when a Brick Org sync boundary is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgCreatedPayload {
    pub name: String,
    pub description: Option<String>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Partial update payload for Org metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OrgUpdatedPayload {
    pub name: Option<String>,
    pub description: Option<String>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload recorded when a Brick Project is created inside an Org.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectCreatedPayload {
    pub org_id: OrgId,
    pub name: String,
    pub description: Option<String>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Partial update payload for Project metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectUpdatedPayload {
    pub org_id: Option<OrgId>,
    pub name: Option<String>,
    pub description: Option<String>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload recorded when a new Mission accountability container is created.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionCreatedPayload {
    pub project_id: ProjectId,
    pub title: String,
    pub description: Option<String>,
    pub status: MissionStatus,
    pub repo_context_id: Option<RepoContextId>,
}

/// Partial update payload for Mission metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MissionUpdatedPayload {
    pub project_id: Option<ProjectId>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub status: Option<MissionStatus>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload recorded when a canonical session begins or is first observed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionStartedPayload {
    pub session_name: Option<String>,
    pub source: SessionSource,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload for an explicit many-to-many Session ↔ Mission link.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLinkedToMissionPayload {
    pub relationship: String,
    pub repo_context_id: Option<RepoContextId>,
}

/// Declared format for uploaded session log content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionLogFormat {
    Text,
    Jsonl,
    Markdown,
    Unknown,
}

/// Availability of full evidence bytes for a local pointer or copied blob.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceAvailability {
    LocalPointer,
    LocalBlob,
    RemoteBlob,
}

/// Payload for content-addressed session log bytes captured outside artifact attachments.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionLogUploadedPayload {
    pub log_ref_id: LogRefId,
    pub original_path: String,
    pub format: SessionLogFormat,
    pub source: String,
    pub sha256: String,
    pub size_bytes: u64,
    pub storage_uri: String,
    pub local_path: String,
    pub external_uri: Option<String>,
    pub availability: EvidenceAvailability,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload for a reviewable output produced by a session.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactCreatedPayload {
    pub artifact_kind: ArtifactKind,
    pub title: String,
    pub body: Option<String>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Partial metadata update payload for an existing Artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactUpdatedPayload {
    pub title: Option<String>,
    pub body: Option<String>,
    pub artifact_kind: Option<ArtifactKind>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload for linking an existing Artifact to a Mission after capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactLinkedToMissionPayload {
    pub relationship: String,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload for file-level artifact involvement without line-level attribution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactFileRefRecordedPayload {
    pub file_ref_id: FileRefId,
    pub path: String,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload for content-addressed file content attached to an Artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactAttachmentUploadedPayload {
    pub attachment_id: AttachmentId,
    pub name: String,
    pub original_path: String,
    pub content_type: Option<String>,
    pub sha256: String,
    pub size_bytes: u64,
    pub storage_uri: String,
    pub external_uri: Option<String>,
    pub availability: EvidenceAvailability,
    pub repo_context_id: Option<RepoContextId>,
}

/// Git repository snapshot captured alongside write operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoContextCapturedPayload {
    pub repo_root: String,
    pub work_dir: String,
    pub remote_url: Option<String>,
    pub branch: Option<String>,
    pub upstream_branch: Option<String>,
    pub head_commit: Option<String>,
    pub merge_base_commit: Option<String>,
    pub dirty: bool,
    pub context_mode: ContextMode,
}

/// A single hunk's line ranges within a captured diff, mirroring the unified
/// `@@ -old_start,old_lines +new_start,new_lines @@` header. Only line numbers
/// and the optional section header are stored — never the changed content — so
/// line-level blame stays metadata, not a code copy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffHunk {
    pub old_start: u64,
    pub old_lines: u64,
    pub new_start: u64,
    pub new_lines: u64,
    pub header: Option<String>,
}

/// Per-path summary for a captured diff, with optional hunk-level line ranges
/// that drive line-level blame. `hunks` is additive: events recorded before
/// line-level capture deserialize with an empty vec and behave as file-level.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFileChange {
    pub path: String,
    pub old_path: Option<String>,
    pub change_kind: DiffFileChangeKind,
    pub additions: Option<u64>,
    pub deletions: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hunks: Vec<DiffHunk>,
    /// Stable `git patch-id` of *this file's* slice of the diff. Recorded so
    /// blame can bridge a working-tree capture to the commit that later lands
    /// it: a commit usually touches several files, so the whole-commit patch-id
    /// differs from a single-file capture, but `git show <commit> -- <path>`
    /// reproduces exactly this per-file id. Additive: older events deserialize
    /// to `None` and fall back to the file-level / hunk path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch_id: Option<String>,
}

/// Payload for patch provenance captured as diff statistics and file summaries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffCapturedPayload {
    pub diff_target: DiffTarget,
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
    pub patch_id: Option<String>,
    pub summary_hash: String,
    pub file_changes: Vec<DiffFileChange>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Link from trace graph entities to external systems such as PRs or issues.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalRefLinkedPayload {
    pub external_ref_id: ExternalRefId,
    pub provider: String,
    pub ref_type: String,
    pub target: String,
    pub repo_context_id: Option<RepoContextId>,
}

/// Payload for a directed causal edge in the provenance graph.
///
/// The effect this edge is about is anchored at one of three precision levels,
/// highest to lowest: a specific `effect_event` (usually a `diff.captured`), a
/// repo-relative `effect_path` (a file the agent edited with its own tools, with
/// no Brick event yet), or — when neither is known — the `repo_context_id` alone
/// (a repo-level standalone rationale). `cause_events` are the zero-or-more
/// upstream events that caused it (any `event_id` — a diff, an artifact, another
/// session's event, a mission). `cause_events` may be empty for a standalone
/// `Rationale`.
///
/// Invariant (enforced by `TraceEvent::causal_linked`): the edge must carry
/// information — at least one of `effect_event`, `effect_path`, a non-empty
/// `cause_events`, or a non-empty `note`. An edge with none of these is pure
/// noise and is rejected.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CausalLinkedPayload {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_event: Option<Uuid>,
    /// Repo-relative path of a file-level effect when no `effect_event` exists
    /// (the agent edited the file with its own tools). The next-lower anchor
    /// precision after `effect_event`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub cause_events: Vec<Uuid>,
    pub relation: CausalRelation,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub repo_context_id: Option<RepoContextId>,
}
