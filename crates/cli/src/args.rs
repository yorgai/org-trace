//! CLI argument model for the agent-facing trace recorder.
//!
//! These structures define the public command contract for humans and agents.
//! Prefer adding explicit subcommands here over hidden behavior in handlers.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

pub const DEFAULT_RELATIONSHIP: &str = "contributes_to";

#[derive(Debug, Parser)]
#[command(name = "brick")]
#[command(about = "Record and sync signed work-unit provenance events")]
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
    Init,
    Diff {
        #[command(subcommand)]
        command: DiffCommand,
    },
    Mission {
        #[command(subcommand)]
        command: MissionCommand,
    },
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    Artifact {
        #[command(subcommand)]
        command: ArtifactCommand,
    },
    Status,
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    Log {
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    Index {
        #[command(subcommand)]
        command: IndexCommand,
    },
    Db {
        #[command(subcommand)]
        command: DbCommand,
    },
    Inspect {
        #[command(subcommand)]
        command: InspectCommand,
    },
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    Sync {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        repo_id: Option<String>,
    },
    Push {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        repo_id: Option<String>,
    },
    Pull {
        #[arg(long)]
        dry_run: bool,
        #[arg(long)]
        remote: Option<String>,
        #[arg(long)]
        repo_id: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum DiffTargetArg {
    Working,
    Staged,
}

#[derive(Debug, Subcommand)]
pub enum DiffCommand {
    Capture {
        #[arg(long)]
        artifact: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        mission: Option<String>,
        #[arg(long, value_enum)]
        target: DiffTargetArg,
        #[arg(long)]
        base: Option<String>,
        #[arg(long)]
        head: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum MissionCommand {
    Create {
        title: String,
        #[arg(long)]
        description: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum SessionLogFormatArg {
    Text,
    Jsonl,
    Markdown,
    Unknown,
}

#[derive(Debug, Subcommand)]
pub enum SessionCommand {
    Current,
    List {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        app_id: Option<String>,
        #[arg(long)]
        actor_id: Option<String>,
        #[arg(long)]
        runtime_id: Option<String>,
    },
    Find {
        #[arg(long)]
        app_id: Option<String>,
        #[arg(long)]
        app_session_id: Option<String>,
        #[arg(long)]
        app_session_name: Option<String>,
        #[arg(long)]
        runtime_id: Option<String>,
        #[arg(long)]
        actor_id: Option<String>,
    },
    Start {
        #[arg(long)]
        mission: Option<String>,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        set_current: bool,
        #[arg(long)]
        print_env: bool,
    },
    Link {
        #[arg(long)]
        mission: String,
        #[arg(long)]
        session: String,
        #[arg(long, default_value = DEFAULT_RELATIONSHIP)]
        relationship: String,
    },
    UploadLog {
        #[arg(long)]
        session: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, value_enum)]
        format: Option<SessionLogFormatArg>,
        #[arg(long)]
        source: Option<String>,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum ArtifactKindArg {
    Decision,
    FileRef,
    Patch,
    Review,
    TestResult,
    Acceptance,
    Note,
}

#[derive(Debug, Subcommand)]
pub enum ArtifactCommand {
    Decision {
        #[arg(long)]
        mission: Option<String>,
        #[arg(long)]
        session: Option<String>,
        title: String,
        #[arg(long)]
        body: Option<String>,
    },
    File {
        #[arg(long)]
        artifact: String,
        #[arg(long)]
        session: Option<String>,
        path: String,
    },
    Link {
        #[arg(long)]
        mission: String,
        #[arg(long)]
        artifact: String,
        #[arg(long, default_value = DEFAULT_RELATIONSHIP)]
        relationship: String,
    },
    Update {
        #[arg(long)]
        artifact: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        body: Option<String>,
        #[arg(long, value_enum)]
        kind: Option<ArtifactKindArg>,
    },
    Upload {
        #[arg(long)]
        artifact: String,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        name: Option<String>,
        #[arg(long)]
        content_type: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum ContextCommand {
    /// Shows the resolved identity that write commands would use.
    Show,
}

#[derive(Debug, Subcommand)]
pub enum IndexCommand {
    Rebuild,
    Status,
}

#[derive(Debug, Subcommand)]
pub enum DbCommand {
    Rebuild,
    Status,
    Sessions {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        app_id: Option<String>,
        #[arg(long)]
        actor_id: Option<String>,
        #[arg(long)]
        runtime_id: Option<String>,
    },
    Artifacts {
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        mission: Option<String>,
    },
}

#[derive(Debug, Subcommand)]
pub enum InspectCommand {
    Mission { mission: String },
    Session { session: String },
    Artifact { artifact: String },
    File { path: String },
}

#[derive(Debug, Subcommand)]
pub enum SourceCommand {
    Configure(SourceConfigureArgs),
    List,
    Show {
        #[arg(long)]
        name: String,
    },
    Use {
        #[arg(long)]
        name: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ImportCommand {
    Cursor(AgentImportArgs),
    Codex(AgentImportArgs),
    ClaudeCode(AgentImportArgs),
    Ci(CiImportArgs),
}

#[derive(Debug, Args)]
pub struct AgentImportArgs {
    #[arg(long, required = true)]
    pub path: Vec<PathBuf>,

    #[arg(long)]
    pub session: Option<String>,

    #[arg(long)]
    pub mission: Option<String>,

    #[arg(long)]
    pub app_session_id: Option<String>,

    #[arg(long)]
    pub app_session_name: Option<String>,
}

#[derive(Debug, Args)]
pub struct CiImportArgs {
    #[arg(long, required = true)]
    pub path: Vec<PathBuf>,

    #[arg(long)]
    pub mission: Option<String>,

    #[arg(long)]
    pub session: Option<String>,
}

#[derive(Debug, Args)]
pub struct SourceConfigureArgs {
    #[arg(long)]
    pub name: String,

    #[arg(long)]
    pub app_id: Option<String>,

    #[arg(long)]
    pub actor_id: Option<String>,

    #[arg(long)]
    pub actor_type: Option<String>,

    #[arg(long)]
    pub store_root: Option<PathBuf>,

    #[arg(long)]
    pub session_db_path: Option<PathBuf>,

    #[arg(long)]
    pub session_log_path: Option<PathBuf>,

    #[arg(long)]
    pub notes: Option<String>,
}
