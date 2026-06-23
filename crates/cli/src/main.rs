//! Entry point for the standalone `brick` CLI.

use anyhow::{Context, Result};
use brick_core::{discover_repo_root, LocalStore, SourceProfileStore, StorageOptions};
use clap::Parser;
use serde_json::{json, Value};

mod agent;
mod args;
mod claude_hook;
mod defaults;
mod history;
mod mcp;
mod mcp_config;
mod metadata;
mod native_hook;
mod skill;

use agent::handle_agent;
use args::{Cli, Command};

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

    let work_dir = std::env::current_dir().context("failed to read current directory")?;
    let repo_root = match discover_repo_root(&work_dir) {
        Ok(root) => root,
        Err(_) if matches!(cli.command, Command::McpServe { .. }) => work_dir.clone(),
        Err(err) => return Err(err),
    };
    let source_profiles = SourceProfileStore::new(repo_root.clone());
    let selected_source_profile = match &cli.command {
        Command::McpServe { .. } | Command::Agent { .. } | Command::HookExplain => None,
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
        Command::McpServe { planning } => mcp::serve(&source_profiles, &store, planning)?,
        Command::Explain {
            anchor,
            depth,
            format,
        } => handle_explain(&store, &anchor, depth, format)?,
        Command::Link {
            effect,
            cause,
            relation,
            note,
        } => handle_link(&store, effect, cause, relation, note, cli.identity.session)?,
        Command::HookExplain => metadata::run_explain_hook(&store)?,
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
    }

    Ok(())
}

fn handle_explain(
    store: &LocalStore,
    anchor: &str,
    depth: Option<usize>,
    format: args::HistoryFormatArg,
) -> Result<()> {
    history::ensure_json(format);
    history::refresh_repo_sources_best_effort(store.repo_root());
    let events = store.read_all_events()?;
    let index = store.load_or_rebuild_index()?;
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

    let mut chain = brick_core::explain_from_events(&index, &events, resolved, depth);
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

fn handle_link(
    store: &LocalStore,
    effect: Option<String>,
    cause: Option<String>,
    relation: Option<String>,
    note: Option<String>,
    session: Option<String>,
) -> Result<()> {
    let mut args = serde_json::Map::new();
    if let Some(value) = effect {
        args.insert("effect".to_string(), json!(value));
    }
    if let Some(value) = cause {
        args.insert("cause".to_string(), json!(value));
    }
    if let Some(value) = relation {
        args.insert("relation".to_string(), json!(value));
    }
    if let Some(value) = note {
        args.insert("note".to_string(), json!(value));
    }
    if let Some(value) = session {
        args.insert("session".to_string(), json!(value));
    }
    let value = mcp::link_for_cli(store, &Value::Object(args))?;
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
