//! Command handlers that append typed events to the local store.
//!
//! Each write command captures repo context first, then records the domain
//! event. This preserves the provenance chain without making Git the storage
//! backend for trace data.

use anyhow::{anyhow, Context, Result};
use brick_core::{
    capture_diff, capture_repo_context, DiffCaptureRequest, LocalStore, SourceProfile,
};
use brick_importers::{import_traces, ImportRequest, ImportSource};
use brick_protocol::{
    ActorRef, ArtifactAttachmentUploadedPayload, ArtifactCreatedPayload,
    ArtifactFileRefRecordedPayload, ArtifactId, ArtifactKind, ArtifactLinkedToMissionPayload,
    ArtifactUpdatedPayload, AttachmentId, DiffTarget, FileRefId, LogRefId, MissionCreatedPayload,
    MissionId, RepoContextId, SessionId, SessionLinkedToMissionPayload, SessionLogFormat,
    SessionLogUploadedPayload, SessionStartedPayload, TraceEvent,
};

use crate::args::{
    AgentImportArgs, ArtifactCommand, ArtifactKindArg, CiImportArgs, DiffCommand, DiffTargetArg,
    IdentityArgs, ImportCommand, MissionCommand, SessionCommand, SessionLogFormatArg,
};
use crate::context::{parse_optional_id, resolve_cli_identity};
use crate::output::print_session_env;

