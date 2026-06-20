//! Projection logic from append-only events into a local inspection index.
//!
//! The index is derived data. It can be deleted and rebuilt from JSONL events,
//! so this module must never become the source of truth for provenance.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use brick_protocol::{
    ArtifactAttachmentUploadedPayload, ArtifactCreatedPayload, ArtifactFileRefRecordedPayload,
    ArtifactId, ArtifactLinkedToMissionPayload, ArtifactUpdatedPayload, DiffCapturedPayload,
    EventType, MissionCreatedPayload, MissionId, MissionUpdatedPayload, OrgCreatedPayload, OrgId,
    OrgUpdatedPayload, ProjectCreatedPayload, ProjectId, ProjectUpdatedPayload,
    RepoContextCapturedPayload, SessionId, SessionLinkedToMissionPayload,
    SessionLogUploadedPayload, SessionStartedPayload, TraceEvent,
};
use chrono::Utc;

use crate::{
    IndexedArtifact, IndexedAttachment, IndexedDiff, IndexedDiffFileChange, IndexedFile,
    IndexedFileRef, IndexedMission, IndexedOrg, IndexedProject, IndexedRepoContext, IndexedSession,
    IndexedSessionLog, TraceIndex, INDEX_SCHEMA_VERSION,
};

impl TraceIndex {
    /// Rebuilds the derived graph index from the durable event stream.
    pub fn build(events: &[TraceEvent]) -> Result<Self> {
        let mut index = Self::empty(events.len());
        for event in events {
            index.apply_event(event)?;
        }
        Ok(index)
    }

