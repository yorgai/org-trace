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
        /// Require `Authorization: Bearer <token>` on all routes except
        /// `/health`. When unset the server stays open (append-only MVP).
        #[arg(long, env = "BRICK_SERVER_AUTH_TOKEN")]
        auth_token: Option<String>,
    },
    RebuildIndex {
        #[arg(long, default_value = ".brick-server")]
        data_dir: PathBuf,
        #[arg(long)]
        repo_id: Option<String>,
    },
    Migrate,
    CreateAdmin,
    /// Issue a new scoped access token. Prints the plaintext once.
    CreateToken {
        #[arg(long, default_value = ".brick-server")]
        data_dir: PathBuf,
        /// Human-facing label identifying who/what holds the token.
        #[arg(long)]
        label: String,
        /// Resource scope; repeatable. Accepts `*`/`all`, `org:<id>`,
        /// `repo:<id>`, or a bare repo id. Defaults to `*`.
        #[arg(long = "scope")]
        scopes: Vec<String>,
        /// Grant write access (read + write). Omit for read-only.
        #[arg(long)]
        write: bool,
    },
    /// List issued tokens (labels + scope/access summary; never plaintext).
    ListTokens {
        #[arg(long, default_value = ".brick-server")]
        data_dir: PathBuf,
    },
    /// Revoke a token by its label.
    RevokeToken {
        #[arg(long, default_value = ".brick-server")]
        data_dir: PathBuf,
        #[arg(long)]
        label: String,
    },
}
