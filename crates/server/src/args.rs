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
        /// Supabase project URL used to validate JWT issuer. Required together
        /// with `--supabase-jwt-secret` to accept Supabase bearer tokens.
        #[arg(long, env = "BRICK_SUPABASE_URL")]
        supabase_url: Option<String>,
        /// Supabase project's JWT secret for HS256 access-token verification.
        #[arg(long, env = "BRICK_SUPABASE_JWT_SECRET")]
        supabase_jwt_secret: Option<String>,
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
        /// Expire the token this many days after issuance. Omit for no expiry.
        #[arg(long)]
        expires_in_days: Option<u32>,
        /// Bind the token to an actor identity. When set, pushed events must
        /// carry an `actor.actor_id` equal to this value. Omit to leave the
        /// token unbound (events are not actor-checked).
        #[arg(long)]
        actor_id: Option<String>,
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
    /// Rotate a token's secret in place, keeping its scopes and access. Prints
    /// the new plaintext once; the old secret stops working immediately.
    RotateToken {
        #[arg(long, default_value = ".brick-server")]
        data_dir: PathBuf,
        #[arg(long)]
        label: String,
        /// Reset expiry to this many days from now. Omit to keep the token's
        /// current expiry; pass 0 to clear any expiry.
        #[arg(long)]
        expires_in_days: Option<u32>,
    },
    /// Print the write-audit log (one JSON line per authorized write).
    Audit {
        #[arg(long, default_value = ".brick-server")]
        data_dir: PathBuf,
        /// Only show the most recent N entries.
        #[arg(long)]
        limit: Option<usize>,
    },
}
