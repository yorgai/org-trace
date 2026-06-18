//! Command handlers that append typed events to the local store.
//!
//! Each write command captures repo context first, then records the domain
//! event. This preserves the provenance chain without making Git the storage
//! backend for trace data.

use anyhow::{anyhow, Context, Result};
use brick_core::{
    capture_diff, capture_repo_context, list_native_source_sessions, BrickConfig,
    DiffCaptureRequest, LocalStore, NativeSourceSession, SourceProfile,
};
use brick_importers::{import_traces, ImportRequest, ImportSource};
use brick_protocol::{
    ActorRef, ArtifactAttachmentUploadedPayload, ArtifactCreatedPayload,
    ArtifactFileRefRecordedPayload, ArtifactId, ArtifactKind, ArtifactLinkedToMissionPayload,
    ArtifactUpdatedPayload, AttachmentId, DiffTarget, EvidenceAvailability, FileRefId, LogRefId,
    MissionCreatedPayload, MissionId, MissionStatus, MissionUpdatedPayload, OrgCreatedPayload,
    OrgId, ProjectCreatedPayload, ProjectId, RepoContextId, SessionId,
    SessionLinkedToMissionPayload, SessionLogFormat, SessionLogUploadedPayload,
    SessionStartedPayload, TraceEvent,
};

use crate::args::{
    AgentImportArgs, ArtifactCommand, ArtifactKindArg, CiImportArgs, DiffTargetArg,
    EvidenceCommand, IdentityArgs, ImportCommand, MissionCommand, MissionStatusArg,
    NativeImportCommand, OrgCommand, ProjectCommand, SessionCommand, SessionLogFormatArg,
};
use crate::context::{parse_optional_id, resolve_cli_identity};
use crate::output::print_session_env;

