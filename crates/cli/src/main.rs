//! Entry point for the standalone `brick` CLI.

use anyhow::{Context, Result};
use brick_core::{discover_repo_root, LocalStore, SourceProfileStore, StorageOptions};
use clap::Parser;

mod agent;
mod args;
mod claude_hook;
mod history;
mod mcp;
mod mcp_config;
mod metadata;
mod native_hook;
mod skill;

use agent::handle_agent;
use args::{
    AgentFormatArg, AgentInstallArgs, AgentTargetArg, AgentTargetArgs, Cli, Command, SetupArgs,
};

#[cfg(feature = "sync")]
use args::SyncCommand;
#[cfg(feature = "sync")]
use brick_sync::{
    auto_pull_best_effort, handle_accept_invites, handle_create_org, handle_invite, handle_pull,
    handle_push, handle_sync, identity,
};

fn main() -> Result<()> {
    let cli = Cli::parse();
    if let Command::Version { format } = &cli.command {
        history::print_version(*format)?;
        return Ok(());
    }

    let work_dir = std::env::current_dir().context("failed to read current directory")?;
    let repo_root = match discover_repo_root(&work_dir) {
        Ok(root) => root,
        Err(_) if matches!(cli.command, Command::McpServe { .. } | Command::Setup(_)) => {
            work_dir.clone()
        }
        Err(err) => return Err(err),
    };
    let source_profiles = SourceProfileStore::new(repo_root.clone());
    let selected_source_profile = match &cli.command {
        Command::McpServe { .. }
        | Command::Agent { .. }
        | Command::Setup(_)
        | Command::HookExplain => None,
        _ => source_profiles.selected_profile(cli.source.as_deref())?,
    };
    let store = LocalStore::with_options(
        repo_root.clone(),
        StorageOptions::new()
            .with_explicit_store_root(cli.store_root.clone())
            .with_source_profile(selected_source_profile),
    )?;

    match cli.command {
        Command::Version { .. } => unreachable!("version handled before repo discovery"),
        Command::Agent { command } => handle_agent(command)?,
        Command::Setup(args) => handle_setup(args)?,
        Command::McpServe { planning } => mcp::serve(&source_profiles, &store, planning)?,
        Command::Explain {
            anchor,
            depth,
            format,
        } => handle_explain(&store, &anchor, depth, format)?,
        Command::HookExplain => metadata::run_explain_hook(&store)?,
        #[cfg(feature = "sync")]
        Command::Sync { command } => match command {
            SyncCommand::Run(args) => handle_sync(
                &store,
                args.dry_run,
                args.remote,
                args.repo_id,
                args.org_id,
                args.full,
                args.all_repos,
            )?,
            SyncCommand::Push(args) => handle_push(
                &store,
                args.dry_run,
                args.remote,
                args.repo_id,
                args.org_id,
                args.full,
                args.all_repos,
            )?,
            SyncCommand::Pull(args) => {
                handle_pull(&store, args.dry_run, args.remote, args.repo_id)?
            }
            SyncCommand::Login(args) => {
                handle_sync_login(args.email, args.code, args.callback_url)?
            }
            SyncCommand::Logout => handle_sync_logout()?,
            SyncCommand::Whoami => handle_sync_whoami()?,
            SyncCommand::CreateOrg(args) => handle_create_org(args.org_id)?,
            SyncCommand::Invite(args) => handle_invite(args.org_id, args.email)?,
            SyncCommand::AcceptInvites => handle_accept_invites()?,
        },
    }

    Ok(())
}

fn handle_setup(args: SetupArgs) -> Result<()> {
    if args.agents {
        agent::install(AgentInstallArgs {
            target: AgentTargetArgs {
                global: true,
                target: AgentTargetArg::All,
                dir: None,
                format: AgentFormatArg::Text,
            },
            force: false,
            print: false,
        })?;
    }

    #[cfg(feature = "sync")]
    {
        match (args.email, args.code) {
            (Some(email), Some(code)) => {
                let identity = identity::verify_email_otp(&email, &code)?;
                println!("share_enabled=true");
                println!("logged_in=true");
                println!("user_id={}", identity.user_id);
                println!("email={}", identity.email.as_deref().unwrap_or(""));
            }
            (Some(email), None) => {
                identity::request_email_otp(&email)?;
                println!("share_enabled=pending");
                println!("otp_sent=true");
                println!("email={email}");
                println!("run `brick setup --email {email} --code <code>` to enable sharing");
            }
            (None, Some(_)) => {
                anyhow::bail!("--code requires --email");
            }
            (None, None) => {
                println!("share_enabled=false");
                println!("Brick is ready for local-only use.");
                println!("Run `brick setup --email <you@example.com>` later to enable sharing.");
            }
        }
    }

    #[cfg(not(feature = "sync"))]
    {
        println!("share_enabled=false");
        println!("Brick is ready for local-only use.");
        println!("This binary was built without the sync feature, so Supabase sharing login is unavailable.");
    }

    Ok(())
}