/// Executes diff capture write subcommands against the local event queue.
pub fn handle_diff(
    command: DiffCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    match command {
        DiffCommand::Capture {
            artifact,
            session,
            mission,
            target,
            base,
            head,
        } => {
            let artifact_id = artifact
                .parse::<ArtifactId>()
                .context("invalid artifact id")?;
            let session_id = parse_optional_id::<SessionId>(session.as_deref(), "session")?;
            let mission_id = parse_optional_id::<MissionId>(mission.as_deref(), "mission")?;
            let diff_target = diff_target_from_arg(target, base.as_ref(), head.as_ref());
            let identity = resolve_cli_identity(
                store,
                identity_args,
                mission_id.clone(),
                session_id.clone(),
                source_profile,
            )?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let payload = capture_diff(
                repo_root,
                DiffCaptureRequest {
                    target: diff_target,
                    base_commit: base,
                    head_commit: head,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            let file_count = payload.file_changes.len();
            let additions: u64 = payload
                .file_changes
                .iter()
                .filter_map(|change| change.additions)
                .sum();
            let deletions: u64 = payload
                .file_changes
                .iter()
                .filter_map(|change| change.deletions)
                .sum();
            let patch_id = payload.patch_id.clone().unwrap_or_default();
            let summary_hash = payload.summary_hash.clone();
            let event = TraceEvent::diff_captured(
                actor,
                artifact_id.clone(),
                Some(identity.session_id.clone()),
                identity.mission_id.clone(),
                payload,
            )?;
            store.append_event(&event)?;
            println!("artifact_id={artifact_id}");
            println!("event_id={}", event.event_id);
            println!("file_count={file_count}");
            println!("additions={additions}");
            println!("deletions={deletions}");
            println!("patch_id={patch_id}");
            println!("summary_hash={summary_hash}");
        }
    }
    Ok(())
}

/// Executes Mission write subcommands against the local event queue.
pub fn handle_mission(
    command: MissionCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    match command {
        MissionCommand::Create { title, description } => {
            let identity = resolve_cli_identity(store, identity_args, None, None, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let mission_id = MissionId::new();
            let event = TraceEvent::mission_created(
                actor,
                mission_id.clone(),
                MissionCreatedPayload {
                    title,
                    description,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("mission_id={mission_id}");
        }
    }
    Ok(())
}

/// Executes Session write/link subcommands against the local event queue.
pub fn handle_session(
    command: SessionCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    match command {
        SessionCommand::Start {
            mission,
            name,
            set_current,
            print_env,
        } => {
            let mission_id = parse_optional_id::<MissionId>(mission.as_deref(), "mission")?;
            let identity = resolve_cli_identity(
                store,
                identity_args,
                mission_id.clone(),
                None,
                source_profile,
            )?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let event = TraceEvent::session_started(
                actor,
                identity.session_id.clone(),
                identity.mission_id.clone(),
                SessionStartedPayload {
                    session_name: name,
                    source: identity.session_source.clone(),
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;

            if set_current {
                store.write_current_context(&identity.current_context())?;
            }
            if print_env {
                print_session_env(&identity);
            }
            println!("session_id={}", identity.session_id);
        }
        SessionCommand::Link {
            mission,
            session,
            relationship,
        } => {
            let mission_id = mission.parse::<MissionId>().context("invalid mission id")?;
            let session_id = session.parse::<SessionId>().context("invalid session id")?;
            let identity = resolve_cli_identity(
                store,
                identity_args,
                Some(mission_id.clone()),
                Some(session_id.clone()),
                source_profile,
            )?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let event = TraceEvent::session_linked_to_mission(
                actor,
                session_id,
                mission_id,
                SessionLinkedToMissionPayload {
                    relationship,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("linked_session_to_mission=true");
        }
        SessionCommand::UploadLog {
            session,
            path,
            format,
            source,
        } => {
            let session_id = session.parse::<SessionId>().context("invalid session id")?;
            let identity = resolve_cli_identity(
                store,
                identity_args,
                None,
                Some(session_id.clone()),
                source_profile,
            )?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let stored = store.log_store().store_file(&path)?;
            let log_ref_id = LogRefId::new();
            let event = TraceEvent::session_log_uploaded(
                actor,
                session_id.clone(),
                SessionLogUploadedPayload {
                    log_ref_id: log_ref_id.clone(),
                    original_path: stored.original_path.display().to_string(),
                    format: format
                        .map(session_log_format_from_arg)
                        .unwrap_or_else(|| infer_session_log_format(&stored.original_path)),
                    source: source
                        .or_else(|| identity.session_source.app_id.clone())
                        .unwrap_or_else(|| "unknown".to_string()),
                    sha256: stored.sha256.clone(),
                    size_bytes: stored.size_bytes,
                    storage_uri: stored.storage_uri.clone(),
                    local_path: stored.storage_path.display().to_string(),
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("log_ref_id={log_ref_id}");
            println!("session_id={session_id}");
            println!("sha256={}", stored.sha256);
            println!("size_bytes={}", stored.size_bytes);
            println!("storage_uri={}", stored.storage_uri);
            println!("storage_path={}", stored.storage_path.display());
        }
        SessionCommand::Current | SessionCommand::List { .. } | SessionCommand::Find { .. } => {}
    }
    Ok(())
}

/// Executes external trace import subcommands against the local event queue.
pub fn handle_import(
    command: ImportCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    let (source, paths, session, mission, app_session_id, app_session_name) = match command {
        ImportCommand::Cursor(args) => agent_import_parts(ImportSource::Cursor, args),
        ImportCommand::Codex(args) => agent_import_parts(ImportSource::Codex, args),
        ImportCommand::ClaudeCode(args) => agent_import_parts(ImportSource::ClaudeCode, args),
        ImportCommand::Ci(args) => ci_import_parts(args),
    };
    let session_id = parse_optional_id::<SessionId>(session.as_deref(), "session")?;
    let mission_id = parse_optional_id::<MissionId>(mission.as_deref(), "mission")?;
    let identity = resolve_cli_identity(
        store,
        identity_args,
        mission_id.clone(),
        session_id.clone(),
        source_profile,
    )?;
    let request = ImportRequest {
        source,
        paths,
        app_session_id: app_session_id.or(identity.session_source.app_session_id.clone()),
        app_session_name: app_session_name.or(identity.session_source.app_session_name.clone()),
        actor: Some(identity.actor),
        mission_id: identity.mission_id,
        session_id: Some(identity.session_id),
    };
    let result = import_traces(request)?;
    for event in &result.events {
        store.append_event(event)?;
    }
    println!("imported_event_count={}", result.imported_event_count());
    Ok(())
}

fn agent_import_parts(
    source: ImportSource,
    args: AgentImportArgs,
) -> (
    ImportSource,
    Vec<std::path::PathBuf>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    (
        source,
        args.path,
        args.session,
        args.mission,
        args.app_session_id,
        args.app_session_name,
    )
}

fn ci_import_parts(
    args: CiImportArgs,
) -> (
    ImportSource,
    Vec<std::path::PathBuf>,
    Option<String>,
    Option<String>,
    Option<String>,
    Option<String>,
) {
    (
        ImportSource::CI,
        args.path,
        args.session,
        args.mission,
        None,
        None,
    )
}

/// Executes Artifact creation, file-ref, and linking subcommands.
pub fn handle_artifact(
    command: ArtifactCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    match command {
        ArtifactCommand::Decision {
            mission,
            session,
            title,
            body,
        } => {
            let mission_id = parse_optional_id::<MissionId>(mission.as_deref(), "mission")?;
            let session_id = parse_optional_id::<SessionId>(session.as_deref(), "session")?;
            let identity =
                resolve_cli_identity(store, identity_args, mission_id, session_id, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let artifact_id = ArtifactId::new();
            let event = TraceEvent::artifact_created(
                actor,
                artifact_id.clone(),
                identity.mission_id.clone(),
                Some(identity.session_id.clone()),
                ArtifactCreatedPayload {
                    artifact_kind: ArtifactKind::Decision,
                    title,
                    body,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("artifact_id={artifact_id}");
        }
        ArtifactCommand::File {
            artifact,
            session,
            path,
        } => {
            let artifact_id = artifact
                .parse::<ArtifactId>()
                .context("invalid artifact id")?;
            let session_id = parse_optional_id::<SessionId>(session.as_deref(), "session")?;
            let identity =
                resolve_cli_identity(store, identity_args, None, session_id, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let event = TraceEvent::artifact_file_ref_recorded(
                actor,
                artifact_id,
                Some(identity.session_id),
                ArtifactFileRefRecordedPayload {
                    file_ref_id: FileRefId::new(),
                    path,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("file_ref_recorded=true");
        }
        ArtifactCommand::Link {
            mission,
            artifact,
            relationship,
        } => {
            let mission_id = mission.parse::<MissionId>().context("invalid mission id")?;
            let artifact_id = artifact
                .parse::<ArtifactId>()
                .context("invalid artifact id")?;
            let identity = resolve_cli_identity(
                store,
                identity_args,
                Some(mission_id.clone()),
                None,
                source_profile,
            )?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let event = TraceEvent::artifact_linked_to_mission(
                actor,
                artifact_id,
                mission_id,
                ArtifactLinkedToMissionPayload {
                    relationship,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("linked_artifact_to_mission=true");
        }
        ArtifactCommand::Update {
            artifact,
            session,
            title,
            body,
            kind,
        } => {
            if title.is_none() && body.is_none() && kind.is_none() {
                return Err(anyhow!(
                    "artifact update requires at least one of --title, --body, or --kind"
                ));
            }
            let artifact_id = artifact
                .parse::<ArtifactId>()
                .context("invalid artifact id")?;
            let session_id = parse_optional_id::<SessionId>(session.as_deref(), "session")?;
            let identity =
                resolve_cli_identity(store, identity_args, None, session_id, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let event = TraceEvent::artifact_updated(
                actor,
                artifact_id.clone(),
                Some(identity.session_id),
                ArtifactUpdatedPayload {
                    title,
                    body,
                    artifact_kind: kind.map(artifact_kind_from_arg),
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("artifact_updated=true");
            println!("artifact_id={artifact_id}");
        }
        ArtifactCommand::Upload {
            artifact,
            session,
            path,
            name,
            content_type,
        } => {
            let artifact_id = artifact
                .parse::<ArtifactId>()
                .context("invalid artifact id")?;
            let session_id = parse_optional_id::<SessionId>(session.as_deref(), "session")?;
            let identity =
                resolve_cli_identity(store, identity_args, None, session_id, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let stored = store.attachment_store().store_file(&path)?;
            let attachment_name = name.unwrap_or_else(|| {
                path.file_name()
                    .and_then(|value| value.to_str())
                    .unwrap_or("attachment")
                    .to_string()
            });
            let attachment_id = AttachmentId::new();
            let event = TraceEvent::artifact_attachment_uploaded(
                actor,
                artifact_id.clone(),
                Some(identity.session_id),
                ArtifactAttachmentUploadedPayload {
                    attachment_id: attachment_id.clone(),
                    name: attachment_name,
                    original_path: stored.original_path.display().to_string(),
                    content_type,
                    sha256: stored.sha256.clone(),
                    size_bytes: stored.size_bytes,
                    storage_uri: stored.storage_uri.clone(),
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("attachment_id={attachment_id}");
            println!("sha256={}", stored.sha256);
            println!("size_bytes={}", stored.size_bytes);
            println!("storage_uri={}", stored.storage_uri);
            println!("storage_path={}", stored.storage_path.display());
        }
    }
    Ok(())
}

fn diff_target_from_arg(
    target: DiffTargetArg,
    base: Option<&String>,
    head: Option<&String>,
) -> DiffTarget {
    if base.is_some() || head.is_some() {
        DiffTarget::Range
    } else {
        match target {
            DiffTargetArg::Working => DiffTarget::Working,
            DiffTargetArg::Staged => DiffTarget::Staged,
        }
    }
}

fn session_log_format_from_arg(format: SessionLogFormatArg) -> SessionLogFormat {
    match format {
        SessionLogFormatArg::Text => SessionLogFormat::Text,
        SessionLogFormatArg::Jsonl => SessionLogFormat::Jsonl,
        SessionLogFormatArg::Markdown => SessionLogFormat::Markdown,
        SessionLogFormatArg::Unknown => SessionLogFormat::Unknown,
    }
}

fn infer_session_log_format(path: &std::path::Path) -> SessionLogFormat {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("txt" | "log") => SessionLogFormat::Text,
        Some("jsonl") => SessionLogFormat::Jsonl,
        Some("md" | "markdown") => SessionLogFormat::Markdown,
        _ => SessionLogFormat::Unknown,
    }
}

fn artifact_kind_from_arg(kind: ArtifactKindArg) -> ArtifactKind {
    match kind {
        ArtifactKindArg::Decision => ArtifactKind::Decision,
        ArtifactKindArg::FileRef => ArtifactKind::FileRef,
        ArtifactKindArg::Patch => ArtifactKind::Patch,
        ArtifactKindArg::Review => ArtifactKind::Review,
        ArtifactKindArg::TestResult => ArtifactKind::TestResult,
        ArtifactKindArg::Acceptance => ArtifactKind::Acceptance,
        ArtifactKindArg::Note => ArtifactKind::Note,
    }
}

fn append_repo_context(
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
    actor: ActorRef,
) -> Result<RepoContextId> {
    let repo_context_id = RepoContextId::new();
    let payload = capture_repo_context(repo_root, work_dir);
    let event = TraceEvent::repo_context_captured(actor, repo_context_id.clone(), payload)?;
    store.append_event(&event)?;
    Ok(repo_context_id)
}