/// Executes Evidence write subcommands against the local event queue.
pub fn handle_evidence(
    command: EvidenceCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
    source_profile: Option<&SourceProfile>,
    brick_config: &BrickConfig,
) -> Result<()> {
    match command {
        EvidenceCommand::Diff {
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
        EvidenceCommand::Attach {
            artifact,
            session,
            path,
            name,
            content_type,
            copy,
        } => {
            let artifact_id = artifact
                .parse::<ArtifactId>()
                .context("invalid artifact id")?;
            let session_id = parse_optional_id::<SessionId>(session.as_deref(), "session")?;
            let identity =
                resolve_cli_identity(store, identity_args, None, session_id, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let should_copy = copy
                || source_profile
                    .map(|profile| profile.should_upload_full_evidence(brick_config))
                    .unwrap_or(brick_config.evidence.default_full_evidence_upload);
            let pointer = store.attachment_store().inspect_file(&path)?;
            let (storage_uri, storage_path, availability) = if should_copy {
                let stored = store.attachment_store().store_file(&path)?;
                (
                    stored.storage_uri,
                    stored.storage_path.display().to_string(),
                    EvidenceAvailability::LocalBlob,
                )
            } else {
                (
                    format!("file://{}", pointer.original_path.display()),
                    String::new(),
                    EvidenceAvailability::LocalPointer,
                )
            };
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
                    original_path: pointer.original_path.display().to_string(),
                    content_type,
                    sha256: pointer.sha256.clone(),
                    size_bytes: pointer.size_bytes,
                    storage_uri: storage_uri.clone(),
                    external_uri: Some(format!("file://{}", pointer.original_path.display())),
                    availability,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("attachment_id={attachment_id}");
            println!("sha256={}", pointer.sha256);
            println!("size_bytes={}", pointer.size_bytes);
            println!("storage_uri={storage_uri}");
            println!("storage_path={storage_path}");
            println!(
                "availability={}",
                format_evidence_availability(availability)
            );
        }
        EvidenceCommand::File {
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
        EvidenceCommand::Log {
            session,
            path,
            format,
            source,
            copy,
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
            let should_copy = copy
                || source_profile
                    .map(|profile| profile.should_upload_full_evidence(brick_config))
                    .unwrap_or(brick_config.evidence.default_full_evidence_upload);
            let pointer = store.attachment_store().inspect_file(&path)?;
            let (storage_uri, local_path, availability) = if should_copy {
                let stored = store.log_store().store_file(&path)?;
                (
                    stored.storage_uri,
                    stored.storage_path.display().to_string(),
                    EvidenceAvailability::LocalBlob,
                )
            } else {
                (
                    format!("file://{}", pointer.original_path.display()),
                    String::new(),
                    EvidenceAvailability::LocalPointer,
                )
            };
            let log_ref_id = LogRefId::new();
            let event = TraceEvent::session_log_uploaded(
                actor,
                session_id.clone(),
                SessionLogUploadedPayload {
                    log_ref_id: log_ref_id.clone(),
                    original_path: pointer.original_path.display().to_string(),
                    format: format
                        .map(session_log_format_from_arg)
                        .unwrap_or_else(|| infer_session_log_format(&pointer.original_path)),
                    source: source
                        .or_else(|| identity.session_source.app_id.clone())
                        .unwrap_or_else(|| "unknown".to_string()),
                    sha256: pointer.sha256.clone(),
                    size_bytes: pointer.size_bytes,
                    storage_uri: storage_uri.clone(),
                    local_path: local_path.clone(),
                    external_uri: Some(format!("file://{}", pointer.original_path.display())),
                    availability,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("log_ref_id={log_ref_id}");
            println!("session_id={session_id}");
            println!("sha256={}", pointer.sha256);
            println!("size_bytes={}", pointer.size_bytes);
            println!("storage_uri={storage_uri}");
            println!("storage_path={local_path}");
            println!(
                "availability={}",
                format_evidence_availability(availability)
            );
        }
        EvidenceCommand::FileShow { .. } => {}
    }
    Ok(())
}

/// Executes Org write subcommands against the local event queue.
pub fn handle_org(
    command: OrgCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    match command {
        OrgCommand::Create { name, description } => {
            let identity = resolve_cli_identity(store, identity_args, None, None, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let org_id = OrgId::new();
            let event = TraceEvent::org_created(
                actor,
                org_id.clone(),
                OrgCreatedPayload {
                    name,
                    description,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("org_id={org_id}");
        }
        OrgCommand::Show { .. } => {}
    }
    Ok(())
}

/// Executes Project write subcommands against the local event queue.
pub fn handle_project(
    command: ProjectCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    repo_root: &std::path::Path,
    work_dir: &std::path::Path,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    match command {
        ProjectCommand::Create {
            org,
            name,
            description,
        } => {
            let org_id = org.parse::<OrgId>().context("invalid org id")?;
            let identity = resolve_cli_identity(store, identity_args, None, None, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let project_id = ProjectId::new();
            let event = TraceEvent::project_created(
                actor,
                project_id.clone(),
                ProjectCreatedPayload {
                    org_id,
                    name,
                    description,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("project_id={project_id}");
        }
        ProjectCommand::Show { .. } => {}
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
        MissionCommand::Create {
            project,
            title,
            description,
            status,
        } => {
            let project_id = project.parse::<ProjectId>().context("invalid project id")?;
            let identity = resolve_cli_identity(store, identity_args, None, None, source_profile)?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let mission_id = MissionId::new();
            let event = TraceEvent::mission_created(
                actor,
                mission_id.clone(),
                MissionCreatedPayload {
                    project_id,
                    title,
                    description,
                    status: mission_status_from_arg(status),
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("mission_id={mission_id}");
        }
        MissionCommand::Update {
            mission,
            project,
            title,
            description,
            status,
        } => {
            if project.is_none() && title.is_none() && description.is_none() && status.is_none() {
                return Err(anyhow!(
                    "mission update requires at least one of --project, --title, --description, or --status"
                ));
            }
            let mission_id = mission.parse::<MissionId>().context("invalid mission id")?;
            let project_id = parse_optional_id::<ProjectId>(project.as_deref(), "project")?;
            let identity = resolve_cli_identity(
                store,
                identity_args,
                Some(mission_id.clone()),
                None,
                source_profile,
            )?;
            let actor = identity.actor.clone();
            let repo_context_id = append_repo_context(store, repo_root, work_dir, actor.clone())?;
            let event = TraceEvent::mission_updated(
                actor,
                mission_id.clone(),
                MissionUpdatedPayload {
                    project_id,
                    title,
                    description,
                    status: status.map(mission_status_from_arg),
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("mission_updated=true");
            println!("mission_id={mission_id}");
        }
        MissionCommand::Show { .. } => {}
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
        SessionCommand::Current
        | SessionCommand::List { .. }
        | SessionCommand::Find { .. }
        | SessionCommand::Show { .. } => {}
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
    let command = match command {
        ImportCommand::Native { command } => {
            return handle_native_import(command, identity_args, store, source_profile);
        }
        command => command,
    };
    let (source, paths, session, mission, app_session_id, app_session_name) = match command {
        ImportCommand::Cursor(args) => agent_import_parts(ImportSource::Cursor, args),
        ImportCommand::Codex(args) => agent_import_parts(ImportSource::Codex, args),
        ImportCommand::ClaudeCode(args) => agent_import_parts(ImportSource::ClaudeCode, args),
        ImportCommand::Ci(args) => ci_import_parts(args),
        ImportCommand::Native { .. } => unreachable!("native import handled before file import"),
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

fn handle_native_import(
    command: NativeImportCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    let profile = source_profile.ok_or_else(|| {
        anyhow!("native import requires a selected source profile; pass --source <name>")
    })?;
    match command {
        NativeImportCommand::List(args) => {
            let sessions = list_native_source_sessions(profile, Some(args.limit))?;
            println!("native_session_count={}", sessions.len());
            for session in sessions {
                print_native_source_session(&session);
            }
            Ok(())
        }
        NativeImportCommand::Ingest(args) => {
            let session = list_native_source_sessions(profile, None)?
                .into_iter()
                .find(|session| session.external_session_id == args.external_session_id)
                .ok_or_else(|| {
                    anyhow!(
                        "native session not found for external_session_id={}",
                        args.external_session_id
                    )
                })?;
            let requested_session_id =
                parse_optional_id::<SessionId>(args.session.as_deref(), "session")?;
            let native_session_id = requested_session_id.unwrap_or_default();
            let mission_id = parse_optional_id::<MissionId>(args.mission.as_deref(), "mission")?;
            let identity = resolve_cli_identity(
                store,
                identity_args,
                mission_id,
                Some(native_session_id),
                Some(profile),
            )?;
            let pointer = store.attachment_store().inspect_file(&session.path)?;
            let brick_session_id = identity.session_id.clone();
            let started = TraceEvent::session_started(
                identity.actor.clone(),
                brick_session_id.clone(),
                identity.mission_id.clone(),
                SessionStartedPayload {
                    session_name: session.title.clone(),
                    source: brick_protocol::SessionSource {
                        app_id: Some(session.source_app_id.clone()),
                        app_session_id: Some(session.external_session_id.clone()),
                        app_session_name: session.title.clone(),
                        runtime_id: None,
                    },
                    repo_context_id: None,
                },
            )
            .context("failed to build native imported session.started event")?;
            let log = TraceEvent::session_log_uploaded(
                identity.actor,
                identity.session_id,
                SessionLogUploadedPayload {
                    log_ref_id: LogRefId::new(),
                    original_path: pointer.original_path.display().to_string(),
                    format: infer_native_log_format(&session.path),
                    source: session.source_app_id,
                    sha256: pointer.sha256,
                    size_bytes: pointer.size_bytes,
                    storage_uri: format!("file://{}", session.path.display()),
                    local_path: String::new(),
                    external_uri: Some(format!("file://{}", session.path.display())),
                    availability: EvidenceAvailability::LocalPointer,
                    repo_context_id: None,
                },
            )
            .context("failed to build native imported session.log_uploaded event")?;
            store.append_event(&started)?;
            store.append_event(&log)?;
            println!("imported_event_count=2");
            println!("session_id={brick_session_id}");
            println!("external_session_id={}", session.external_session_id);
            println!("path={}", session.path.display());
            Ok(())
        }
    }
}

fn print_native_source_session(session: &NativeSourceSession) {
    println!(
        "native_session={} app_id={} size_bytes={} path={}",
        session.external_session_id,
        session.source_app_id,
        session.size_bytes,
        session.path.display()
    );
}

fn infer_native_log_format(path: &std::path::Path) -> SessionLogFormat {
    match path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("jsonl") => SessionLogFormat::Jsonl,
        Some("json") => SessionLogFormat::Unknown,
        Some("txt" | "log") => SessionLogFormat::Text,
        Some("md" | "markdown") => SessionLogFormat::Markdown,
        _ => SessionLogFormat::Unknown,
    }
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
        ArtifactCommand::Create {
            mission,
            session,
            kind,
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
                    artifact_kind: artifact_kind_from_arg(kind),
                    title,
                    body,
                    repo_context_id: Some(repo_context_id),
                },
            )?;
            store.append_event(&event)?;
            println!("artifact_id={artifact_id}");
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
        ArtifactCommand::Show { .. } => {}
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

fn format_evidence_availability(availability: EvidenceAvailability) -> &'static str {
    match availability {
        EvidenceAvailability::LocalPointer => "local_pointer",
        EvidenceAvailability::LocalBlob => "local_blob",
        EvidenceAvailability::RemoteBlob => "remote_blob",
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

fn mission_status_from_arg(status: MissionStatusArg) -> MissionStatus {
    match status {
        MissionStatusArg::Planned => MissionStatus::Planned,
        MissionStatusArg::Active => MissionStatus::Active,
        MissionStatusArg::Blocked => MissionStatus::Blocked,
        MissionStatusArg::Completed => MissionStatus::Completed,
        MissionStatusArg::Archived => MissionStatus::Archived,
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
