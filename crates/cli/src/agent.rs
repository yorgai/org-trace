//! `brick agent` — inject Brick awareness into AI coding agents.
//!
//! Coding agents read a native "memory" file in the working directory as
//! standing context: `CLAUDE.md` (Claude Code), `AGENTS.md` (Codex, Cursor,
//! Copilot, OpenCode, …), `GEMINI.md` (Gemini). This module append/replaces a
//! sentinel-delimited managed block into those files telling the agent to call
//! `brick history` to recall what past sessions did to the files it is about to
//! touch. Edits are confined to the managed region and written atomically, so a
//! user's existing memory file is never corrupted.
//!
//! The markdown block is a *soft* nudge. For Claude Code, `install` additionally
//! registers a `PreToolUse` hook (see [`crate::claude_hook`]) so recall fires
//! *automatically* before every edit — the seamless path that does not depend on
//! the agent remembering to call Brick.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Serialize;

use crate::args::{
    AgentCommand, AgentFormatArg, AgentInstallArgs, AgentTargetArg, AgentTargetArgs,
};
use crate::claude_hook;
use crate::mcp_config;

/// Bumped whenever the managed-block wording changes so `status` can report a
/// block as stale and `install` can roll it forward.
const TEMPLATE_VERSION: u32 = 3;
const BLOCK_START_PREFIX: &str = "<!-- brick:managed:start";
const BLOCK_END: &str = "<!-- brick:managed:end -->";

/// Renders the current managed block, including its version sentinel.
fn managed_block() -> String {
    format!(
        "{BLOCK_START_PREFIX} v={TEMPLATE_VERSION} -->\n{}\n{BLOCK_END}",
        BLOCK_BODY.trim_end()
    )
}

/// The agent-directed instructions. Command-first and short so agents reliably
/// act on it. Update `TEMPLATE_VERSION` when this changes.
const BLOCK_BODY: &str = "\
## Brick — cross-tool session metadata

Brick is a local metadata index of past AI coding sessions across every tool on this
machine. Use it to recall prior decisions instead of rediscovering them.

### Workflow

- Before editing a file, recall who changed it and why in one call:
  `brick metadata recall --path <file> --format json`
  This returns a one-line summary plus per-session intent, change size, and a
  `recall_chunks_hint` command for the full transcript when you need detail.
- To find past sessions by topic (not a specific file), search the metadata index:
  `brick metadata query --query \"<keywords>\" --format json`
  Matches session intent, touched files, repo, and branch; each hit includes a
  `recall_chunks_hint` for the full transcript.
- For broader project history, list sources then drill in:
  `brick history sources --format json`
  `brick history sessions --source <id> --format json`
  `brick history chunks --source <id> --session-id <sid> --format json`

All `brick metadata` and `brick history` commands support `--format json`.";

/// One memory file to act on, resolved from a target + scope.
#[derive(Debug, Clone)]
struct MemoryFile {
    target: AgentTargetArg,
    path: PathBuf,
}

/// What happened to one target during an operation.
#[derive(Debug, Clone, Serialize)]
struct AgentOutcome {
    target: String,
    path: String,
    action: String,
}

/// Entry point for `brick agent <subcommand>`.
pub fn handle_agent(command: AgentCommand) -> Result<()> {
    match command {
        AgentCommand::Install(args) => install(args),
        AgentCommand::Uninstall(args) => uninstall(args),
        AgentCommand::Status(args) => status(args),
    }
}

/// Offered at the end of `brick init`: in an interactive terminal, ask whether
/// to inject the Brick block into this repo's memory files; otherwise print a
/// hint. Kept best-effort so init never fails on this step.
pub fn init_prompt(dir: &Path) -> Result<()> {
    use std::io::IsTerminal;

    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        println!("Run `brick agent install` to make coding agents use Brick metadata.");
        return Ok(());
    }
    let confirm = dialoguer::Confirm::new()
        .with_prompt(
            "Install Brick agent instructions into this repo's CLAUDE.md/AGENTS.md/GEMINI.md?",
        )
        .default(true)
        .interact()?;
    if !confirm {
        return Ok(());
    }
    install(AgentInstallArgs {
        target: AgentTargetArgs {
            global: false,
            target: AgentTargetArg::All,
            dir: Some(dir.to_path_buf()),
            format: AgentFormatArg::Text,
        },
        force: false,
        print: false,
    })
}

