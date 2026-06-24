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
    /// A directed causal edge: one effect event was caused by zero or more
    /// upstream events and/or carries a standalone rationale. This is what turns
    /// the time-ordered event stream into a causal graph — the core of Brick's
    /// `explain` (WHY), as opposed to a mere timeline (= `git log`).
    #[serde(rename = "causal.linked")]
    CausalLinked,
}

/// The kind of causal edge from an effect event to its cause(s).
///
/// Every variant is orthogonal and must be matched exhaustively (no catch-all)
/// so a future relation forces every consumer to decide how to handle it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CausalRelation {
    /// The effect was set in motion by the cause (e.g. an A2A `previous_actions`
    /// step, or "I changed auth.rs because config.rs added a field").
    TriggeredBy,
    /// The effect was derived/built from the cause (e.g. a test written to cover
    /// an earlier fix).
    DerivedFrom,
    /// The effect corrects or replaces the cause.
    Supersedes,
    /// The effect is a response to a request captured by the cause.
    RespondsTo,
    /// A standalone reason with no upstream event — the WHY that can never be
    /// reverse-engineered from the code itself (e.g. "token refresh has a race").
    /// Used when `cause_events` is empty and only `note` carries meaning.
    Rationale,
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MissionStatus {
    Planned,
    Active,
    Blocked,
    Completed,
    Archived,
}

impl Default for MissionStatus {
    fn default() -> Self {
        Self::Planned
    }
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
