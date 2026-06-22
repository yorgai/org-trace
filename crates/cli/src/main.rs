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
mod announce;
mod args;
#[cfg(feature = "sync")]
mod auth;
mod claude_hook;
mod commands;
mod context;
mod db;
mod defaults;
mod history;
mod inspect;
mod mcp;
mod mcp_config;
mod metadata;
mod output;
mod source;

use agent::handle_agent;
use announce::handle_announce;
use args::{Cli, Command, EvidenceCommand, MaintenanceCommand, SessionCommand};
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
use metadata::handle_metadata;
use output::{print_log, print_status};
use source::handle_source;

#[cfg(feature = "sync")]
use args::SyncCommand;
#[cfg(feature = "sync")]
use brick_sync::{handle_pull, handle_push, handle_sync};

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Version { format } = &cli.command {
        history::print_version(*format)?;
        return Ok(());
    }
    // Account commands don't need a repo, so handle them before repo discovery
    // (which would fail outside a git working tree).
    #[cfg(feature = "sync")]
    match &cli.command {
        Command::Login { email } => return auth::handle_login(email.clone()),
        Command::Logout => return auth::handle_logout(),
        Command::Whoami => return auth::handle_whoami(),
        _ => {}
    }
    let work_dir = std::env::current_dir().context("failed to read current directory")?;
    // `mcp-serve` is a long-lived server an MCP client (ORGII, Claude, …) may
    // launch from any working directory, including one outside a git repo. It
    // must not hard-fail on startup: the tools that actually need a repo (e.g.
    // blame) resolve it lazily per call and error gracefully there. So fall back
    // to the working dir as the root rather than aborting initialization.
    let repo_root = match discover_repo_root(&work_dir) {
        Ok(root) => root,
        Err(_) if matches!(cli.command, Command::McpServe { .. }) => work_dir.clone(),
        Err(err) => return Err(err),
    };
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
        | Command::Metadata { .. }
        | Command::McpServe { .. }
        | Command::Announce { .. }
        | Command::Blame { .. }
        | Command::LogLine { .. }
        | Command::Sessions { .. }
        | Command::Claims { .. }
        | Command::Claim { .. }
        | Command::Log { .. }
        | Command::Search { .. }
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
            args::MissionCommand::List {
                status,
                project,
                limit,
            } => inspect::list_missions(
                status.map(commands::mission_status_from_arg),
                project,
                limit,
                &store,
            )?,
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
            args::ArtifactCommand::Attach {
                artifact,
                session,
                path,
                name,
                content_type,
                copy,
            } => handle_evidence(
                args::EvidenceCommand::Attach {
                    artifact,
                    session,
                    path,
                    name,
                    content_type,
                    copy,
                },
                &cli.identity,
                &store,
                &repo_root,
                &work_dir,
                selected_source_profile.as_ref(),
                &brick_config,
            )?,
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
        Command::Status => handle_context(
            args::ContextCommand::Show,
            &cli.identity,
            &store,
            selected_source_profile.as_ref(),
        )?,
        Command::Sessions {
            source,
            limit,
            window_secs,
            format,
        } => handle_history(
            args::HistoryCommand::Live {
                source,
                limit,
                window_secs,
                format,
            },
            &source_profiles,
            &store,
        )?,
        Command::Claims { path, format } => handle_announce(
            args::AnnounceCommand::List { path, format },
            cli.source.as_deref(),
            cli.identity.session.as_deref(),
        )?,
        Command::Claim {
            scope,
            message,
            release,
            source,
            session,
            work_dir,
            ttl_minutes,
            format,
        } => {
            let command = if release {
                args::AnnounceCommand::Release {
                    scope: Some(scope),
                    source,
                    session,
                    format,
                }
            } else {
                let message = message.context(
                    "`brick claim` requires --message unless --release is set",
                )?;
                args::AnnounceCommand::Claim {
                    scope,
                    message,
                    source,
                    session,
                    work_dir,
                    ttl_minutes,
                    format,
                }
            };
            handle_announce(
                command,
                cli.source.as_deref(),
                cli.identity.session.as_deref(),
            )?
        }
        Command::Log { command } => match command {
            args::LogCommand::File {
                path,
                source,
                limit,
                format,
            } => handle_metadata(
                args::MetadataCommand::Recall {
                    path,
                    source,
                    limit,
                    format,
                },
                &source_profiles,
                &store,
            )?,
        },
        Command::Show { command } => match command {
            args::ShowCommand::Mission { mission } => show_mission(mission, &store)?,
            args::ShowCommand::Session { session } => show_session(session, &store)?,
        },
        Command::Search {
            query,
            source,
            limit,
            format,
        } => handle_metadata(
            args::MetadataCommand::Query {
                query,
                source,
                limit,
                format,
            },
            &source_profiles,
            &store,
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
        Command::Metadata { command } => handle_metadata(command, &source_profiles, &store)?,
        Command::McpServe { planning } => mcp::serve(&source_profiles, &store, planning)?,
        Command::Explain {
            anchor,
            depth,
            format,
        } => handle_explain(&store, &anchor, depth, format)?,
        Command::Blame {
            path,
            line_start,
            line_end,
            format,
        } => handle_blame(&store, &path, line_start, line_end, format)?,
        Command::LogLine {
            path,
            line_start,
            line_end,
            format,
        } => handle_log_line(&store, &path, line_start, line_end, format)?,
        Command::Announce { command } => handle_announce(
            command,
            cli.source.as_deref(),
            cli.identity.session.as_deref(),
        )?,
        #[cfg(feature = "sync")]
        Command::Sync { command } => match command {
            SyncCommand::Run(args) => handle_sync(&store, args.dry_run, args.remote, args.repo_id)?,
            SyncCommand::Push(args) => {
                handle_push(&store, args.dry_run, args.remote, args.repo_id)?
            }
            SyncCommand::Pull(args) => {
                handle_pull(&store, args.dry_run, args.remote, args.repo_id)?
            }
        },
        #[cfg(feature = "sync")]
        Command::Login { .. } | Command::Logout | Command::Whoami => {
            unreachable!("account commands handled before repo discovery")
        }
        Command::Maintenance { command } => match command {
            MaintenanceCommand::Status => print_status(&store, &repo_root, &work_dir)?,
            MaintenanceCommand::Log { limit } => print_log(&store, limit)?,
            MaintenanceCommand::Index { command } => handle_index(command, &store)?,
            MaintenanceCommand::Db { command } => handle_db(command, &store)?,
        },
    }

    Ok(())
}

