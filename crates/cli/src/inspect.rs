//! Local index and inspection command handlers.
//!
//! Inspection reads the rebuildable cache when available and falls back to
//! rebuilding from JSONL, so users can query local provenance without a server.

use anyhow::{anyhow, Result};
use brick_core::{
    IndexedArtifact, IndexedFile, IndexedMission, IndexedOrg, IndexedProject, IndexedSession,
    LocalStore,
};
use brick_protocol::{ActorType, MissionStatus};

use crate::args::IndexCommand;

/// Executes index cache maintenance subcommands.
pub fn handle_index(command: IndexCommand, store: &LocalStore) -> Result<()> {
    match command {
        IndexCommand::Rebuild => {
            let index = store.rebuild_index()?;
            println!("index_rebuilt=true");
            println!("event_count={}", index.event_count);
            println!("missions={}", index.missions.len());
            println!("sessions={}", index.sessions.len());
            println!("artifacts={}", index.artifacts.len());
            println!("attachments={}", index.attachments.len());
            println!("session_logs={}", index.session_logs.len());
            println!("files={}", index.files.len());
        }
        IndexCommand::Status => {
            let status = store.index_status()?;
            println!("index_exists={}", status.exists);
            println!("index_event_count={}", status.event_count);
            println!(
                "index_rebuilt_at={}",
                status
                    .rebuilt_at
                    .map(|value| value.to_rfc3339())
                    .unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// Prints a local Org projection by ID.
pub fn show_org(org: String, store: &LocalStore) -> Result<()> {
    let index = store.load_or_rebuild_index()?;
    let item = index
        .orgs
        .get(&org)
        .ok_or_else(|| anyhow!("org not found: {org}"))?;
    print_org(item);
    Ok(())
}

/// Prints a local Project projection by ID.
pub fn show_project(project: String, store: &LocalStore) -> Result<()> {
    let index = store.load_or_rebuild_index()?;
    let item = index
        .projects
        .get(&project)
        .ok_or_else(|| anyhow!("project not found: {project}"))?;
    print_project(item);
    Ok(())
}

/// Prints a local Mission projection by ID.
pub fn show_mission(mission: String, store: &LocalStore) -> Result<()> {
    let index = store.load_or_rebuild_index()?;
    let item = index
        .missions
        .get(&mission)
        .ok_or_else(|| anyhow!("mission not found: {mission}"))?;
    print_mission(item);
    Ok(())
}

/// Prints a local Session projection by ID.
pub fn show_session(session: String, store: &LocalStore) -> Result<()> {
    let index = store.load_or_rebuild_index()?;
    let item = index
        .sessions
        .get(&session)
        .ok_or_else(|| anyhow!("session not found: {session}"))?;
    print_session(item);
    Ok(())
}

/// Prints a local Artifact projection by ID.
pub fn show_artifact(artifact: String, store: &LocalStore) -> Result<()> {
    let index = store.load_or_rebuild_index()?;
    let item = index
        .artifacts
        .get(&artifact)
        .ok_or_else(|| anyhow!("artifact not found: {artifact}"))?;
    print_artifact(item);
    Ok(())
}

/// Prints local file evidence by path.
pub fn show_file(path: String, store: &LocalStore) -> Result<()> {
    let index = store.load_or_rebuild_index()?;
    let item = index
        .files
        .get(&path)
        .ok_or_else(|| anyhow!("file not found in trace index: {path}"))?;
    print_file(item);
    Ok(())
}

fn print_org(item: &IndexedOrg) {
    println!("org_id={}", item.org_id);
    println!("name={}", item.name.as_deref().unwrap_or(""));
    println!("description={}", item.description.as_deref().unwrap_or(""));
    println!("created_at={}", item.created_at);
    println!("last_event_at={}", item.last_event_at);
    print_set("projects", item.project_ids.iter());
    print_set("repo_contexts", item.repo_context_ids.iter());
}

fn print_project(item: &IndexedProject) {
    println!("project_id={}", item.project_id);
    println!("org_id={}", item.org_id.as_deref().unwrap_or(""));
    println!("name={}", item.name.as_deref().unwrap_or(""));
    println!("description={}", item.description.as_deref().unwrap_or(""));
    println!("created_at={}", item.created_at);
    println!("last_event_at={}", item.last_event_at);
    print_set("missions", item.mission_ids.iter());
    print_set("repo_contexts", item.repo_context_ids.iter());
}

fn print_mission(item: &IndexedMission) {
    println!("mission_id={}", item.mission_id);
    println!("project_id={}", item.project_id.as_deref().unwrap_or(""));
    println!("title={}", item.title.as_deref().unwrap_or(""));
    println!("description={}", item.description.as_deref().unwrap_or(""));
    println!("status={}", format_mission_status(item.status));
    println!("created_at={}", item.created_at);
    println!("last_event_at={}", item.last_event_at);
    print_set("sessions", item.session_ids.iter());
    print_set("artifacts", item.artifact_ids.iter());
    print_set("repo_contexts", item.repo_context_ids.iter());
}

fn print_session(item: &IndexedSession) {
    println!("session_id={}", item.session_id);
    println!(
        "session_name={}",
        item.session_name.as_deref().unwrap_or("")
    );
    println!("started_at={}", item.started_at);
    println!("last_event_at={}", item.last_event_at);
    println!("actor_id={}", item.actor_id.as_deref().unwrap_or(""));
    println!(
        "actor_type={}",
        item.actor_type.map(format_actor_type).unwrap_or("")
    );
    println!("app_id={}", item.source.app_id.as_deref().unwrap_or(""));
    println!(
        "app_session_id={}",
        item.source.app_session_id.as_deref().unwrap_or("")
    );
    println!(
        "app_session_name={}",
        item.source.app_session_name.as_deref().unwrap_or("")
    );
    println!(
        "runtime_id={}",
        item.source.runtime_id.as_deref().unwrap_or("")
    );
    print_set("missions", item.mission_ids.iter());
    print_set("artifacts", item.artifact_ids.iter());
    print_set("log_refs", item.log_ref_ids.iter());
    println!("log_count={}", item.log_ref_ids.len());
    print_set("repo_contexts", item.repo_context_ids.iter());
}

fn print_artifact(item: &IndexedArtifact) {
    println!("artifact_id={}", item.artifact_id);
    println!(
        "artifact_kind={}",
        item.artifact_kind
            .map(|kind| format!("{kind:?}"))
            .unwrap_or_default()
    );
    println!("title={}", item.title.as_deref().unwrap_or(""));
    println!("body={}", item.body.as_deref().unwrap_or(""));
    println!("created_at={}", item.created_at);
    println!("last_event_at={}", item.last_event_at);
    print_set("missions", item.mission_ids.iter());
    print_set("sessions", item.session_ids.iter());
    print_set("files", item.file_paths.iter());
    print_set("attachments", item.attachment_ids.iter());
    print_set("diffs", item.diff_ids.iter());
    println!("diff_count={}", item.diff_ids.len());
    print_set("repo_contexts", item.repo_context_ids.iter());
}

fn print_file(item: &IndexedFile) {
    println!("path={}", item.path);
    println!("file_ref_count={}", item.file_refs.len());
    for file_ref in &item.file_refs {
        println!(
            "file_ref={} artifact={} session={} repo_context={} recorded_at={}",
            file_ref.file_ref_id,
            file_ref.artifact_id,
            file_ref.session_id.as_deref().unwrap_or(""),
            file_ref.repo_context_id.as_deref().unwrap_or(""),
            file_ref.recorded_at
        );
    }
}

fn print_set<'a>(label: &str, values: impl Iterator<Item = &'a String>) {
    let joined = values.cloned().collect::<Vec<_>>().join(",");
    println!("{label}={joined}");
}

fn format_mission_status(status: MissionStatus) -> &'static str {
    match status {
        MissionStatus::Planned => "planned",
        MissionStatus::Active => "active",
        MissionStatus::Blocked => "blocked",
        MissionStatus::Completed => "completed",
        MissionStatus::Archived => "archived",
    }
}

fn format_actor_type(actor_type: ActorType) -> &'static str {
    match actor_type {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}
