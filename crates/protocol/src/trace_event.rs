//! Event envelope and constructors for the Brick JSONL protocol.
//!
//! Constructors attach typed entity IDs to the generic envelope and serialize
//! typed payload structs into the JSON payload slot. This keeps local writes
//! simple while preserving strongly typed call sites.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::{
    ActorRef, ArtifactAttachmentUploadedPayload, ArtifactCreatedPayload,
    ArtifactFileRefRecordedPayload, ArtifactId, ArtifactLinkedToMissionPayload,
    ArtifactUpdatedPayload, ConfidenceLevel, DiffCapturedPayload, EventType,
    ExternalRefLinkedPayload, MissionCreatedPayload, MissionId, MissionUpdatedPayload,
    OrgCreatedPayload, OrgId, OrgUpdatedPayload, ProjectCreatedPayload, ProjectId,
    ProjectUpdatedPayload, RepoContextCapturedPayload, RepoContextId, SessionId,
    SessionLinkedToMissionPayload, SessionLogUploadedPayload, SessionStartedPayload,
    SourceSessionObservedPayload,
};

/// Current protocol schema version for event envelopes and typed payloads.
pub const EVENT_SCHEMA_VERSION: u16 = 1;

/// Append-only provenance event persisted in local JSONL and future remotes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub event_id: Uuid,
    pub event_type: EventType,
    pub schema_version: u16,
    pub payload_schema_version: u16,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub actor: ActorRef,
    pub repo_id: Option<String>,
    pub org_id: Option<OrgId>,
    pub project_id: Option<ProjectId>,
    pub mission_id: Option<MissionId>,
    pub session_id: Option<SessionId>,
    pub artifact_id: Option<ArtifactId>,
    pub repo_context_id: Option<RepoContextId>,
    pub confidence: ConfidenceLevel,
    pub payload: Value,
}