    fn empty(event_count: usize) -> Self {
        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            rebuilt_at: Utc::now(),
            event_count,
            orgs: BTreeMap::new(),
            projects: BTreeMap::new(),
            missions: BTreeMap::new(),
            sessions: BTreeMap::new(),
            artifacts: BTreeMap::new(),
            attachments: BTreeMap::new(),
            diffs: BTreeMap::new(),
            session_logs: BTreeMap::new(),
            files: BTreeMap::new(),
            repo_contexts: BTreeMap::new(),
        }
    }

    fn apply_event(&mut self, event: &TraceEvent) -> Result<()> {
        match event.event_type {
            EventType::OrgCreated => self.apply_org_created(event),
            EventType::OrgUpdated => self.apply_org_updated(event),
            EventType::ProjectCreated => self.apply_project_created(event),
            EventType::ProjectUpdated => self.apply_project_updated(event),
            EventType::MissionCreated => self.apply_mission_created(event),
            EventType::MissionUpdated => self.apply_mission_updated(event),
            EventType::SessionStarted => self.apply_session_started(event),
            EventType::SessionLinkedToMission => self.apply_session_linked(event),
            EventType::SessionLogUploaded => self.apply_session_log_uploaded(event),
            EventType::ArtifactCreated => self.apply_artifact_created(event),
            EventType::ArtifactUpdated => self.apply_artifact_updated(event),
            EventType::ArtifactLinkedToMission => self.apply_artifact_linked(event),
            EventType::ArtifactFileRefRecorded => self.apply_artifact_file_ref(event),
            EventType::ArtifactAttachmentUploaded => self.apply_artifact_attachment_uploaded(event),
            EventType::DiffCaptured => self.apply_diff_captured(event),
            EventType::RepoContextCaptured => self.apply_repo_context_captured(event),
            EventType::ArtifactReviewed
            | EventType::ArtifactAccepted
            | EventType::ExternalRefLinked => Ok(()),
        }
    }

    fn apply_org_created(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(org_id) = event.org_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<OrgCreatedPayload>(event)?;
        let org = self
            .orgs
            .entry(org_id.to_string())
            .or_insert_with(|| new_org(org_id, event));
        org.name = Some(payload.name);
        org.description = payload.description;
        org.repo_context_ids
            .insert_optional(payload.repo_context_id);
        org.last_event_at = event.recorded_at;
        Ok(())
    }

    fn apply_org_updated(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(org_id) = event.org_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<OrgUpdatedPayload>(event)?;
        let org = self
            .orgs
            .entry(org_id.to_string())
            .or_insert_with(|| new_org(org_id, event));
        if let Some(name) = payload.name {
            org.name = Some(name);
        }
        if payload.description.is_some() {
            org.description = payload.description;
        }
        org.repo_context_ids
            .insert_optional(payload.repo_context_id);
        org.last_event_at = event.recorded_at;
        Ok(())
    }

    fn apply_project_created(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(project_id) = event.project_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<ProjectCreatedPayload>(event)?;
        let org_id = payload.org_id.clone();
        let project = self
            .projects
            .entry(project_id.to_string())
            .or_insert_with(|| new_project(project_id, event));
        project.org_id = Some(org_id.to_string());
        project.name = Some(payload.name);
        project.description = payload.description;
        project
            .repo_context_ids
            .insert_optional(payload.repo_context_id.clone());
        project.last_event_at = event.recorded_at;
        self.link_project_to_org(project_id, &org_id, event);
        if let Some(org) = self.orgs.get_mut(org_id.as_str()) {
            org.repo_context_ids
                .insert_optional(payload.repo_context_id);
        }
        Ok(())
    }

    fn apply_project_updated(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(project_id) = event.project_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<ProjectUpdatedPayload>(event)?;
        if let Some(org_id) = payload.org_id.as_ref() {
            self.link_project_to_org(project_id, org_id, event);
        }
        let project = self
            .projects
            .entry(project_id.to_string())
            .or_insert_with(|| new_project(project_id, event));
        if let Some(org_id) = payload.org_id {
            project.org_id = Some(org_id.to_string());
        }
        if let Some(name) = payload.name {
            project.name = Some(name);
        }
        if payload.description.is_some() {
            project.description = payload.description;
        }
        project
            .repo_context_ids
            .insert_optional(payload.repo_context_id);
        project.last_event_at = event.recorded_at;
        Ok(())
    }

    fn apply_mission_created(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(mission_id) = event.mission_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<MissionCreatedPayload>(event)?;
        self.link_mission_to_project(mission_id, &payload.project_id, event);
        let mission = self
            .missions
            .entry(mission_id.to_string())
            .or_insert_with(|| new_mission(mission_id, event));
        mission.project_id = Some(payload.project_id.to_string());
        mission.title = Some(payload.title);
        mission.description = payload.description;
        mission.status = payload.status;
        mission
            .repo_context_ids
            .insert_optional(payload.repo_context_id);
        mission.last_event_at = event.recorded_at;
        Ok(())
    }

    fn apply_mission_updated(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(mission_id) = event.mission_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<MissionUpdatedPayload>(event)?;
        if let Some(project_id) = payload.project_id.as_ref() {
            self.link_mission_to_project(mission_id, project_id, event);
        }
        let mission = self
            .missions
            .entry(mission_id.to_string())
            .or_insert_with(|| new_mission(mission_id, event));
        if let Some(project_id) = payload.project_id {
            mission.project_id = Some(project_id.to_string());
        }
        if let Some(title) = payload.title {
            mission.title = Some(title);
        }
        if payload.description.is_some() {
            mission.description = payload.description;
        }
        if let Some(status) = payload.status {
            mission.status = status;
        }
        mission
            .repo_context_ids
            .insert_optional(payload.repo_context_id);
        mission.last_event_at = event.recorded_at;
        Ok(())
    }

    fn apply_session_started(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(session_id) = event.session_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<SessionStartedPayload>(event)?;
        let session = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| new_session(session_id, event));
        session.session_name = payload.session_name;
        session.actor_id = Some(event.actor.actor_id.clone());
        session.actor_type = Some(event.actor.actor_type);
        session.source = payload.source;
        session
            .repo_context_ids
            .insert_optional(payload.repo_context_id);
        session.last_event_at = event.recorded_at;
        if let Some(mission_id) = event.mission_id.as_ref() {
            self.link_session_to_mission(session_id, mission_id, event);
        }
        Ok(())
    }

    fn apply_session_linked(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(session_id) = event.session_id.as_ref() else {
            return Ok(());
        };
        let Some(mission_id) = event.mission_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<SessionLinkedToMissionPayload>(event)?;
        self.link_session_to_mission(session_id, mission_id, event);
        if let Some(session) = self.sessions.get_mut(session_id.as_str()) {
            session
                .repo_context_ids
                .insert_optional(payload.repo_context_id.clone());
        }
        if let Some(mission) = self.missions.get_mut(mission_id.as_str()) {
            mission
                .repo_context_ids
                .insert_optional(payload.repo_context_id);
        }
        Ok(())
    }

    fn apply_session_log_uploaded(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(session_id) = event.session_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<SessionLogUploadedPayload>(event)?;
        self.session_logs.insert(
            payload.log_ref_id.to_string(),
            IndexedSessionLog {
                log_ref_id: payload.log_ref_id.to_string(),
                session_id: session_id.to_string(),
                original_path: payload.original_path,
                format: payload.format,
                source: payload.source,
                sha256: payload.sha256,
                size_bytes: payload.size_bytes,
                storage_uri: payload.storage_uri,
                local_path: payload.local_path,
                external_uri: payload.external_uri,
                availability: payload.availability,
                repo_context_id: payload.repo_context_id.as_ref().map(ToString::to_string),
                uploaded_at: event.recorded_at,
            },
        );
        let session = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| new_session(session_id, event));
        session.log_ref_ids.insert(payload.log_ref_id.to_string());
        session
            .repo_context_ids
            .insert_optional(payload.repo_context_id);
        session.last_event_at = event.recorded_at;
        Ok(())
    }

    fn apply_artifact_created(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(artifact_id) = event.artifact_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<ArtifactCreatedPayload>(event)?;
        let artifact = self
            .artifacts
            .entry(artifact_id.to_string())
            .or_insert_with(|| new_artifact(artifact_id, event));
        artifact.artifact_kind = Some(payload.artifact_kind);
        artifact.title = Some(payload.title);
        artifact.body = payload.body;
        artifact
            .repo_context_ids
            .insert_optional(payload.repo_context_id);
        artifact.last_event_at = event.recorded_at;
        if let Some(mission_id) = event.mission_id.as_ref() {
            self.link_artifact_to_mission(artifact_id, mission_id, event);
        }
        if let Some(session_id) = event.session_id.as_ref() {
            self.link_artifact_to_session(artifact_id, session_id, event);
        }
        Ok(())
    }

    fn apply_artifact_updated(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(artifact_id) = event.artifact_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<ArtifactUpdatedPayload>(event)?;
        let artifact = self
            .artifacts
            .entry(artifact_id.to_string())
            .or_insert_with(|| new_artifact(artifact_id, event));
        if let Some(title) = payload.title {
            artifact.title = Some(title);
        }
        if payload.body.is_some() {
            artifact.body = payload.body;
        }
        if let Some(artifact_kind) = payload.artifact_kind {
            artifact.artifact_kind = Some(artifact_kind);
        }
        artifact
            .repo_context_ids
            .insert_optional(payload.repo_context_id);
        artifact.last_event_at = event.recorded_at;
        if let Some(session_id) = event.session_id.as_ref() {
            self.link_artifact_to_session(artifact_id, session_id, event);
        }
        Ok(())
    }

    fn apply_artifact_linked(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(artifact_id) = event.artifact_id.as_ref() else {
            return Ok(());
        };
        let Some(mission_id) = event.mission_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<ArtifactLinkedToMissionPayload>(event)?;
        self.link_artifact_to_mission(artifact_id, mission_id, event);
        if let Some(artifact) = self.artifacts.get_mut(artifact_id.as_str()) {
            artifact
                .repo_context_ids
                .insert_optional(payload.repo_context_id.clone());
        }
        if let Some(mission) = self.missions.get_mut(mission_id.as_str()) {
            mission
                .repo_context_ids
                .insert_optional(payload.repo_context_id);
        }
        Ok(())
    }

    fn apply_artifact_file_ref(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(artifact_id) = event.artifact_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<ArtifactFileRefRecordedPayload>(event)?;
        let file = self
            .files
            .entry(payload.path.clone())
            .or_insert_with(|| IndexedFile::new(payload.path.clone()));
        file.file_refs.push(IndexedFileRef {
            file_ref_id: payload.file_ref_id.to_string(),
            artifact_id: artifact_id.to_string(),
            session_id: event.session_id.as_ref().map(ToString::to_string),
            repo_context_id: payload.repo_context_id.as_ref().map(ToString::to_string),
            recorded_at: event.recorded_at,
        });
        if let Some(artifact) = self.artifacts.get_mut(artifact_id.as_str()) {
            artifact.file_paths.insert(payload.path);
            artifact
                .repo_context_ids
                .insert_optional(payload.repo_context_id);
            artifact.last_event_at = event.recorded_at;
        }
        if let Some(session_id) = event.session_id.as_ref() {
            self.link_artifact_to_session(artifact_id, session_id, event);
        }
        Ok(())
    }

    fn apply_artifact_attachment_uploaded(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(artifact_id) = event.artifact_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<ArtifactAttachmentUploadedPayload>(event)?;
        self.attachments.insert(
            payload.attachment_id.to_string(),
            IndexedAttachment {
                attachment_id: payload.attachment_id.to_string(),
                artifact_id: artifact_id.to_string(),
                session_id: event.session_id.as_ref().map(ToString::to_string),
                name: payload.name,
                original_path: payload.original_path,
                content_type: payload.content_type,
                sha256: payload.sha256,
                size_bytes: payload.size_bytes,
                storage_uri: payload.storage_uri,
                external_uri: payload.external_uri,
                availability: payload.availability,
                repo_context_id: payload.repo_context_id.as_ref().map(ToString::to_string),
                uploaded_at: event.recorded_at,
            },
        );
        let artifact = self
            .artifacts
            .entry(artifact_id.to_string())
            .or_insert_with(|| new_artifact(artifact_id, event));
        artifact
            .attachment_ids
            .insert(payload.attachment_id.to_string());
        artifact
            .repo_context_ids
            .insert_optional(payload.repo_context_id);
        artifact.last_event_at = event.recorded_at;
        if let Some(session_id) = event.session_id.as_ref() {
            self.link_artifact_to_session(artifact_id, session_id, event);
        }
        Ok(())
    }

    fn apply_diff_captured(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(artifact_id) = event.artifact_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<DiffCapturedPayload>(event)?;
        let diff_id = event.event_id.to_string();
        let file_changes = payload
            .file_changes
            .iter()
            .map(|change| IndexedDiffFileChange {
                path: change.path.clone(),
                old_path: change.old_path.clone(),
                change_kind: change.change_kind,
                additions: change.additions,
                deletions: change.deletions,
                hunks: change.hunks.clone(),
            })
            .collect::<Vec<_>>();
        let additions = payload
            .file_changes
            .iter()
            .filter_map(|change| change.additions)
            .sum();
        let deletions = payload
            .file_changes
            .iter()
            .filter_map(|change| change.deletions)
            .sum();
        let binary_file_count = payload
            .file_changes
            .iter()
            .filter(|change| change.additions.is_none() || change.deletions.is_none())
            .count();

        self.diffs.insert(
            diff_id.clone(),
            IndexedDiff {
                diff_id: diff_id.clone(),
                artifact_id: artifact_id.to_string(),
                session_id: event.session_id.as_ref().map(ToString::to_string),
                mission_id: event.mission_id.as_ref().map(ToString::to_string),
                diff_target: payload.diff_target,
                base_commit: payload.base_commit,
                head_commit: payload.head_commit,
                patch_id: payload.patch_id,
                summary_hash: payload.summary_hash,
                file_count: file_changes.len(),
                additions,
                deletions,
                binary_file_count,
                repo_context_id: payload.repo_context_id.as_ref().map(ToString::to_string),
                captured_at: event.recorded_at,
                file_changes,
            },
        );

        let artifact = self
            .artifacts
            .entry(artifact_id.to_string())
            .or_insert_with(|| new_artifact(artifact_id, event));
        artifact.diff_ids.insert(diff_id);
        artifact
            .repo_context_ids
            .insert_optional(payload.repo_context_id.clone());
        artifact.last_event_at = event.recorded_at;
        for change in payload.file_changes {
            artifact.file_paths.insert(change.path.clone());
            let file = self
                .files
                .entry(change.path.clone())
                .or_insert_with(|| IndexedFile::new(change.path));
            file.file_refs.push(IndexedFileRef {
                file_ref_id: event.event_id.to_string(),
                artifact_id: artifact_id.to_string(),
                session_id: event.session_id.as_ref().map(ToString::to_string),
                repo_context_id: payload.repo_context_id.as_ref().map(ToString::to_string),
                recorded_at: event.recorded_at,
            });
        }
        if let Some(session_id) = event.session_id.as_ref() {
            self.link_artifact_to_session(artifact_id, session_id, event);
        }
        if let Some(mission_id) = event.mission_id.as_ref() {
            self.link_artifact_to_mission(artifact_id, mission_id, event);
        }
        Ok(())
    }

    fn apply_repo_context_captured(&mut self, event: &TraceEvent) -> Result<()> {
        let Some(repo_context_id) = event.repo_context_id.as_ref() else {
            return Ok(());
        };
        let payload = payload::<RepoContextCapturedPayload>(event)?;
        self.repo_contexts.insert(
            repo_context_id.to_string(),
            IndexedRepoContext {
                repo_context_id: repo_context_id.to_string(),
                branch: payload.branch,
                head_commit: payload.head_commit,
                dirty: payload.dirty,
                captured_at: event.recorded_at,
            },
        );
        Ok(())
    }

    fn link_project_to_org(&mut self, project_id: &ProjectId, org_id: &OrgId, event: &TraceEvent) {
        let org = self
            .orgs
            .entry(org_id.to_string())
            .or_insert_with(|| new_org(org_id, event));
        org.project_ids.insert(project_id.to_string());
        org.last_event_at = event.recorded_at;

        let project = self
            .projects
            .entry(project_id.to_string())
            .or_insert_with(|| new_project(project_id, event));
        project.org_id = Some(org_id.to_string());
        project.last_event_at = event.recorded_at;
    }

    fn link_mission_to_project(
        &mut self,
        mission_id: &MissionId,
        project_id: &ProjectId,
        event: &TraceEvent,
    ) {
        let project = self
            .projects
            .entry(project_id.to_string())
            .or_insert_with(|| new_project(project_id, event));
        project.mission_ids.insert(mission_id.to_string());
        project.last_event_at = event.recorded_at;

        let mission = self
            .missions
            .entry(mission_id.to_string())
            .or_insert_with(|| new_mission(mission_id, event));
        mission.project_id = Some(project_id.to_string());
        mission.last_event_at = event.recorded_at;
    }

    fn link_session_to_mission(
        &mut self,
        session_id: &SessionId,
        mission_id: &MissionId,
        event: &TraceEvent,
    ) {
        let mission = self
            .missions
            .entry(mission_id.to_string())
            .or_insert_with(|| new_mission(mission_id, event));
        mission.session_ids.insert(session_id.to_string());
        mission.last_event_at = event.recorded_at;

        let session = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| new_session(session_id, event));
        session.mission_ids.insert(mission_id.to_string());
        session.last_event_at = event.recorded_at;
    }

    fn link_artifact_to_mission(
        &mut self,
        artifact_id: &ArtifactId,
        mission_id: &MissionId,
        event: &TraceEvent,
    ) {
        let mission = self
            .missions
            .entry(mission_id.to_string())
            .or_insert_with(|| new_mission(mission_id, event));
        mission.artifact_ids.insert(artifact_id.to_string());
        mission.last_event_at = event.recorded_at;

        let artifact = self
            .artifacts
            .entry(artifact_id.to_string())
            .or_insert_with(|| new_artifact(artifact_id, event));
        artifact.mission_ids.insert(mission_id.to_string());
        artifact.last_event_at = event.recorded_at;
    }

    fn link_artifact_to_session(
        &mut self,
        artifact_id: &ArtifactId,
        session_id: &SessionId,
        event: &TraceEvent,
    ) {
        let session = self
            .sessions
            .entry(session_id.to_string())
            .or_insert_with(|| new_session(session_id, event));
        session.artifact_ids.insert(artifact_id.to_string());
        session.last_event_at = event.recorded_at;

        let artifact = self
            .artifacts
            .entry(artifact_id.to_string())
            .or_insert_with(|| new_artifact(artifact_id, event));
        artifact.session_ids.insert(session_id.to_string());
        artifact.last_event_at = event.recorded_at;
    }
}