#[cfg(feature = "sync")]
fn handle_sync_login(
    email: Option<String>,
    code: Option<String>,
    callback_url: Option<String>,
) -> Result<()> {
    if let Some(callback_url) = callback_url {
        if email.is_some() || code.is_some() {
            anyhow::bail!("--callback-url cannot be combined with --email or --code");
        }
        let identity = identity::save_magic_link_callback(&callback_url)?;
        println!("logged_in=true");
        println!("user_id={}", identity.user_id);
        println!("email={}", identity.email.as_deref().unwrap_or(""));
        return Ok(());
    }

    let email = email.context("sync login requires --email or --callback-url")?;
    match code {
        Some(code) => {
            let identity = identity::verify_email_otp(&email, &code)?;
            println!("logged_in=true");
            println!("user_id={}", identity.user_id);
            println!("email={}", identity.email.as_deref().unwrap_or(""));
        }
        None => {
            identity::request_email_otp(&email)?;
            println!("otp_sent=true");
            println!("email={email}");
            println!("run `brick sync login --email {email} --code <code>` to finish");
        }
    }
    Ok(())
}

#[cfg(feature = "sync")]
fn handle_sync_logout() -> Result<()> {
    let removed = identity::clear()?;
    println!("logged_out={removed}");
    Ok(())
}

#[cfg(feature = "sync")]
fn handle_sync_whoami() -> Result<()> {
    match identity::refresh_if_needed() {
        Ok(identity) => {
            println!("logged_in=true");
            println!("user_id={}", identity.user_id);
            println!("email={}", identity.email.as_deref().unwrap_or(""));
            println!(
                "expires_at={}",
                identity
                    .expires_at
                    .map(|expiry| expiry.to_rfc3339())
                    .unwrap_or_default()
            );
        }
        Err(_) => println!("logged_in=false"),
    }
    Ok(())
}

fn handle_explain(
    store: &LocalStore,
    anchor: &str,
    depth: Option<usize>,
    format: args::HistoryFormatArg,
) -> Result<()> {
    #[cfg(feature = "sync")]
    auto_pull_best_effort(store);
    history::ensure_json(format);
    history::refresh_repo_sources_best_effort(store);
    let events = store.read_all_events()?;
    let depth = depth.unwrap_or(brick_core::DEFAULT_EXPLAIN_DEPTH);

    let (resolved, anchored_path, is_file_line) =
        if let Some((rel_path, start, end)) = parse_anchor_file_range(anchor) {
            let cwd = std::env::current_dir()?;
            let repo_root = discover_repo_root(&cwd)?;
            let rel = match std::path::Path::new(&rel_path).strip_prefix(&repo_root) {
                Ok(stripped) => stripped.to_string_lossy().into_owned(),
                Err(_) => rel_path.trim_start_matches("./").to_string(),
            };
            (
                brick_core::resolve_file_range_anchor(store, &repo_root, &rel, start, end)?,
                Some(rel),
                true,
            )
        } else if let Some((rel_path, line)) = parse_anchor_file_line(anchor) {
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
            (
                brick_core::resolve_direct_anchor(&events, anchor),
                None,
                false,
            )
        };

    let mut chain = brick_core::explain_from_events(&events, resolved, depth);
    let index_session_hint = mcp::merge_index_sessions_into_chain(
        &mut chain,
        store.repo_root(),
        anchored_path.as_deref(),
        is_file_line,
        depth,
    );
    let value = mcp::finalize_explain_chain(
        chain,
        store,
        None,
        anchored_path.as_deref(),
        index_session_hint,
    )?;
    history::print_json(&value)
}

fn parse_anchor_file_line(input: &str) -> Option<(String, u64)> {
    let (path, line) = input.rsplit_once(':')?;
    let line: u64 = line.trim().parse().ok()?;
    if path.is_empty() {
        return None;
    }
    Some((path.to_string(), line))
}

fn parse_anchor_file_range(input: &str) -> Option<(String, u64, u64)> {
    let (path, span) = input.rsplit_once(':')?;
    if path.is_empty() {
        return None;
    }
    let (start, end) = span.trim().split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    Some((path.to_string(), start, end))
}

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