/// Prints line-level AI blame for `path` as JSON. Resolves the repo root from
/// the current directory, then maps each current line to its producing session.
/// Prints the causal chain for an anchor as JSON: the read side of Brick's
/// causal layer. `path:line` anchors resolve through blame (git-aware); other
/// anchors (artifact/mission/event id) resolve directly off the event stream.
/// Never gated — local explain is free; the wall is cross-machine sync.
fn handle_explain(
    store: &LocalStore,
    anchor: &str,
    depth: Option<usize>,
    format: args::HistoryFormatArg,
) -> Result<()> {
    history::ensure_json(format);
    let events = store.read_all_events()?;
    let index = store.load_or_rebuild_index()?;
    let depth = depth.unwrap_or(brick_core::DEFAULT_EXPLAIN_DEPTH);

    let (resolved, anchored_path, is_file_line) = if let Some((rel_path, line)) =
        parse_anchor_file_line(anchor)
    {
        let cwd = std::env::current_dir()?;
        let repo_root = discover_repo_root(&cwd)?;
        let rel = match std::path::Path::new(&rel_path).strip_prefix(&repo_root) {
            Ok(stripped) => stripped.to_string_lossy().into_owned(),
            Err(_) => rel_path.trim_start_matches("./").to_string(),
        };
        (
            brick_core::resolve_file_line_anchor(store, &repo_root, &rel, line)?,
            Some(rel),
            true,
        )
    } else if anchor_looks_like_path(anchor) {
        let cwd = std::env::current_dir()?;
        let rel = match discover_repo_root(&cwd) {
            Ok(repo_root) => match std::path::Path::new(anchor).strip_prefix(&repo_root) {
                Ok(stripped) => stripped.to_string_lossy().into_owned(),
                Err(_) => anchor.trim_start_matches("./").to_string(),
            },
            Err(_) => anchor.trim_start_matches("./").to_string(),
        };
        (
            brick_core::resolve_file_anchor(&events, &rel),
            Some(rel),
            false,
        )
    } else {
        (brick_core::resolve_direct_anchor(&events, anchor), None, false)
    };

    let mut chain = brick_core::explain_from_events(&index, &events, resolved, depth);
    // One db, one explain: share the metadata-db index fallback with the MCP
    // surface so `brick explain` answers identically for indexed-only history.
    mcp::merge_index_sessions_into_chain(
        &mut chain,
        store.repo_root(),
        anchored_path.as_deref(),
        is_file_line,
        depth,
    );
    history::print_json(&chain)
}

