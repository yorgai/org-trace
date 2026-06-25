//! Enumerations for trace event categories and provenance confidence.
//!
//! Event variants serialize to stable snake/dotted wire names. These values are
//! part of the local JSONL protocol and should only change through an explicit
//! schema migration.

use serde::{Deserialize, Serialize};

/// Stable event names used in the append-only provenance log.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EventType {
    #[serde(rename = "org.created")]
    OrgCreated,
    #[serde(rename = "org.updated")]
    OrgUpdated,
    #[serde(rename = "project.created")]
    ProjectCreated,
    #[serde(rename = "project.updated")]
    ProjectUpdated,
    #[serde(rename = "mission.created")]
    MissionCreated,
    #[serde(rename = "mission.updated")]
    MissionUpdated,
    #[serde(rename = "session.started")]
    SessionStarted,
    #[serde(rename = "session.linked_to_mission")]
    SessionLinkedToMission,
    #[serde(rename = "session.log_uploaded")]
    SessionLogUploaded,
    #[serde(rename = "artifact.created")]
    ArtifactCreated,
    #[serde(rename = "artifact.updated")]
    ArtifactUpdated,
    #[serde(rename = "artifact.linked_to_mission")]
    ArtifactLinkedToMission,
    #[serde(rename = "artifact.file_ref_recorded")]
    ArtifactFileRefRecorded,
    #[serde(rename = "artifact.attachment_uploaded")]
    ArtifactAttachmentUploaded,
    #[serde(rename = "artifact.reviewed")]
    ArtifactReviewed,
    #[serde(rename = "artifact.accepted")]
    ArtifactAccepted,
    #[serde(rename = "repo_context.captured")]
    RepoContextCaptured,
    #[serde(rename = "diff.captured")]
    DiffCaptured,
    #[serde(rename = "external_ref.linked")]
    ExternalRefLinked,
    #[serde(rename = "source.session_observed")]
    SourceSessionObserved,
}

/// Confidence level for how directly Brick observed the event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    Explicit,
    Observed,
    Imported,
    Inferred,
    Unknown,
}

/// How the working tree was attached when repo context was captured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMode {
    AttachedCurrentBranch,
    CreatedWorktree,
    DetachedHead,
    Unknown,
}

/// Lifecycle state for a Mission accountability container.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    #[default]
    Planned,
    Active,
    Blocked,
    Completed,
    Archived,
}

/// Reviewable output category produced by a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactKind {
    Decision,
    FileRef,
    Patch,
    Review,
    TestResult,
    Acceptance,
    Note,
}

/// Git comparison source used when capturing patch provenance metadata.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffTarget {
    Working,
    Staged,
    Range,
}

/// Coarse file-level change kind for a captured diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiffFileChangeKind {
    Added,
    Modified,
    Deleted,
    Renamed,
    Copied,
    TypeChanged,
    Unknown,
}