fn install(args: AgentInstallArgs) -> Result<()> {
    if args.print {
        println!("{}", managed_block());
        return Ok(());
    }
    let format = args.target.format;
    let (files, skipped) = resolve_targets(&args.target)?;
    let mut outcomes = skipped;
    for file in files {
        let action = file.install(args.force)?;
        outcomes.push(file.outcome(action));
    }
    if let Some(outcome) = claude_hook_outcome(&args.target, HookOp::Install { force: args.force })
    {
        outcomes.push(outcome);
    }
    outcomes.extend(mcp_config_outcomes(
        &args.target,
        &HookOp::Install { force: args.force },
    ));
    report(&outcomes, format);
    Ok(())
}

fn uninstall(args: AgentTargetArgs) -> Result<()> {
    let format = args.format;
    let (files, skipped) = resolve_targets(&args)?;
    let mut outcomes = skipped;
    for file in files {
        let action = file.uninstall()?;
        outcomes.push(file.outcome(action));
    }
    if let Some(outcome) = claude_hook_outcome(&args, HookOp::Uninstall) {
        outcomes.push(outcome);
    }
    outcomes.extend(mcp_config_outcomes(&args, &HookOp::Uninstall));
    report(&outcomes, format);
    Ok(())
}

fn status(args: AgentTargetArgs) -> Result<()> {
    let format = args.format;
    let (files, skipped) = resolve_targets(&args)?;
    let mut outcomes = skipped;
    for file in files {
        let action = file.status()?;
        outcomes.push(file.outcome(action));
    }
    if let Some(outcome) = claude_hook_outcome(&args, HookOp::Status) {
        outcomes.push(outcome);
    }
    outcomes.extend(mcp_config_outcomes(&args, &HookOp::Status));
    report(&outcomes, format);
    Ok(())
}

/// Which hook operation to run, paired with `claude_hook_outcome`.
enum HookOp {
    Install { force: bool },
    Uninstall,
    Status,
}

/// Runs the requested Claude `PreToolUse` hook operation when the claude target
/// (or `all`) is selected, returning a reportable outcome. The hook is a
/// Claude-only mechanism, so other targets are skipped silently. A failure to
/// resolve the settings path or the `brick` binary is reported, not fatal.
fn claude_hook_outcome(args: &AgentTargetArgs, op: HookOp) -> Option<AgentOutcome> {
    if !matches!(args.target, AgentTargetArg::Claude | AgentTargetArg::All) {
        return None;
    }
    let home = home_dir();
    let Some(settings) =
        claude_hook::settings_path(args.global, args.dir.as_deref(), home.as_deref())
    else {
        return Some(hook_outcome(
            String::new(),
            "skipped no_known_global_path".to_string(),
        ));
    };
    let path_label = settings.display().to_string();
    let result = match op {
        HookOp::Install { force } => match brick_binary() {
            Ok(bin) => claude_hook::install(&settings, &bin, force),
            Err(error) => return Some(hook_outcome(path_label, format!("error {error}"))),
        },
        HookOp::Uninstall => claude_hook::uninstall(&settings),
        HookOp::Status => claude_hook::status(&settings),
    };
    let action = match result {
        Ok(action) => action.as_str().to_string(),
        Err(error) => format!("error {error}"),
    };
    Some(hook_outcome(path_label, action))
}

/// Builds an outcome row for the Claude hook (a distinct pseudo-target).
fn hook_outcome(path: String, action: String) -> AgentOutcome {
    AgentOutcome {
        target: "claude_hook".to_string(),
        path,
        action,
    }
}

/// One MCP config file Brick can register `brick mcp-serve` into, with the
/// pseudo-target label used in reports and the config format/schema it uses.
struct McpConfigTarget {
    label: &'static str,
    path: PathBuf,
    format: mcp_config::Format,
}