/// Parses a `path:line` anchor into `(path, line)`; `None` for bare ids.
fn parse_anchor_file_line(input: &str) -> Option<(String, u64)> {
    let (path, line) = input.rsplit_once(':')?;
    let line: u64 = line.trim().parse().ok()?;
    if path.is_empty() {
        return None;
    }
    Some((path.to_string(), line))
}

/// Heuristic mirror of the MCP layer: a whole-file anchor (path, no `:line`) vs a
/// Brick id. Lets `brick explain src/auth.rs` work without a line number.
fn anchor_looks_like_path(input: &str) -> bool {
    let s = input.trim();
    if s.is_empty()
        || s.starts_with("artifact_")
        || s.starts_with("mission_")
        || s.starts_with("session_")
        || s.starts_with("org_")
        || s.starts_with("project_")
    {
        return false;
    }
    if uuid::Uuid::parse_str(s).is_ok() {
        return false;
    }
    s.contains('/') || s.contains('.')
}

fn handle_blame(
    store: &LocalStore,
    path: &str,
    line_start: Option<usize>,
    line_end: Option<usize>,
    format: args::HistoryFormatArg,
) -> Result<()> {
    // Soft login gate: line-level blame requires a Brick account. Only enforced
    // in the proprietary `sync` build; the open-source binary has no login
    // concept and runs unguarded (registration hook, not a security boundary).
    #[cfg(feature = "sync")]
    if !brick_sync::is_logged_in() {
        anyhow::bail!("line-level blame needs a Brick account. Run `brick login` first.");
    }
    history::ensure_json(format);
    let cwd = std::env::current_dir()?;
    let repo_root = discover_repo_root(&cwd)?;
    let rel_path = match std::path::Path::new(path).strip_prefix(&repo_root) {
        Ok(stripped) => stripped.to_string_lossy().into_owned(),
        Err(_) => path.trim_start_matches("./").to_string(),
    };
    let mut lines = brick_core::blame_file(store, &repo_root, &rel_path)?;
    if let Some(start) = line_start {
        lines.retain(|line| line.line_no as usize >= start);
    }
    if let Some(end) = line_end {
        lines.retain(|line| line.line_no as usize <= end);
    }
    let attributed = lines
        .iter()
        .filter(|line| line.session_id.is_some() || line.actor_id.is_some())
        .count();
    history::print_json(&serde_json::json!({
        "path": rel_path,
        "line_count": lines.len(),
        "attributed_lines": attributed,
        "lines": lines,
    }))
}

/// Prints the full change history of a line range as JSON (like `git log -L`):
/// every commit that touched `[line_start, line_end]`, each tagged with its AI
/// session when locally attributable. Shares the soft login gate with `blame`.
fn handle_log_line(
    store: &LocalStore,
    path: &str,
    line_start: usize,
    line_end: usize,
    format: args::HistoryFormatArg,
) -> Result<()> {
    #[cfg(feature = "sync")]
    if !brick_sync::is_logged_in() {
        anyhow::bail!("line-level history needs a Brick account. Run `brick login` first.");
    }
    history::ensure_json(format);
    let cwd = std::env::current_dir()?;
    let repo_root = discover_repo_root(&cwd)?;
    let rel_path = match std::path::Path::new(path).strip_prefix(&repo_root) {
        Ok(stripped) => stripped.to_string_lossy().into_owned(),
        Err(_) => path.trim_start_matches("./").to_string(),
    };
    let touches = brick_core::blame_line_range_history(
        store,
        &repo_root,
        &rel_path,
        line_start as u64,
        line_end as u64,
    )?;
    let attributed = touches.iter().filter(|touch| touch.attributed).count();
    history::print_json(&serde_json::json!({
        "path": rel_path,
        "line_start": line_start,
        "line_end": line_end,
        "commit_count": touches.len(),
        "attributed_commits": attributed,
        "history": touches,
    }))
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
