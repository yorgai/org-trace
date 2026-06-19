//! Entry point for the standalone `brick` CLI.
//!
//! The binary keeps parsing and dispatch thin; command construction, local
//! inspection, and presentation live in focused modules so agent-facing behavior
//! is easier to evolve without growing a monolithic main file.

use std::io::IsTerminal;
use std::path::PathBuf;

use anyhow::{Context, Result};
use brick_core::{
    discover_repo_root, discover_sources, DiscoveredPathKind, DiscoveredSource, LocalStore,
    SourceProfile, SourceProfileStore, StorageOptions,
};
use clap::Parser;
use dialoguer::{Confirm, Input, MultiSelect};

mod agent;
mod args;
mod commands;
mod context;
mod db;
mod history;
mod inspect;
mod memory;
mod output;
mod source;
mod sync;

use agent::handle_agent;
use args::{Cli, Command, EvidenceCommand, MaintenanceCommand, SessionCommand, SyncCommand};
use commands::{
    handle_artifact, handle_evidence, handle_import, handle_mission, handle_org, handle_project,
    handle_session,
};
use context::{handle_context, handle_session_read};
use db::handle_db;
use history::handle_history;
use inspect::{
    handle_index, show_artifact, show_file, show_mission, show_org, show_project, show_session,
};
use memory::handle_memory;
use output::{print_log, print_status};
use source::handle_source;
use sync::{handle_pull, handle_push, handle_sync};

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Version { format } = &cli.command {
        history::print_version(*format)?;
        return Ok(());
    }
    let work_dir = std::env::current_dir().context("failed to read current directory")?;
    let repo_root = discover_repo_root(&work_dir)?;
    let source_profiles = SourceProfileStore::new(repo_root.clone());
    let brick_config = source_profiles.read_config()?;
    let mut command = cli.command;
    let upload_log_uses_global_source = matches!(
        command,
        Command::Evidence {
            command: EvidenceCommand::Log { .. }
        }
    );
    if let Command::Evidence {
        command: EvidenceCommand::Log { source, .. },
    } = &mut command
    {
        if source.is_none() {
            *source = cli.source.clone();
        }
    }
    let selected_source_profile = match &command {
        Command::Source { .. }
        | Command::History { .. }
        | Command::Memory { .. }
        | Command::Agent { .. } => None,
        _ if upload_log_uses_global_source => source_profiles.selected_profile(None)?,
        _ => source_profiles.selected_profile(cli.source.as_deref())?,
    };
    let store = LocalStore::with_options(
        repo_root.clone(),
        StorageOptions::new()
            .with_explicit_store_root(cli.store_root.clone())
            .with_source_profile(selected_source_profile.clone()),
    )?;

    match command {
        Command::Init => {
            store.init()?;
            source_profiles.write_config(&brick_config)?;
            println!("Initialized Brick at {}", store.provenance_dir().display());
            init_source_discovery(&source_profiles)?;
            agent::init_prompt(&work_dir)?;
        }
        Command::Version { .. } => unreachable!("version handled before repo discovery"),
        Command::Org { command } => match command {
            args::OrgCommand::Show { org } => show_org(org, &store)?,
            command => handle_org(
                command,
                &cli.identity,
                &store,
                &repo_root,
                &work_dir,
                selected_source_profile.as_ref(),
            )?,
        },
        Command::Project { command } => match command {
            args::ProjectCommand::Show { project } => show_project(project, &store)?,
            command => handle_project(
                command,
                &cli.identity,
                &store,
                &repo_root,
                &work_dir,
                selected_source_profile.as_ref(),
            )?,
        },
        Command::Mission { command } => match command {
            args::MissionCommand::Show { mission } => show_mission(mission, &store)?,
            command => handle_mission(
                command,
                &cli.identity,
                &store,
                &repo_root,
                &work_dir,
                selected_source_profile.as_ref(),
            )?,
        },
        Command::Session { command } => match command {
            SessionCommand::Show { session } => show_session(session, &store)?,
            command => {
                if !handle_session_read(
                    &command,
                    &cli.identity,
                    &store,
                    selected_source_profile.as_ref(),
                )? {
                    handle_session(
                        command,
                        &cli.identity,
                        &store,
                        &repo_root,
                        &work_dir,
                        selected_source_profile.as_ref(),
                    )?;
                }
            }
        },
        Command::Artifact { command } => match command {
            args::ArtifactCommand::Show { artifact } => show_artifact(artifact, &store)?,
            command => handle_artifact(
                command,
                &cli.identity,
                &store,
                &repo_root,
                &work_dir,
                selected_source_profile.as_ref(),
            )?,
        },
        Command::Evidence { command } => match command {
            EvidenceCommand::FileShow { path } => show_file(path, &store)?,
            command => handle_evidence(
                command,
                &cli.identity,
                &store,
                &repo_root,
                &work_dir,
                selected_source_profile.as_ref(),
                &brick_config,
            )?,
        },
        Command::Context { command } => handle_context(
            command,
            &cli.identity,
            &store,
            selected_source_profile.as_ref(),
        )?,
        Command::Agent { command } => handle_agent(command)?,
        Command::Source { command } => handle_source(command, &source_profiles)?,
        Command::Import { command } => handle_import(
            command,
            &cli.identity,
            &store,
            selected_source_profile.as_ref(),
        )?,
        Command::History { command } => handle_history(command, &source_profiles, &store)?,
        Command::Memory { command } => handle_memory(command, &source_profiles, &store)?,
        Command::Sync { command } => match command {
            SyncCommand::Run(args) => handle_sync(&store, args.dry_run, args.remote, args.repo_id)?,
            SyncCommand::Push(args) => {
                handle_push(&store, args.dry_run, args.remote, args.repo_id)?
            }
            SyncCommand::Pull(args) => {
                handle_pull(&store, args.dry_run, args.remote, args.repo_id)?
            }
        },
        Command::Maintenance { command } => match command {
            MaintenanceCommand::Status => print_status(&store, &repo_root, &work_dir)?,
            MaintenanceCommand::Log { limit } => print_log(&store, limit)?,
            MaintenanceCommand::Index { command } => handle_index(command, &store)?,
            MaintenanceCommand::Db { command } => handle_db(command, &store)?,
        },
    }

    Ok(())
}