/// Resolves the MCP config files to act on for the selected target + scope.
///
/// MCP registration spans more clients than the markdown-memory targets because
/// many MCP-capable tools have no native memory file. `all` registers every
/// known client (global scope only — most use a single per-user config). The
/// discrete `cursor`/`vscode`/`windsurf`/`codex`/`orgii` targets act on just one.
///
/// Formats: Claude, Cursor, ORGII, Windsurf, Claude Desktop use the `mcpServers`
/// JSON family; VS Code uses a `servers`-keyed JSON; Codex uses `config.toml`.
fn mcp_config_targets(args: &AgentTargetArgs) -> Vec<McpConfigTarget> {
    use mcp_config::Format;
    let is_all = matches!(args.target, AgentTargetArg::All);
    let want = |t: AgentTargetArg| is_all || args.target == t;
    let mut targets = Vec::new();

    // Claude Code: project `.mcp.json` (local) or `~/.claude.json` (global).
    if want(AgentTargetArg::Claude) {
        if !args.global {
            if let Some(base) = local_base(args) {
                targets.push(McpConfigTarget {
                    label: "claude_mcp",
                    path: base.join(".mcp.json"),
                    format: Format::JsonMcpServers,
                });
            }
        } else if let Some(home) = home_dir() {
            targets.push(McpConfigTarget {
                label: "claude_mcp",
                path: home.join(".claude.json"),
                format: Format::JsonMcpServers,
            });
        }
    }

    // Cursor: `.cursor/mcp.json` (local or global).
    if want(AgentTargetArg::Cursor) {
        let base = if args.global {
            home_dir()
        } else {
            local_base(args)
        };
        if let Some(base) = base {
            targets.push(McpConfigTarget {
                label: "cursor_mcp",
                path: base.join(".cursor").join("mcp.json"),
                format: Format::JsonMcpServers,
            });
        }
    }

    // The remaining clients are global-only (single per-user config); skip them
    // for local-scope installs.
    if !args.global {
        return targets;
    }
    let Some(home) = home_dir() else {
        return targets;
    };

    // ORGII: ~/.orgii/mcp-servers.json
    if want(AgentTargetArg::Orgii) {
        targets.push(McpConfigTarget {
            label: "orgii_mcp",
            path: home.join(".orgii").join("mcp-servers.json"),
            format: Format::JsonMcpServers,
        });
    }

    // Windsurf: ~/.codeium/windsurf/mcp_config.json
    if want(AgentTargetArg::Windsurf) {
        targets.push(McpConfigTarget {
            label: "windsurf_mcp",
            path: home
                .join(".codeium")
                .join("windsurf")
                .join("mcp_config.json"),
            format: Format::JsonMcpServers,
        });
    }

    // VS Code (Copilot): per-user mcp.json, `servers` root key.
    if want(AgentTargetArg::Vscode) {
        if let Some(path) = vscode_user_mcp_path(&home) {
            targets.push(McpConfigTarget {
                label: "vscode_mcp",
                path,
                format: Format::JsonServers,
            });
        }
    }

    // Codex: ~/.codex/config.toml, [mcp_servers.<name>] tables.
    if want(AgentTargetArg::Codex) {
        targets.push(McpConfigTarget {
            label: "codex_mcp",
            path: home.join(".codex").join("config.toml"),
            format: Format::CodexToml,
        });
    }

    targets
}

/// Per-user VS Code MCP config path, which is OS-specific.
fn vscode_user_mcp_path(home: &Path) -> Option<PathBuf> {
    #[cfg(target_os = "macos")]
    {
        Some(
            home.join("Library")
                .join("Application Support")
                .join("Code")
                .join("User")
                .join("mcp.json"),
        )
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA")
            .map(PathBuf::from)
            .map(|base| base.join("Code").join("User").join("mcp.json"))
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        Some(
            home.join(".config")
                .join("Code")
                .join("User")
                .join("mcp.json"),
        )
    }
}

/// Resolves the local working directory base for config files.
fn local_base(args: &AgentTargetArgs) -> Option<PathBuf> {
    match args.dir.as_deref() {
        Some(dir) => Some(dir.to_path_buf()),
        None => std::env::current_dir().ok(),
    }
}

/// Runs the requested MCP-config operation across all applicable targets,
/// returning a reportable outcome per file. Mirrors `claude_hook_outcome` but for
/// the pull-side MCP registration.
fn mcp_config_outcomes(args: &AgentTargetArgs, op: &HookOp) -> Vec<AgentOutcome> {
    let targets = mcp_config_targets(args);
    if targets.is_empty() {
        return Vec::new();
    }
    let brick_bin = brick_binary();
    targets
        .into_iter()
        .map(|target| {
            let path_label = target.path.display().to_string();
            let action = match (&brick_bin, op) {
                (Ok(bin), HookOp::Install { force }) => {
                    mcp_config::install(&target.path, bin, target.format, *force)
                        .map(|a| a.as_str().to_string())
                }
                (Ok(bin), HookOp::Status) => mcp_config::status(&target.path, bin, target.format)
                    .map(|a| a.as_str().to_string()),
                (_, HookOp::Uninstall) => mcp_config::uninstall(&target.path, target.format)
                    .map(|a| a.as_str().to_string()),
                (Err(error), _) => Err(anyhow::anyhow!("{error}")),
            };
            AgentOutcome {
                target: target.label.to_string(),
                path: path_label,
                action: action.unwrap_or_else(|error| format!("error {error}")),
            }
        })
        .collect()
}

