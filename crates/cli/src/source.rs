//! CLI handlers for source profile configuration.
//!
//! Source commands read and write repo-local profile config so storage roots can
//! be selected before the effective event store exists.

use std::str::FromStr;

use anyhow::{Context, Result};
use brick_core::{
    discover_sources, BrickConfig, DiscoveredPathKind, DiscoveredSource, EvidenceConfig,
    SourceProfile, SourceProfileStore,
};
use brick_protocol::ActorType;

use crate::args::{SourceCommand, SourceConfigArgs, SourceConfigureArgs, SourceScanArgs};

/// Executes source profile subcommands.
pub fn handle_source(command: SourceCommand, profiles: &SourceProfileStore) -> Result<()> {
    match command {
        SourceCommand::Configure(args) => configure_source(args, profiles),
        SourceCommand::Config(args) => configure_repo(args, profiles),
        SourceCommand::Scan(args) => scan_sources(args, profiles),
        SourceCommand::List => list_sources(profiles),
        SourceCommand::Show { name } => show_source(&name, profiles),
        SourceCommand::Use { name } => use_source(&name, profiles),
    }
}

fn scan_sources(args: SourceScanArgs, profiles: &SourceProfileStore) -> Result<()> {
    let discovered = discover_sources();
    if discovered.is_empty() {
        println!("source_scan_found=0");
        return Ok(());
    }

    println!("source_scan_found={}", discovered.len());
    for source in &discovered {
        print_discovered_source(source);
        if args.write_defaults {
            profiles.write_profile(&profile_from_discovered_source(source))?;
        }
    }
    if args.write_defaults {
        println!("source_defaults_written={}", discovered.len());
    }
    Ok(())
}

fn configure_repo(args: SourceConfigArgs, profiles: &SourceProfileStore) -> Result<()> {
    let existing = profiles.read_config()?;
    let config = BrickConfig {
        evidence: EvidenceConfig {
            default_full_evidence_upload: args
                .default_full_evidence_upload
                .unwrap_or(existing.evidence.default_full_evidence_upload),
            metadata_only_local: args
                .metadata_only_local
                .unwrap_or(existing.evidence.metadata_only_local),
        },
    };
    profiles.write_config(&config)?;
    println!(
        "default_full_evidence_upload={}",
        config.evidence.default_full_evidence_upload
    );
    println!(
        "metadata_only_local={}",
        config.evidence.metadata_only_local
    );
    Ok(())
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
        evidence_root: args.evidence_root,
        cursor_state_db_path: args.cursor_state_db_path,
        default_full_evidence_upload: args.default_full_evidence_upload,
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
    println!(
        "evidence_root={}",
        profile
            .evidence_root
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    );
    println!(
        "cursor_state_db_path={}",
        profile
            .cursor_state_db_path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default()
    );
    println!(
        "default_full_evidence_upload={}",
        profile
            .default_full_evidence_upload
            .map(|enabled| enabled.to_string())
            .unwrap_or_default()
    );
    println!("notes={}", profile.notes.as_deref().unwrap_or(""));
}

pub fn profile_from_discovered_source(source: &DiscoveredSource) -> SourceProfile {
    let evidence_root = source
        .paths
        .iter()
        .find(|path| {
            matches!(
                path.kind,
                DiscoveredPathKind::EvidenceRoot | DiscoveredPathKind::SessionLogRoot
            )
        })
        .map(|path| path.path.clone());
    let cursor_state_db_path = source
        .paths
        .iter()
        .find(|path| path.kind == DiscoveredPathKind::CursorStateDatabase)
        .map(|path| path.path.clone());
    let session_log_path = source
        .paths
        .iter()
        .find(|path| path.kind == DiscoveredPathKind::SessionLogRoot)
        .map(|path| path.path.clone());
    let session_db_path = source
        .paths
        .iter()
        .find(|path| {
            matches!(
                path.kind,
                DiscoveredPathKind::SessionDatabase | DiscoveredPathKind::HistoryDatabase
            )
        })
        .map(|path| path.path.clone());

    SourceProfile {
        name: source.source.profile_name().to_string(),
        app_id: Some(source.source.app_id().to_string()),
        actor_id: None,
        actor_type: None,
        store_root: None,
        session_db_path,
        session_log_path,
        evidence_root,
        cursor_state_db_path,
        default_full_evidence_upload: None,
        notes: Some("Discovered by brick source scan".to_string()),
    }
}

pub fn print_discovered_source(source: &DiscoveredSource) {
    println!(
        "source={} label={}",
        source.source.profile_name(),
        source.source.label()
    );
    for discovered_path in &source.paths {
        println!(
            "  {}={}",
            discovered_path.kind.label(),
            discovered_path.path.display()
        );
    }
}

fn format_actor_type(actor_type: ActorType) -> &'static str {
    match actor_type {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}