trait InsertOptionalId {
    fn insert_optional<T>(&mut self, id: Option<T>)
    where
        T: ToString;
}

impl InsertOptionalId for BTreeSet<String> {
    fn insert_optional<T>(&mut self, id: Option<T>)
    where
        T: ToString,
    {
        if let Some(value) = id {
            self.insert(value.to_string());
        }
    }
}

fn new_org(org_id: &OrgId, event: &TraceEvent) -> IndexedOrg {
    IndexedOrg::blank(org_id.to_string(), event.recorded_at)
}

fn new_project(project_id: &ProjectId, event: &TraceEvent) -> IndexedProject {
    IndexedProject::blank(project_id.to_string(), event.recorded_at)
}

fn new_mission(mission_id: &MissionId, event: &TraceEvent) -> IndexedMission {
    IndexedMission::blank(mission_id.to_string(), event.recorded_at)
}

fn new_session(session_id: &SessionId, event: &TraceEvent) -> IndexedSession {
    IndexedSession::blank(session_id.to_string(), event.recorded_at, &event.actor)
}

fn new_artifact(artifact_id: &ArtifactId, event: &TraceEvent) -> IndexedArtifact {
    IndexedArtifact::blank(artifact_id.to_string(), event.recorded_at)
}

fn payload<T>(event: &TraceEvent) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_value(event.payload.clone()).with_context(|| {
        format!(
            "failed to parse payload for event {} ({})",
            event.event_id,
            event_type_name(event.event_type)
        )
    })
}