/// Resolves the absolute path to the running `brick` binary so the hook command
/// works regardless of the user's `PATH`.
fn brick_binary() -> Result<String> {
    let exe = std::env::current_exe().context("failed to resolve brick binary path")?;
    Ok(exe.display().to_string())
}

/// Resolves the requested targets into concrete files, returning any
/// skip-with-reason outcomes (e.g. an unresolvable global path) separately so
/// they still appear in the report.
fn resolve_targets(args: &AgentTargetArgs) -> Result<(Vec<MemoryFile>, Vec<AgentOutcome>)> {
    let targets = match args.target {
        AgentTargetArg::All => vec![
            AgentTargetArg::Claude,
            AgentTargetArg::Codex,
            AgentTargetArg::Gemini,
        ],
        // MCP-only targets have no markdown memory file, so they never
        // contribute a `MemoryFile`; their registration is handled separately
        // by `mcp_config_outcomes`.
        AgentTargetArg::Cursor
        | AgentTargetArg::Orgii
        | AgentTargetArg::Windsurf
        | AgentTargetArg::Vscode => vec![],
        single => vec![single],
    };

    let mut files = Vec::new();
    let mut skipped = Vec::new();
    for target in targets {
        match resolve_path(target, args.global, args.dir.as_deref())? {
            Some(path) => files.push(MemoryFile { target, path }),
            None => skipped.push(AgentOutcome {
                target: target_label(target).to_string(),
                path: String::new(),
                action: "skipped no_known_global_path".to_string(),
            }),
        }
    }
    Ok((files, skipped))
}

/// Maps a target + scope to its memory file path. Local scope writes into the
/// working directory; global scope uses the tool's per-user location, returning
/// `None` when that cannot be resolved (so the caller skips it rather than
/// guessing).
fn resolve_path(
    target: AgentTargetArg,
    global: bool,
    dir: Option<&Path>,
) -> Result<Option<PathBuf>> {
    let filename = target_filename(target);
    if !global {
        let base = match dir {
            Some(dir) => dir.to_path_buf(),
            None => std::env::current_dir().context("failed to read current directory")?,
        };
        return Ok(Some(base.join(filename)));
    }

    let Some(home) = home_dir() else {
        return Ok(None);
    };
    let path = match target {
        AgentTargetArg::Claude => home.join(".claude").join(filename),
        AgentTargetArg::Codex => home.join(".codex").join(filename),
        AgentTargetArg::Gemini => home.join(".gemini").join(filename),
        AgentTargetArg::Cursor
        | AgentTargetArg::Orgii
        | AgentTargetArg::Windsurf
        | AgentTargetArg::Vscode => unreachable!("MCP-only target has no memory file"),
        AgentTargetArg::All => unreachable!("`all` is expanded before path resolution"),
    };
    Ok(Some(path))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn target_filename(target: AgentTargetArg) -> &'static str {
    match target {
        AgentTargetArg::Claude => "CLAUDE.md",
        AgentTargetArg::Codex => "AGENTS.md",
        AgentTargetArg::Gemini => "GEMINI.md",
        AgentTargetArg::Cursor
        | AgentTargetArg::Orgii
        | AgentTargetArg::Windsurf
        | AgentTargetArg::Vscode => unreachable!("MCP-only target has no memory file"),
        AgentTargetArg::All => unreachable!("`all` is expanded before filename resolution"),
    }
}

fn target_label(target: AgentTargetArg) -> &'static str {
    match target {
        AgentTargetArg::Claude => "claude",
        AgentTargetArg::Codex => "codex",
        AgentTargetArg::Gemini => "gemini",
        AgentTargetArg::Cursor => "cursor",
        AgentTargetArg::Orgii => "orgii",
        AgentTargetArg::Windsurf => "windsurf",
        AgentTargetArg::Vscode => "vscode",
        AgentTargetArg::All => "all",
    }
}

