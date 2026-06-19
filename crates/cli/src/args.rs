//! CLI argument model for the Brick-native provenance surface.
//!
//! These structures define the public command contract around Orgs, Projects,
//! Missions, Sessions, Artifacts, and Evidence. Old recorder-shaped commands
//! are intentionally not kept as aliases.

use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

pub const DEFAULT_RELATIONSHIP: &str = "contributes_to";

#[derive(Debug, Parser)]
#[command(name = "brick")]
#[command(about = "Record and sync Brick work provenance")]
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
    Version {
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Org {
        #[command(subcommand)]
        command: OrgCommand,
    },
    Project {
        #[command(subcommand)]
        command: ProjectCommand,
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
    Evidence {
        #[command(subcommand)]
        command: EvidenceCommand,
    },
    Context {
        #[command(subcommand)]
        command: ContextCommand,
    },
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    Source {
        #[command(subcommand)]
        command: SourceCommand,
    },
    Import {
        #[command(subcommand)]
        command: ImportCommand,
    },
    History {
        #[command(subcommand)]
        command: HistoryCommand,
    },
    Metadata {
        #[command(subcommand)]
        command: MetadataCommand,
    },
    Sync {
        #[command(subcommand)]
        command: SyncCommand,
    },
    Maintenance {
        #[command(subcommand)]
        command: MaintenanceCommand,
    },
}

#[derive(Debug, Subcommand)]
pub enum OrgCommand {
    Create {
        name: String,
        #[arg(long)]
        description: Option<String>,
    },
    Show {
        org: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ProjectCommand {
    Create {
        #[arg(long)]
        org: String,
        name: String,
        #[arg(long)]
        description: Option<String>,
    },
    Show {
        project: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum MissionStatusArg {
    Planned,
    Active,
    Blocked,
    Completed,
    Archived,
}

#[derive(Debug, Subcommand)]
pub enum MissionCommand {
    Create {
        #[arg(long)]
        project: String,
        title: String,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, value_enum, default_value_t = MissionStatusArg::Planned)]
        status: MissionStatusArg,
    },
    Update {
        mission: String,
        #[arg(long)]
        project: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
        description: Option<String>,
        #[arg(long, value_enum)]
        status: Option<MissionStatusArg>,
    },
    Show {
        mission: String,
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
    Show {
        session: String,
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
    Create {
        #[arg(long)]
        mission: Option<String>,
        #[arg(long)]
        session: Option<String>,
        #[arg(long, value_enum, default_value_t = ArtifactKindArg::Decision)]
        kind: ArtifactKindArg,
        title: String,
        #[arg(long)]
        body: Option<String>,
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
    Show {
        artifact: String,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum DiffTargetArg {
    Working,
    Staged,
}

#[derive(Debug, Subcommand)]
pub enum EvidenceCommand {
    Attach {
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
        #[arg(long)]
        copy: bool,
    },
    File {
        #[arg(long)]
        artifact: String,
        #[arg(long)]
        session: Option<String>,
        path: String,
    },
    Log {
        #[arg(long)]
        session: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long, value_enum)]
        format: Option<SessionLogFormatArg>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        copy: bool,
    },
    Diff {
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
    FileShow {
        path: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum ContextCommand {
    Show,
}

#[derive(Debug, Subcommand)]
pub enum SyncCommand {
    Run(SyncArgs),
    Push(SyncArgs),
    Pull(SyncArgs),
}

#[derive(Debug, Args)]
pub struct SyncArgs {
    #[arg(long)]
    pub dry_run: bool,
    #[arg(long)]
    pub remote: Option<String>,
    #[arg(long)]
    pub org_id: Option<String>,
    #[arg(long)]
    pub repo_id: Option<String>,
}

#[derive(Debug, Subcommand)]
pub enum MaintenanceCommand {
    Status,
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
pub enum SourceCommand {
    Configure(SourceConfigureArgs),
    Config(SourceConfigArgs),
    Scan(SourceScanArgs),
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
pub enum HistoryCommand {
    Sources {
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Sessions {
        #[arg(long)]
        source: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Plans {
        #[arg(long)]
        source: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    RecentPaths {
        #[arg(long)]
        source: String,
        #[arg(long, default_value_t = 20)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Doctor {
        #[arg(long)]
        source: String,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Chunks {
        #[arg(long)]
        source: String,
        #[arg(long)]
        session_id: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        offset: usize,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Export {
        #[arg(long)]
        source: String,
        #[arg(long)]
        session_id: String,
        #[arg(long, value_enum, default_value_t = HistoryExportSchemaArg::AuditV1)]
        schema: HistoryExportSchemaArg,
        #[arg(long, value_enum, default_value_t = HistoryExportFormatArg::Json)]
        format: HistoryExportFormatArg,
    },
    FileSessionBlame {
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "all")]
        source: String,
        #[arg(long, default_value_t = 100)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Link {
        #[arg(long)]
        brick_session: String,
        #[arg(long)]
        source: String,
        #[arg(long)]
        session_id: String,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    Linked {
        #[arg(long)]
        brick_session: String,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum HistoryFormatArg {
    Json,
}

/// `brick metadata <subcommand>` — agent-facing recall over indexed metadata.
#[derive(Debug, Subcommand)]
pub enum MetadataCommand {
    /// Summarize who changed a file across past sessions and why.
    Recall {
        #[arg(long)]
        path: String,
        #[arg(long, default_value = "all")]
        source: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    /// Find past sessions related to a free-text query (title/intent, files, repo).
    Query {
        #[arg(long)]
        query: String,
        #[arg(long, default_value = "all")]
        source: String,
        #[arg(long, default_value_t = 10)]
        limit: usize,
        #[arg(long, value_enum, default_value_t = HistoryFormatArg::Json)]
        format: HistoryFormatArg,
    },
    /// Claude Code PreToolUse hook adapter: reads the tool-call JSON on stdin and
    /// emits `hookSpecificOutput.additionalContext` recalling the target file.
    RecallHook {
        #[arg(long, default_value = "all")]
        source: String,
        #[arg(long, default_value_t = 5)]
        limit: usize,
    },
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum HistoryExportSchemaArg {
    AuditV1,
    SourceMetadataV1,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum HistoryExportFormatArg {
    Json,
    Csv,
}

/// Which agent memory file(s) to inject Brick awareness into.
#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum AgentTargetArg {
    /// `CLAUDE.md` (Claude Code).
    Claude,
    /// `AGENTS.md` (Codex, Cursor, Copilot, OpenCode, etc.).
    Codex,
    /// `GEMINI.md` (Gemini).
    Gemini,
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

/// Targeting/scope flags shared by the agent subcommands.
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

#[derive(Debug, Subcommand)]
pub enum ImportCommand {
    Cursor(AgentImportArgs),
    Codex(AgentImportArgs),
    ClaudeCode(AgentImportArgs),
    Native {
        #[command(subcommand)]
        command: NativeImportCommand,
    },
    Ci(CiImportArgs),
}

#[derive(Debug, Subcommand)]
pub enum NativeImportCommand {
    List(NativeImportListArgs),
    Ingest(NativeImportIngestArgs),
    Pick(NativeImportPickArgs),
}

#[derive(Debug, Args)]
pub struct NativeImportListArgs {
    #[arg(long, default_value_t = 20)]
    pub limit: usize,
}

#[derive(Debug, Args)]
pub struct NativeImportPickArgs {
    #[arg(long, default_value_t = 50)]
    pub limit: usize,

    #[arg(long)]
    pub mission: Option<String>,

    #[arg(long)]
    pub force: bool,
}

#[derive(Debug, Args)]
pub struct NativeImportIngestArgs {
    #[arg(long)]
    pub external_session_id: String,

    #[arg(long)]
    pub mission: Option<String>,

    #[arg(long)]
    pub session: Option<String>,

    #[arg(long)]
    pub force: bool,
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
pub struct SourceScanArgs {
    #[arg(long)]
    pub write_defaults: bool,

    #[arg(long)]
    pub include: Vec<String>,

    #[arg(long, value_enum, default_value_t = SourceScanFormatArg::Text)]
    pub format: SourceScanFormatArg,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
#[value(rename_all = "snake_case")]
pub enum SourceScanFormatArg {
    Text,
    Json,
}

#[derive(Debug, Args)]
pub struct SourceConfigArgs {
    #[arg(long)]
    pub default_full_evidence_upload: Option<bool>,

    #[arg(long)]
    pub metadata_only_local: Option<bool>,
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
    pub evidence_root: Option<PathBuf>,

    #[arg(long)]
    pub cursor_state_db_path: Option<PathBuf>,

    #[arg(long)]
    pub default_full_evidence_upload: Option<bool>,

    #[arg(long)]
    pub notes: Option<String>,
}
