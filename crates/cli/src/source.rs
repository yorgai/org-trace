//! CLI handlers for source profile configuration.
//!
//! Source commands read and write repo-local profile config so storage roots can
//! be selected before the effective event store exists.

use std::str::FromStr;

use anyhow::{Context, Result};
use brick_core::{SourceProfile, SourceProfileStore};
use brick_protocol::ActorType;

use crate::args::{SourceCommand, SourceConfigureArgs};

/// Executes source profile subcommands.
pub fn handle_source(command: SourceCommand, profiles: &SourceProfileStore) -> Result<()> {
    match command {
        SourceCommand::Configure(args) => configure_source(args, profiles),
        SourceCommand::List => list_sources(profiles),
        SourceCommand::Show { name } => show_source(&name, profiles),
        SourceCommand::Use { name } => use_source(&name, profiles),
    }
}

fn configure_source(args: SourceConfigureArgs, profiles: &SourceProfileStore) -> Result<()> {
    let profile = SourceProfile {
        name: args.name,
        app_id: args.app_id,
        actor_id: args.actor_id,
        actor_type: args
            .actor_type
            .as_deref()
            .map(ActorType::from_str)
            .transpose()
            .context("invalid --actor-type")?,
        store_root: args.store_root,
        session_db_path: args.session_db_path,
        session_log_path: args.session_log_path,
        notes: args.notes,
    };
    profiles.write_profile(&profile)?;
    println!("source_configured={}", profile.name);
    Ok(())
}

fn list_sources(profiles: &SourceProfileStore) -> Result<()> {
    let selected = profiles.selected_profile_name()?;
    for profile in profiles.list_profiles()? {
        let marker = if selected.as_deref() == Some(profile.name.as_str()) {
            "*"
        } else {
            ""
        };
        println!(
            "source={} selected={} app_id={} actor_id={} actor_type={} store_root={}",
            profile.name,
            marker,
            profile.app_id.as_deref().unwrap_or(""),
            profile.actor_id.as_deref().unwrap_or(""),
            profile.actor_type.map(format_actor_type).unwrap_or(""),
            profile
                .store_root
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_default()
        );
    }
    Ok(())
}

fn show_source(name: &str, profiles: &SourceProfileStore) -> Result<()> {
    let profile = profiles
        .read_profile(name)?
        .ok_or_else(|| anyhow::anyhow!("source profile not found: {name}"))?;
    print_profile(&profile);
    Ok(())
}

fn use_source(name: &str, profiles: &SourceProfileStore) -> Result<()> {
    let profile = profiles.use_profile(name)?;
    println!("source_selected={}", profile.name);
    Ok(())
}

fn print_profile(profile: &SourceProfile) {
    println!("name={}", profile.name);
    println!("app_id={}", profile.app_id.as_deref().unwrap_or(""));
    println!("actor_id={}", profile.actor_id.as_deref().unwrap_or(""));
    println!(
        "actor_type={}",
        profile.actor_type.map(format_actor_type).unwrap_or("")
    );
    println!(
        "store_root={}",
        profile
            .store_root
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    );
    println!(
        "session_db_path={}",
        profile
            .session_db_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    );
    println!(
        "session_log_path={}",
        profile
            .session_log_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    );
    println!("notes={}", profile.notes.as_deref().unwrap_or(""));
}

fn format_actor_type(actor_type: ActorType) -> &'static str {
    match actor_type {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}