impl MemoryFile {
    fn outcome(&self, action: String) -> AgentOutcome {
        AgentOutcome {
            target: target_label(self.target).to_string(),
            path: self.path.display().to_string(),
            action,
        }
    }

    /// Installs or refreshes the managed block, returning the action taken.
    fn install(&self, force: bool) -> Result<String> {
        let existing = self.read()?;
        let block = managed_block();
        let action = match find_block(&existing) {
            Some(span) => {
                if !force && existing[span.clone()] == block {
                    return Ok("unchanged".to_string());
                }
                let mut updated = String::with_capacity(existing.len());
                updated.push_str(&existing[..span.start]);
                updated.push_str(&block);
                updated.push_str(&existing[span.end..]);
                self.write(&updated)?;
                "updated"
            }
            None => {
                let updated = append_block(&existing, &block);
                self.write(&updated)?;
                "installed"
            }
        };
        Ok(action.to_string())
    }

    /// Removes the managed block (and a blank line it introduced) if present.
    fn uninstall(&self) -> Result<String> {
        let existing = self.read()?;
        let Some(span) = find_block(&existing) else {
            return Ok("absent".to_string());
        };
        // Drop one separating blank line before the block, then any leading
        // blank line left behind, so removal is the inverse of append.
        let mut start = span.start;
        if existing[..start].ends_with("\n\n") {
            start -= 1;
        }
        let mut end = span.end;
        if existing[end..].starts_with('\n') {
            end += 1;
        }
        let mut updated = String::with_capacity(existing.len());
        updated.push_str(&existing[..start]);
        updated.push_str(&existing[end..]);
        if updated.trim().is_empty() {
            updated.clear();
        }
        self.write(&updated)?;
        Ok("removed".to_string())
    }

    /// Reports whether a current/stale/absent block is present.
    fn status(&self) -> Result<String> {
        let existing = self.read()?;
        let action = match find_block(&existing) {
            Some(span) if existing[span.clone()] == managed_block() => "present",
            Some(_) => "stale",
            None => "absent",
        };
        Ok(action.to_string())
    }

    fn read(&self) -> Result<String> {
        if !self.path.exists() {
            return Ok(String::new());
        }
        std::fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read {}", self.path.display()))
    }

    /// Writes atomically (temp file + rename), creating parent dirs as needed.
    fn write(&self, contents: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let tmp = self.path.with_extension("md.brick-tmp");
        std::fs::write(&tmp, contents)
            .with_context(|| format!("failed to write {}", tmp.display()))?;
        std::fs::rename(&tmp, &self.path)
            .with_context(|| format!("failed to commit {}", self.path.display()))?;
        Ok(())
    }
}

/// Appends the block to existing content, separated by a blank line, with a
/// trailing newline. An empty file yields just the block.
fn append_block(existing: &str, block: &str) -> String {
    if existing.trim().is_empty() {
        return format!("{block}\n");
    }
    let mut out = existing.trim_end().to_string();
    out.push_str("\n\n");
    out.push_str(block);
    out.push('\n');
    out
}

/// Locates the managed block's byte span (from the start marker to the end of
/// the end marker), if present.
fn find_block(content: &str) -> Option<std::ops::Range<usize>> {
    let start = content.find(BLOCK_START_PREFIX)?;
    let end_marker = content[start..].find(BLOCK_END)? + start;
    Some(start..end_marker + BLOCK_END.len())
}

