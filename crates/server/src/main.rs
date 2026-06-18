//! Minimal self-hosted server entry point.
//!
//! The server now exposes a small append-only event sync surface while keeping
//! auth, repo authorization, queue draining, and migrations for later phases.

use anyhow::Result;
use clap::Parser;

mod args;
mod index;
mod routes;
mod store;

use args::{Cli, Command};
use index::{rebuild_server_index, server_index_status};
use routes::{serve, LocalHistoryBridge};
use store::ServerStore;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            bind,
            data_dir,
            enable_local_history,
            brick_bin,
            repo_root,
        } => {
            let history_bridge =
                enable_local_history.then(|| LocalHistoryBridge::new(brick_bin, repo_root));
            serve(bind, ServerStore::new(data_dir), history_bridge).await?
        }
        Command::RebuildIndex { data_dir, repo_id } => {
            let store = ServerStore::new(data_dir);
            let events = store.read_events_for_repo(repo_id.as_deref())?;
            let index = rebuild_server_index(repo_id.as_deref(), &events)?;
            let status = server_index_status(repo_id.as_deref(), &index);
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Command::Migrate => println!("migrate is not implemented yet"),
        Command::CreateAdmin => println!("create-admin is not implemented yet"),
    }

    Ok(())
}