impl TraceEvent {
    /// Builds an `org.created` event for a new Brick Org sync boundary.
    pub fn org_created(
        actor: ActorRef,
        org_id: OrgId,
        payload: OrgCreatedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::OrgCreated, actor, payload)?;
        event.org_id = Some(org_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds an `org.updated` event with partial Org metadata.
    pub fn org_updated(
        actor: ActorRef,
        org_id: OrgId,
        payload: OrgUpdatedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::OrgUpdated, actor, payload)?;
        event.org_id = Some(org_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds a `project.created` event for a new Brick Project.
    pub fn project_created(
        actor: ActorRef,
        project_id: ProjectId,
        payload: ProjectCreatedPayload,
    ) -> serde_json::Result<Self> {
        let org_id = payload.org_id.clone();
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::ProjectCreated, actor, payload)?;
        event.org_id = Some(org_id);
        event.project_id = Some(project_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds a `project.updated` event with partial Project metadata.
    pub fn project_updated(
        actor: ActorRef,
        project_id: ProjectId,
        payload: ProjectUpdatedPayload,
    ) -> serde_json::Result<Self> {
        let org_id = payload.org_id.clone();
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::ProjectUpdated, actor, payload)?;
        event.org_id = org_id;
        event.project_id = Some(project_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds a `mission.created` event for a new Mission.
    pub fn mission_created(
        actor: ActorRef,
        mission_id: MissionId,
        payload: MissionCreatedPayload,
    ) -> serde_json::Result<Self> {
        let project_id = payload.project_id.clone();
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::MissionCreated, actor, payload)?;
        event.project_id = Some(project_id);
        event.mission_id = Some(mission_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds a `mission.updated` event with partial Mission metadata.
    pub fn mission_updated(
        actor: ActorRef,
        mission_id: MissionId,
        payload: MissionUpdatedPayload,
    ) -> serde_json::Result<Self> {
        let project_id = payload.project_id.clone();
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::MissionUpdated, actor, payload)?;
        event.project_id = project_id;
        event.mission_id = Some(mission_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds a `session.started` event for canonical and app-native session IDs.
    pub fn session_started(
        actor: ActorRef,
        session_id: SessionId,
        mission_id: Option<MissionId>,
        payload: SessionStartedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::SessionStarted, actor, payload)?;
        event.mission_id = mission_id;
        event.session_id = Some(session_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds an explicit Session → Mission linkage event.
    pub fn session_linked_to_mission(
        actor: ActorRef,
        session_id: SessionId,
        mission_id: MissionId,
        payload: SessionLinkedToMissionPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::SessionLinkedToMission, actor, payload)?;
        event.mission_id = Some(mission_id);
        event.session_id = Some(session_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds a `session.log_uploaded` event for content-addressed session log bytes.
    pub fn session_log_uploaded(
        actor: ActorRef,
        session_id: SessionId,
        payload: SessionLogUploadedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::SessionLogUploaded, actor, payload)?;
        event.session_id = Some(session_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds an `artifact.created` event for a reviewable session output.
    pub fn artifact_created(
        actor: ActorRef,
        artifact_id: ArtifactId,
        mission_id: Option<MissionId>,
        session_id: Option<SessionId>,
        payload: ArtifactCreatedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::ArtifactCreated, actor, payload)?;
        event.mission_id = mission_id;
        event.session_id = session_id;
        event.artifact_id = Some(artifact_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds an `artifact.updated` event with partial Artifact metadata.
    pub fn artifact_updated(
        actor: ActorRef,
        artifact_id: ArtifactId,
        session_id: Option<SessionId>,
        payload: ArtifactUpdatedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::ArtifactUpdated, actor, payload)?;
        event.session_id = session_id;
        event.artifact_id = Some(artifact_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds an event linking an existing Artifact to a Mission.
    pub fn artifact_linked_to_mission(
        actor: ActorRef,
        artifact_id: ArtifactId,
        mission_id: MissionId,
        payload: ArtifactLinkedToMissionPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::ArtifactLinkedToMission, actor, payload)?;
        event.mission_id = Some(mission_id);
        event.artifact_id = Some(artifact_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds an event recording a file touched or represented by an Artifact.
    pub fn artifact_file_ref_recorded(
        actor: ActorRef,
        artifact_id: ArtifactId,
        session_id: Option<SessionId>,
        payload: ArtifactFileRefRecordedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::ArtifactFileRefRecorded, actor, payload)?;
        event.session_id = session_id;
        event.artifact_id = Some(artifact_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds an event recording content-addressed file content attached to an Artifact.
    pub fn artifact_attachment_uploaded(
        actor: ActorRef,
        artifact_id: ArtifactId,
        session_id: Option<SessionId>,
        payload: ArtifactAttachmentUploadedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::ArtifactAttachmentUploaded, actor, payload)?;
        event.session_id = session_id;
        event.artifact_id = Some(artifact_id);
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds a repo context snapshot event for Git state at capture time.
    pub fn repo_context_captured(
        actor: ActorRef,
        repo_context_id: RepoContextId,
        payload: RepoContextCapturedPayload,
    ) -> serde_json::Result<Self> {
        let mut event = Self::from_payload(EventType::RepoContextCaptured, actor, payload)?;
        event.repo_context_id = Some(repo_context_id);
        Ok(event)
    }

    /// Builds a `diff.captured` event linked to the artifact it describes.
    pub fn diff_captured(
        actor: ActorRef,
        artifact_id: ArtifactId,
        session_id: Option<SessionId>,
        mission_id: Option<MissionId>,
        payload: DiffCapturedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::DiffCaptured, actor, payload)?;
        event.artifact_id = Some(artifact_id);
        event.session_id = session_id;
        event.mission_id = mission_id;
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds an event linking trace entities to an external system reference.
    pub fn external_ref_linked(
        actor: ActorRef,
        mission_id: Option<MissionId>,
        session_id: Option<SessionId>,
        artifact_id: Option<ArtifactId>,
        payload: ExternalRefLinkedPayload,
    ) -> serde_json::Result<Self> {
        let repo_context_id = payload.repo_context_id.clone();
        let mut event = Self::from_payload(EventType::ExternalRefLinked, actor, payload)?;
        event.mission_id = mission_id;
        event.session_id = session_id;
        event.artifact_id = artifact_id;
        event.repo_context_id = repo_context_id;
        Ok(event)
    }

    /// Builds a normalized source-session observation event for sync.
    pub fn source_session_observed(
        actor: ActorRef,
        payload: SourceSessionObservedPayload,
    ) -> serde_json::Result<Self> {
        let mut event = Self::from_payload(EventType::SourceSessionObserved, actor, payload)?;
        event.confidence = ConfidenceLevel::Observed;
        Ok(event)
    }

    fn from_payload<T>(
        event_type: EventType,
        actor: ActorRef,
        payload: T,
    ) -> serde_json::Result<Self>
    where
        T: Serialize,
    {
        let now = Utc::now();
        Ok(Self {
            event_id: Uuid::new_v4(),
            event_type,
            schema_version: EVENT_SCHEMA_VERSION,
            payload_schema_version: EVENT_SCHEMA_VERSION,
            occurred_at: now,
            recorded_at: now,
            actor,
            repo_id: None,
            org_id: None,
            project_id: None,
            mission_id: None,
            session_id: None,
            artifact_id: None,
            repo_context_id: None,
            confidence: ConfidenceLevel::Explicit,
            payload: serde_json::to_value(payload)?,
        })
    }
}

/// Error returned when a `causal.linked` event cannot be constructed.
#[derive(Debug)]
pub enum CausalLinkError {
    /// Both `cause_events` and `note` were empty — an information-free edge.
    Empty,
    /// The payload failed to serialize.
    Serialize(serde_json::Error),
}

impl std::fmt::Display for CausalLinkError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CausalLinkError::Empty => formatter
                .write_str("causal edge requires at least one cause_event or a non-empty note"),
            CausalLinkError::Serialize(err) => {
                write!(formatter, "failed to serialize payload: {err}")
            }
        }
    }
}

impl std::error::Error for CausalLinkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            CausalLinkError::Empty => None,
            CausalLinkError::Serialize(err) => Some(err),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        ActorRef, ActorType, ArtifactAttachmentUploadedPayload, ArtifactId, ArtifactKind,
        ArtifactUpdatedPayload, AttachmentId, DiffCapturedPayload, DiffFileChange,
        DiffFileChangeKind, DiffHunk, DiffTarget, EventType, EvidenceAvailability, LogRefId,
        SessionId, SessionLogFormat, SessionLogUploadedPayload, SessionSource,
        SessionStartedPayload,
    };

    use super::*;

    fn actor() -> ActorRef {
        ActorRef {
            actor_type: ActorType::Agent,
            actor_id: "agent-1".to_string(),
            display_name: None,
        }
    }

    #[test]
    fn artifact_updated_serializes_wire_event_and_partial_payload() {
        let artifact_id = ArtifactId::new();
        let event = TraceEvent::artifact_updated(
            actor(),
            artifact_id.clone(),
            None,
            ArtifactUpdatedPayload {
                title: Some("Renamed artifact".to_string()),
                body: Some("Updated body".to_string()),
                artifact_kind: Some(ArtifactKind::Review),
                repo_context_id: None,
            },
        )
        .expect("build artifact update event");

        let serialized = serde_json::to_string(&event).expect("serialize artifact update");
        assert!(serialized.contains("\"event_type\":\"artifact.updated\""));
        assert_eq!(event.artifact_id, Some(artifact_id));
        assert_eq!(event.event_type, EventType::ArtifactUpdated);
        assert_eq!(event.payload["title"], "Renamed artifact");
        assert_eq!(event.payload["artifact_kind"], "review");
    }

    #[test]
    fn artifact_attachment_upload_serializes_without_inline_content() {
        let artifact_id = ArtifactId::new();
        let attachment_id = AttachmentId::new();
        let event = TraceEvent::artifact_attachment_uploaded(
            actor(),
            artifact_id.clone(),
            None,
            ArtifactAttachmentUploadedPayload {
                attachment_id: attachment_id.clone(),
                name: "report.txt".to_string(),
                original_path: "/tmp/report.txt".to_string(),
                content_type: Some("text/plain".to_string()),
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                size_bytes: 5,
                storage_uri: "brick-blob://sha256/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                external_uri: Some("file:///tmp/report.txt".to_string()),
                availability: EvidenceAvailability::LocalBlob,
                repo_context_id: None,
            },
        )
        .expect("build attachment event");

        let serialized = serde_json::to_string(&event).expect("serialize attachment upload");
        assert!(serialized.contains("\"event_type\":\"artifact.attachment_uploaded\""));
        assert_eq!(event.artifact_id, Some(artifact_id));
        assert_eq!(event.event_type, EventType::ArtifactAttachmentUploaded);
        assert_eq!(event.payload["attachment_id"], attachment_id.as_str());
        assert!(event.payload.get("content").is_none());
    }

    #[test]
    fn diff_captured_serializes_patch_metadata() {
        let artifact_id = ArtifactId::new();
        let session_id = SessionId::new();
        let event = TraceEvent::diff_captured(
            actor(),
            artifact_id.clone(),
            Some(session_id.clone()),
            None,
            DiffCapturedPayload {
                diff_target: DiffTarget::Working,
                base_commit: Some("base".to_string()),
                head_commit: Some("head".to_string()),
                patch_id: None,
                summary_hash: "abc123".to_string(),
                file_changes: vec![DiffFileChange {
                    path: "src/lib.rs".to_string(),
                    old_path: None,
                    change_kind: DiffFileChangeKind::Modified,
                    additions: Some(3),
                    deletions: Some(1),
                    hunks: vec![DiffHunk {
                        old_start: 1,
                        old_lines: 1,
                        new_start: 1,
                        new_lines: 3,
                        header: None,
                    }],
                    patch_id: None,
                }],
                repo_context_id: None,
            },
        )
        .expect("build diff event");

        let serialized = serde_json::to_string(&event).expect("serialize diff capture");
        assert!(serialized.contains("\"event_type\":\"diff.captured\""));
        assert_eq!(event.event_type, EventType::DiffCaptured);
        assert_eq!(event.artifact_id, Some(artifact_id));
        assert_eq!(event.session_id, Some(session_id));
        assert_eq!(event.payload["diff_target"], "working");
        assert_eq!(event.payload["file_changes"][0]["change_kind"], "modified");
    }

    #[test]
    fn session_started_uses_canonical_and_app_session_ids() {
        let session_id = SessionId::new();
        let event = TraceEvent::session_started(
            actor(),
            session_id.clone(),
            None,
            SessionStartedPayload {
                session_name: Some("local session".to_string()),
                source: SessionSource {
                    app_id: Some("cursor".to_string()),
                    app_session_id: Some("native-123".to_string()),
                    app_session_name: Some("Captured Chat".to_string()),
                    runtime_id: Some("runtime-1".to_string()),
                },
                repo_context_id: None,
            },
        )
        .expect("build session event");

        assert_eq!(event.session_id, Some(session_id));
        assert_eq!(event.event_type, EventType::SessionStarted);
        assert_eq!(event.payload["source"]["app_session_id"], "native-123");
    }

    #[test]
    fn session_log_upload_serializes_without_inline_content() {
        let session_id = SessionId::new();
        let log_ref_id = LogRefId::new();
        let event = TraceEvent::session_log_uploaded(
            actor(),
            session_id.clone(),
            SessionLogUploadedPayload {
                log_ref_id: log_ref_id.clone(),
                original_path: "/tmp/session.jsonl".to_string(),
                format: SessionLogFormat::Jsonl,
                source: "cursor".to_string(),
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                size_bytes: 5,
                storage_uri: "brick-blob://sha256/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                local_path: "/repo/.brick/provenance/blobs/sha256/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                external_uri: Some("file:///tmp/session.jsonl".to_string()),
                availability: EvidenceAvailability::LocalBlob,
                repo_context_id: None,
            },
        )
        .expect("build session log event");

        let serialized = serde_json::to_string(&event).expect("serialize session log upload");
        assert!(serialized.contains("\"event_type\":\"session.log_uploaded\""));
        assert_eq!(event.session_id, Some(session_id));
        assert_eq!(event.event_type, EventType::SessionLogUploaded);
        assert_eq!(event.payload["log_ref_id"], log_ref_id.as_str());
        assert_eq!(event.payload["format"], "jsonl");
        assert!(event.payload.get("content").is_none());
    }
}