fn report(outcomes: &[AgentOutcome], format: AgentFormatArg) {
    match format {
        AgentFormatArg::Json => {
            // Best-effort: serialization of these flat structs cannot fail.
            let rendered =
                serde_json::to_string_pretty(outcomes).unwrap_or_else(|_| "[]".to_string());
            println!("{rendered}");
        }
        AgentFormatArg::Text => {
            for outcome in outcomes {
                if outcome.path.is_empty() {
                    println!("{} {}", outcome.target, outcome.action);
                } else {
                    println!("{} {} {}", outcome.target, outcome.action, outcome.path);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brick-agent-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn claude_file(dir: &Path) -> MemoryFile {
        MemoryFile {
            target: AgentTargetArg::Claude,
            path: dir.join("CLAUDE.md"),
        }
    }

    #[test]
    fn install_creates_single_block() {
        let dir = temp_dir("create");
        let file = claude_file(&dir);
        assert_eq!(file.install(false).expect("install"), "installed");

        let content = std::fs::read_to_string(&file.path).expect("read");
        assert_eq!(content.matches(BLOCK_START_PREFIX).count(), 1);
        assert_eq!(content.matches(BLOCK_END).count(), 1);
        assert!(content.contains("brick metadata recall"));
    }

    #[test]
    fn install_twice_is_idempotent() {
        let dir = temp_dir("idempotent");
        let file = claude_file(&dir);
        file.install(false).expect("first install");
        let after_first = std::fs::read_to_string(&file.path).expect("read");

        assert_eq!(file.install(false).expect("second install"), "unchanged");
        let after_second = std::fs::read_to_string(&file.path).expect("read");
        assert_eq!(after_first, after_second);
        assert_eq!(after_second.matches(BLOCK_START_PREFIX).count(), 1);
    }

    #[test]
    fn install_preserves_surrounding_user_content() {
        let dir = temp_dir("preserve");
        let file = claude_file(&dir);
        let user = "# My project\n\nSome house rules.\n";
        std::fs::write(&file.path, user).expect("seed");

        file.install(false).expect("install");
        let content = std::fs::read_to_string(&file.path).expect("read");
        assert!(content.starts_with("# My project\n\nSome house rules."));
        assert!(content.contains(BLOCK_START_PREFIX));
    }

    #[test]
    fn stale_block_is_replaced_in_place() {
        let dir = temp_dir("stale");
        let file = claude_file(&dir);
        let stale = format!(
            "# Top\n\n{BLOCK_START_PREFIX} v=0 -->\nold instructions\n{BLOCK_END}\n\n## Bottom\n"
        );
        std::fs::write(&file.path, &stale).expect("seed");

        assert_eq!(file.install(false).expect("install"), "updated");
        let content = std::fs::read_to_string(&file.path).expect("read");
        assert!(content.starts_with("# Top"));
        assert!(content.trim_end().ends_with("## Bottom"));
        assert!(!content.contains("old instructions"));
        assert!(content.contains(&format!("v={TEMPLATE_VERSION}")));
        assert_eq!(content.matches(BLOCK_START_PREFIX).count(), 1);
    }

    #[test]
    fn uninstall_removes_only_the_block() {
        let dir = temp_dir("uninstall");
        let file = claude_file(&dir);
        let user = "# My project\n\nSome house rules.\n";
        std::fs::write(&file.path, user).expect("seed");
        file.install(false).expect("install");

        assert_eq!(file.uninstall().expect("uninstall"), "removed");
        let content = std::fs::read_to_string(&file.path).expect("read");
        assert_eq!(content, user);
        assert!(!content.contains(BLOCK_START_PREFIX));
    }

    #[test]
    fn uninstall_absent_is_reported() {
        let dir = temp_dir("uninstall-absent");
        let file = claude_file(&dir);
        assert_eq!(file.uninstall().expect("uninstall"), "absent");
    }

    #[test]
    fn status_reports_present_stale_absent() {
        let dir = temp_dir("status");
        let file = claude_file(&dir);
        assert_eq!(file.status().expect("absent"), "absent");

        file.install(false).expect("install");
        assert_eq!(file.status().expect("present"), "present");

        let stale = format!("{BLOCK_START_PREFIX} v=0 -->\nold\n{BLOCK_END}\n");
        std::fs::write(&file.path, stale).expect("seed stale");
        assert_eq!(file.status().expect("stale"), "stale");
    }

    #[test]
    fn local_resolution_uses_provided_dir() {
        let dir = temp_dir("resolve-local");
        let (files, skipped) = resolve_targets(&AgentTargetArgs {
            global: false,
            target: AgentTargetArg::All,
            dir: Some(dir.clone()),
            format: AgentFormatArg::Text,
        })
        .expect("resolve");
        assert!(skipped.is_empty());
        let names: Vec<_> = files
            .iter()
            .map(|f| f.path.file_name().unwrap().to_str().unwrap().to_string())
            .collect();
        assert_eq!(names, ["CLAUDE.md", "AGENTS.md", "GEMINI.md"]);
        assert!(files.iter().all(|f| f.path.starts_with(&dir)));
    }
}