fn init_source_discovery(source_profiles: &SourceProfileStore) -> Result<()> {
    let discovered = discover_sources();
    if discovered.is_empty() {
        println!("source_scan_found=0");
        return Ok(());
    }

    println!("source_scan_found={}", discovered.len());
    for source in &discovered {
        source::print_discovered_source(source);
    }

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("Run `brick source scan --write-defaults` to save discovered source profiles.");
        return Ok(());
    }

    if !Confirm::new()
        .with_prompt("Save discovered source profiles now?")
        .default(true)
        .interact()?
    {
        return Ok(());
    }

    let labels = discovered
        .iter()
        .map(source_selection_label)
        .collect::<Vec<_>>();
    let selected = MultiSelect::new()
        .with_prompt("Select sources to include")
        .items(&labels)
        .defaults(&vec![true; labels.len()])
        .interact()?;

    for index in selected {
        let source = &discovered[index];
        let mut profile = source::profile_from_discovered_source(source);
        if Confirm::new()
            .with_prompt(format!("Override paths for {}?", source.source.label()))
            .default(false)
            .interact()?
        {
            profile = prompt_profile_overrides(source, profile)?;
        }
        source_profiles.write_profile(&profile)?;
        println!("source_configured={}", profile.name);
    }

    if Confirm::new()
        .with_prompt("Add another custom source profile?")
        .default(false)
        .interact()?
    {
        let profile = prompt_custom_profile()?;
        source_profiles.write_profile(&profile)?;
        println!("source_configured={}", profile.name);
    }
    Ok(())
}

fn source_selection_label(source: &DiscoveredSource) -> String {
    let paths = source
        .paths
        .iter()
        .map(|path| format!("{}={}", path.kind.label(), path.path.display()))
        .collect::<Vec<_>>()
        .join(", ");
    format!("{} ({})", source.source.label(), paths)
}

fn prompt_profile_overrides(
    source: &DiscoveredSource,
    mut profile: SourceProfile,
) -> Result<SourceProfile> {
    profile.evidence_root = prompt_optional_path(
        "Evidence root",
        default_path_for_kind(source, DiscoveredPathKind::EvidenceRoot)
            .or_else(|| default_path_for_kind(source, DiscoveredPathKind::SessionLogRoot))
            .or(profile.evidence_root),
    )?;
    profile.session_db_path = prompt_optional_path(
        "Session DB path",
        default_path_for_kind(source, DiscoveredPathKind::SessionDatabase)
            .or_else(|| default_path_for_kind(source, DiscoveredPathKind::HistoryDatabase))
            .or(profile.session_db_path),
    )?;
    profile.session_log_path = prompt_optional_path(
        "Session log path",
        default_path_for_kind(source, DiscoveredPathKind::SessionLogRoot)
            .or(profile.session_log_path),
    )?;
    profile.cursor_state_db_path = prompt_optional_path(
        "Cursor state DB path",
        default_path_for_kind(source, DiscoveredPathKind::CursorStateDatabase)
            .or(profile.cursor_state_db_path),
    )?;
    Ok(profile)
}

fn prompt_custom_profile() -> Result<SourceProfile> {
    let name: String = Input::new().with_prompt("Profile name").interact_text()?;
    let app_id: String = Input::new()
        .with_prompt("App id")
        .default(name.clone())
        .interact_text()?;
    Ok(SourceProfile {
        name,
        app_id: Some(app_id),
        actor_id: None,
        actor_type: None,
        store_root: None,
        session_db_path: prompt_optional_path("Session DB path", None)?,
        session_log_path: prompt_optional_path("Session log path", None)?,
        evidence_root: prompt_optional_path("Evidence root", None)?,
        cursor_state_db_path: prompt_optional_path("Cursor state DB path", None)?,
        default_full_evidence_upload: None,
        notes: Some("Added during brick init".to_string()),
    })
}

fn prompt_optional_path(prompt: &str, default: Option<PathBuf>) -> Result<Option<PathBuf>> {
    let default_text = default
        .as_ref()
        .map(|path| path.display().to_string())
        .unwrap_or_default();
    let value: String = Input::new()
        .with_prompt(format!("{prompt} (empty to skip)"))
        .default(default_text)
        .allow_empty(true)
        .interact_text()?;
    if value.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(PathBuf::from(value)))
    }
}

fn default_path_for_kind(source: &DiscoveredSource, kind: DiscoveredPathKind) -> Option<PathBuf> {
    source
        .paths
        .iter()
        .find(|path| path.kind == kind)
        .map(|path| path.path.clone())
}
