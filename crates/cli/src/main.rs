//! Entry point for the standalone `brick` CLI.
//!
//! The binary keeps parsing and dispatch thin; command construction, local
//! inspection, and presentation live in focused modules so agent-facing behavior
//! is easier to evolve without growing a monolithic main file.

use anyhow::{Context, Result};
use brick_core::{discover_repo_root, LocalStore, SourceProfileStore, StorageOptions};
use clap::Parser;

mod args;
mod commands;
mod context;
mod db;
mod inspect;
mod output;
mod source;
mod sync;

use args::{Cli, Command, SessionCommand};
use commands::{handle_artifact, handle_diff, handle_import, handle_mission, handle_session};
use context::{handle_context, handle_session_read};
use db::handle_db;
use inspect::{handle_index, handle_inspect};
use output::{print_log, print_status};
use source::handle_source;
use sync::{handle_pull, handle_push, handle_sync};

fn main() -> Result<()> {
    let cli = Cli::parse();
    let work_dir = std::env::current_dir().context("failed to read current directory")?;
    let repo_root = discover_repo_root(&work_dir)?;
    let source_profiles = SourceProfileStore::new(repo_root.clone());
    let mut command = cli.command;
    let upload_log_uses_global_source = matches!(
        command,
        Command::Session {
            command: SessionCommand::UploadLog { .. }
        }
    );
    if let Command::Session {
        command: SessionCommand::UploadLog { source, .. },
    } = &mut command
    {
        if source.is_none() {
            *source = cli.source.clone();
        }
    }
    let selected_source_profile = match &command {
        Command::Source { .. } => None,
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
            println!("Initialized Brick at {}", store.provenance_dir().display());
        }
        Command::Diff { command } => handle_diff(
            command,
            &cli.identity,
            &store,
            &repo_root,
            &work_dir,
            selected_source_profile.as_ref(),
        )?,
        Command::Mission { command } => handle_mission(
            command,
            &cli.identity,
            &store,
            &repo_root,
            &work_dir,
            selected_source_profile.as_ref(),
        )?,
        Command::Session { command } => {
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
        Command::Artifact { command } => handle_artifact(
            command,
            &cli.identity,
            &store,
            &repo_root,
            &work_dir,
            selected_source_profile.as_ref(),
        )?,
        Command::Status => print_status(&store, &repo_root, &work_dir)?,
        Command::Context { command } => handle_context(
            command,
            &cli.identity,
            &store,
            selected_source_profile.as_ref(),
        )?,
        Command::Log { limit } => print_log(&store, limit)?,
        Command::Index { command } => handle_index(command, &store)?,
        Command::Db { command } => handle_db(command, &store)?,
        Command::Inspect { command } => handle_inspect(command, &store)?,
        Command::Source { command } => handle_source(command, &source_profiles)?,
        Command::Import { command } => handle_import(
            command,
            &cli.identity,
            &store,
            selected_source_profile.as_ref(),
        )?,
        Command::Sync {
            dry_run,
            remote,
            repo_id,
        } => handle_sync(&store, dry_run, remote, repo_id)?,
        Command::Push {
            dry_run,
            remote,
            repo_id,
        } => handle_push(&store, dry_run, remote, repo_id)?,
        Command::Pull {
            dry_run,
            remote,
            repo_id,
        } => handle_pull(&store, dry_run, remote, repo_id)?,
    }

    Ok(())
}
