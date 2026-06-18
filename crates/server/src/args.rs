//! CLI argument model for the self-hosted trace server.
//!
//! The server command surface is intentionally small while sync semantics are
//! still append-only and unauthenticated.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Parsed command line for `brick-server`.
#[derive(Debug, Parser)]
#[command(name = "brick-server")]
#[command(about = "Self-hosted Brick provenance server")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Server lifecycle commands reserved for self-hosted deployments.
#[derive(Debug, Subcommand)]
pub enum Command {
    Serve {
        #[arg(long, default_value = "127.0.0.1:7821")]
        bind: String,
        #[arg(long, default_value = ".brick-server")]
        data_dir: PathBuf,
        #[arg(long)]
        enable_local_history: bool,
        #[arg(long, default_value = "brick")]
        brick_bin: PathBuf,
        #[arg(long)]
        repo_root: Option<PathBuf>,
    },
    RebuildIndex {
        #[arg(long, default_value = ".brick-server")]
        data_dir: PathBuf,
        #[arg(long)]
        repo_id: Option<String>,
    },
    Migrate,
    CreateAdmin,
}
