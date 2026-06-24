//! CLI argument model for the Brick core surface.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "brick")]
#[command(about = "Record and explain Brick causal provenance")]
pub struct Cli {
    #[arg(long, global = true)]
    pub store_root: Option<PathBuf>,

    #[arg(long, global = true)]
    pub source: Option<String>,

    #[command(flatten)]
    pub identity: IdentityArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Args, Clone, Default)]
pub struct IdentityArgs {
    #[arg(long, global = true)]
    pub actor_id: Option<String>,

    #[arg(long, global = true)]
    pub actor_type: Option<String>,

    #[arg(long, global = true)]
    pub runtime_id: Option<String>,

    #[arg(long, global = true)]
    pub session: Option<String>,

    #[arg(long, global = true)]
    pub app_id: Option<String>,

    #[arg(long, global = true)]
    pub app_session_id: Option<String>,

    #[arg(long, global = true)]
    pub app_session_name: Option<String>,

    #[arg(long, global = true)]
    pub mission: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    Version {
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Run as an MCP server over stdio so any MCP-capable agent can query Brick.
    McpServe {
        /// Expose the planning tool surface instead of the default coding-agent
        /// surface (explain + link).
        #[arg(long)]
        planning: bool,
    },
    /// Explain WHY code looks the way it does.
    Explain {
        /// Anchor: `path:line`, `path:start-end`, whole-file path, artifact id,
        /// mission id, or event id.
        anchor: String,
        /// Causal hops to walk back (default 3, max 8).
        #[arg(long)]
        depth: Option<usize>,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    /// Record WHY for a change.
    Link {
        #[arg(long)]
        effect: Option<String>,
        #[arg(long)]
        cause: Option<String>,
        #[arg(long)]
        relation: Option<String>,
        #[arg(long)]
        note: Option<String>,
    },
    /// Claude Code hook adapter: inject explain context before file inspection.
    #[command(name = "hook-explain", hide = true)]
    HookExplain,
    #[cfg(feature = "sync")]
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
}

#[cfg(feature = "sync")]
#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    Run(SyncArgs),
    Push(SyncArgs),
    Pull(SyncArgs),
    Login(LoginArgs),
    Logout,
    Whoami,
}

#[cfg(feature = "sync")]
#[derive(Debug, Args)]
pub struct LoginArgs {
    #[arg(long)]
    pub email: String,
    #[arg(long)]
    pub code: Option<String>,
}

#[cfg(feature = "sync")]
#[derive(Debug, Args)]
pub struct SyncArgs {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub remote: Option<String>,
    #[arg(long)]
    pub repo_id: Option<String>,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum HistoryFormatArg {
    Json,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum AgentTargetArg {
    /// `CLAUDE.md` plus Claude Code skill/MCP registration.
    Claude,
    /// `AGENTS.md` plus Codex skill/MCP registration.
    Codex,
    /// `GEMINI.md` plus Gemini skill registration.
    Gemini,
    /// Cursor skill/MCP registration.
    Cursor,
    /// ORGII rules plus skill/MCP registration.
    Orgii,
    /// Windsurf skill/MCP registration.
    Windsurf,
    /// VS Code (Copilot) — MCP-server registration only.
    Vscode,
    /// Every known target.
    All,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum AgentFormatArg {
    Text,
    Json,
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Inject the Brick managed block into agent memory files.
    Install(AgentInstallArgs),
    /// Remove the Brick managed block, leaving the rest of the file intact.
    Uninstall(AgentTargetArgs),
    /// Report, per target file, whether a Brick block is present and current.
    Status(AgentTargetArgs),
}

#[derive(Debug, Args)]
pub struct AgentTargetArgs {
    /// Write to per-user memory locations instead of the working directory.
    #[arg(long)]
    pub global: bool,
    /// Which memory file(s) to act on.
    #[arg(long, value_enum, default_value_t = AgentTargetArg::All)]
    pub target: AgentTargetArg,
    /// Working directory to operate in (defaults to the current directory).
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value_t = AgentFormatArg::Text)]
    pub format: AgentFormatArg,
}

#[derive(Debug, Args)]
pub struct AgentInstallArgs {
    #[command(flatten)]
    pub target: AgentTargetArgs,
    /// Rewrite the managed block even if it is already up to date.
    #[arg(long)]
    pub force: bool,
    /// Print the block to stdout without writing any file.
    #[arg(long)]
    pub print: bool,
}
