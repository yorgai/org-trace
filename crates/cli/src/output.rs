//! Human-readable output helpers for status, log, and shell environment export.
//!
//! Output is intentionally simple `key=value` text so agents can parse it
//! without needing a separate JSON mode in the first local phases.

use brick_core::{capture_repo_context, LocalStore};
use brick_protocol::{ActorType, EventType};

/// Prints local queue and Git state in parseable text form.
pub fn print_status(
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
) -> anyhow::Result<()> {
    let status = store.queue_status()?;
    let repo_context = capture_repo_context(repo_root, work_dir);

    println!("initialized={}", status.initialized);
    println!("store_root={}", store.storage_root().display());
    println!("queue_files={}", status.queue_files);
    println!("queued_event_count={}", status.queued_event_count);
    println!("branch={}", repo_context.branch.as_deref().unwrap_or(""));
    println!(
        "head_commit={}",
        repo_context.head_commit.as_deref().unwrap_or("")
    );
    println!("dirty={}", repo_context.dirty);
    Ok(())
}

/// Prints recent queued events as compact one-line summaries.
pub fn print_log(store: &LocalStore, limit: usize) -> anyhow::Result<()> {
    let events = store.recent_events(limit)?;
    for event in events {
        println!(
            "{} {} mission={} session={} artifact={}",
            event.recorded_at,
            format_event_type(event.event_type),
            event
                .mission_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            event
                .session_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
            event
                .artifact_id
                .as_ref()
                .map(ToString::to_string)
                .unwrap_or_default(),
        );
    }
    Ok(())
}

/// Prints shell exports that let follow-up agent commands reuse identity.
pub fn print_session_env(identity: &brick_core::ResolvedIdentity) {
    println!("export BRICK_SESSION_ID=\"{}\"", identity.session_id);
    println!("export BRICK_ACTOR_ID=\"{}\"", identity.actor.actor_id);
    println!(
        "export BRICK_ACTOR_TYPE=\"{}\"",
        match identity.actor.actor_type {
            ActorType::Human => "human",
            ActorType::Agent => "agent",
            ActorType::System => "system",
        }
    );
    if let Some(mission_id) = &identity.mission_id {
        println!("export BRICK_MISSION_ID=\"{mission_id}\"");
    }
    if let Some(runtime_id) = &identity.runtime_id {
        println!("export BRICK_RUNTIME_ID=\"{runtime_id}\"");
    }
    if let Some(app_id) = &identity.session_source.app_id {
        println!("export BRICK_APP_ID=\"{app_id}\"");
    }
    if let Some(app_session_id) = &identity.session_source.app_session_id {
        println!("export BRICK_APP_SESSION_ID=\"{app_session_id}\"");
    }
    if let Some(app_session_name) = &identity.session_source.app_session_name {
        println!("export BRICK_APP_SESSION_NAME=\"{app_session_name}\"");
    }
}

fn format_event_type(event_type: EventType) -> &'static str {
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
        EventType::CausalLinked => "causal.linked",
    }
}