fn event_type_name(event_type: EventType) -> &'static str {
    match event_type {
        EventType::OrgCreated => "org.created",
        EventType::OrgUpdated => "org.updated",
        EventType::ProjectCreated => "project.created",
        EventType::ProjectUpdated => "project.updated",
        EventType::MissionCreated => "mission.created",
        EventType::MissionUpdated => "mission.updated",
        EventType::SessionStarted => "session.started",
        EventType::SessionLinkedToMission => "session.linked_to_mission",
        EventType::SessionLogUploaded => "session.log_uploaded",
        EventType::ArtifactCreated => "artifact.created",
        EventType::ArtifactUpdated => "artifact.updated",
        EventType::ArtifactLinkedToMission => "artifact.linked_to_mission",
        EventType::ArtifactFileRefRecorded => "artifact.file_ref_recorded",
        EventType::ArtifactAttachmentUploaded => "artifact.attachment_uploaded",
        EventType::ArtifactReviewed => "artifact.reviewed",
        EventType::ArtifactAccepted => "artifact.accepted",
        EventType::RepoContextCaptured => "repo_context.captured",
        EventType::DiffCaptured => "diff.captured",
        EventType::ExternalRefLinked => "external_ref.linked",
    }
}

#[cfg(test)]
mod tests {
    use brick_protocol::{
        ActorRef, ActorType, ArtifactAttachmentUploadedPayload, ArtifactId, ArtifactKind,
        AttachmentId, EvidenceAvailability, FileRefId, LogRefId, MissionStatus, ProjectId,
        SessionLogFormat, SessionLogUploadedPayload, SessionSource,
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
    fn builds_mission_session_artifact_file_index() {
        let project_id = ProjectId::new();
        let mission_id = MissionId::new();
        let session_id = SessionId::new();
        let artifact_id = ArtifactId::new();
        let mission = TraceEvent::mission_created(
            actor(),
            mission_id.clone(),
            MissionCreatedPayload {
                project_id: project_id.clone(),
                title: "Build index".to_string(),
                description: None,
                status: MissionStatus::Planned,
                repo_context_id: None,
            },
        )
        .expect("mission event");
        let session = TraceEvent::session_started(
            actor(),
            session_id.clone(),
            Some(mission_id.clone()),
            SessionStartedPayload {
                session_name: Some("local".to_string()),
                source: SessionSource::default(),
                repo_context_id: None,
            },
        )
        .expect("session event");
        let artifact = TraceEvent::artifact_created(
            actor(),
            artifact_id.clone(),
            Some(mission_id.clone()),
            Some(session_id.clone()),
            ArtifactCreatedPayload {
                artifact_kind: ArtifactKind::Decision,
                title: "Use JSON cache".to_string(),
                body: None,
                repo_context_id: None,
            },
        )
        .expect("artifact event");
        let file = TraceEvent::artifact_file_ref_recorded(
            actor(),
            artifact_id.clone(),
            Some(session_id.clone()),
            ArtifactFileRefRecordedPayload {
                file_ref_id: FileRefId::new(),
                path: "src/main.rs".to_string(),
                repo_context_id: None,
            },
        )
        .expect("file event");
        let attachment_id = AttachmentId::new();
        let attachment = TraceEvent::artifact_attachment_uploaded(
            actor(),
            artifact_id.clone(),
            Some(session_id.clone()),
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
        .expect("attachment event");
        let log_ref_id = LogRefId::new();
        let session_log = TraceEvent::session_log_uploaded(
            actor(),
            session_id.clone(),
            SessionLogUploadedPayload {
                log_ref_id: log_ref_id.clone(),
                original_path: "/tmp/session.md".to_string(),
                format: SessionLogFormat::Markdown,
                source: "cursor".to_string(),
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                size_bytes: 5,
                storage_uri: "brick-blob://sha256/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                local_path: "/tmp/blob".to_string(),
                external_uri: Some("file:///tmp/session.md".to_string()),
                availability: EvidenceAvailability::LocalBlob,
                repo_context_id: None,
            },
        )
        .expect("session log event");

        let artifact_created_at = artifact.recorded_at;
        let mut update = TraceEvent::artifact_updated(
            actor(),
            artifact_id.clone(),
            Some(session_id.clone()),
            ArtifactUpdatedPayload {
                title: Some("Use append-only JSON cache".to_string()),
                body: Some("Updated without mutating old events".to_string()),
                artifact_kind: Some(ArtifactKind::Review),
                repo_context_id: None,
            },
        )
        .expect("artifact update event");
        update.recorded_at = artifact_created_at + chrono::Duration::seconds(10);

        let index = TraceIndex::build(&[
            mission,
            session,
            artifact,
            file,
            attachment,
            session_log,
            update,
        ])
        .expect("build index");
        assert_eq!(index.event_count, 7);
        assert!(index
            .missions
            .get(mission_id.as_str())
            .expect("mission indexed")
            .session_ids
            .contains(session_id.as_str()));
        let indexed_artifact = index
            .artifacts
            .get(artifact_id.as_str())
            .expect("artifact indexed");
        assert!(indexed_artifact.file_paths.contains("src/main.rs"));
        assert!(indexed_artifact
            .attachment_ids
            .contains(attachment_id.as_str()));
        let indexed_attachment = index
            .attachments
            .get(attachment_id.as_str())
            .expect("attachment indexed");
        assert_eq!(indexed_attachment.artifact_id, artifact_id.to_string());
        assert_eq!(indexed_attachment.name, "report.txt");
        assert_eq!(indexed_attachment.size_bytes, 5);
        let indexed_session = index
            .sessions
            .get(session_id.as_str())
            .expect("session indexed");
        assert!(indexed_session.log_ref_ids.contains(log_ref_id.as_str()));
        let indexed_log = index
            .session_logs
            .get(log_ref_id.as_str())
            .expect("session log indexed");
        assert_eq!(indexed_log.session_id, session_id.to_string());
        assert_eq!(indexed_log.format, SessionLogFormat::Markdown);
        assert_eq!(indexed_log.size_bytes, 5);
        assert_eq!(
            indexed_artifact.title.as_deref(),
            Some("Use append-only JSON cache")
        );
        assert_eq!(
            indexed_artifact.body.as_deref(),
            Some("Updated without mutating old events")
        );
        assert_eq!(indexed_artifact.artifact_kind, Some(ArtifactKind::Review));
        assert_eq!(indexed_artifact.created_at, artifact_created_at);
        assert_eq!(
            indexed_artifact.last_event_at,
            artifact_created_at + chrono::Duration::seconds(10)
        );
    }
}
