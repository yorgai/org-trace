//! Claude Code `PreToolUse` hook management for `brick agent`.
//!
//! The agent-awareness markdown block is a soft nudge. A Claude Code hook makes
//! explain context automatic: before read/search tools, Claude runs
//! `brick hook-explain`, which injects the target file's causal context. This
//! module merges a Brick-owned hook entry into `settings.json` without disturbing
//! the user's other hooks, keyed by a stable marker command so install is
//! idempotent and uninstall is exact.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};

/// Tool names whose reads should trigger an explain push: when the agent is
/// about to inspect a file, surface Brick's WHY before it concludes from code.
const EXPLAIN_HOOK_MATCHER: &str = "Read|Grep|Glob";

/// Stable marker so we can find (and only touch) the Brick-owned hook entry even
/// if the user reorders or adds their own hooks.
const EXPLAIN_HOOK_COMMAND_MARKER: &str = "brick hook-explain";

/// Result of an install/uninstall/status operation on the hook config.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookAction {
    Installed,
    Updated,
    Unchanged,
    Removed,
    Absent,
    Present,
}

impl HookAction {
    pub fn as_str(self) -> &'static str {
        match self {
            HookAction::Installed => "installed",
            HookAction::Updated => "updated",
            HookAction::Unchanged => "unchanged",
            HookAction::Removed => "removed",
            HookAction::Absent => "absent",
            HookAction::Present => "present",
        }
    }
}

/// The Claude `settings.json` path for a scope: `<dir>/.claude/settings.json`
/// locally (falling back to the current directory when `dir` is `None`, matching
/// the markdown block's resolution), or `~/.claude/settings.json` globally.
/// Returns `None` when the relevant base cannot be resolved.
pub fn settings_path(global: bool, dir: Option<&Path>, home: Option<&Path>) -> Option<PathBuf> {
    if global {
        return home.map(|home| home.join(".claude").join("settings.json"));
    }
    let base = match dir {
        Some(dir) => dir.to_path_buf(),
        None => std::env::current_dir().ok()?,
    };
    Some(base.join(".claude").join("settings.json"))
}

fn explain_hook_command(brick_bin: &str) -> String {
    format!("{brick_bin} hook-explain")
}

/// Builds the Brick-owned `PreToolUse` explain (read/search) matcher block.
fn brick_explain_hook_entry(brick_bin: &str) -> Value {
    json!({
        "matcher": EXPLAIN_HOOK_MATCHER,
        "hooks": [
            {
                "type": "command",
                "command": explain_hook_command(brick_bin),
            }
        ]
    })
}

/// Whether a `PreToolUse` matcher entry contains a hook command matching `marker`.
fn entry_has_marker(entry: &Value, marker: &str) -> bool {
    entry
        .get("hooks")
        .and_then(Value::as_array)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Value::as_str)
                    .is_some_and(|command| command.contains(marker))
            })
        })
}

/// Whether a `PreToolUse` matcher entry is any Brick-owned one (recall or explain).
fn is_brick_entry(entry: &Value) -> bool {
    entry_has_marker(entry, EXPLAIN_HOOK_COMMAND_MARKER)
}

/// Installs or refreshes the Brick `PreToolUse` explain hook in `settings.json`,
/// leaving all other settings and hooks untouched. Creates the file if absent.
pub fn install(path: &Path, brick_bin: &str, force: bool) -> Result<HookAction> {
    let mut root = read_settings(path)?;
    let pre_tool_use = ensure_pre_tool_use_array(&mut root)?;

    let change = upsert_entry(
        pre_tool_use,
        EXPLAIN_HOOK_COMMAND_MARKER,
        brick_explain_hook_entry(brick_bin),
        force,
    );

    if change == EntryChange::Unchanged {
        return Ok(HookAction::Unchanged);
    }
    write_settings(path, &root)?;
    match change {
        EntryChange::Installed => Ok(HookAction::Installed),
        EntryChange::Updated => Ok(HookAction::Updated),
        EntryChange::Unchanged => Ok(HookAction::Unchanged),
    }
}

/// Outcome of upserting one matcher entry into the `PreToolUse` array.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryChange {
    Installed,
    Updated,
    Unchanged,
}

/// Inserts or refreshes the single Brick entry identified by `marker`, returning
/// what changed. Matches by the marker so it only touches the Brick-owned entry.
fn upsert_entry(
    entries: &mut Vec<Value>,
    marker: &str,
    desired: Value,
    force: bool,
) -> EntryChange {
    match entries
        .iter()
        .position(|entry| entry_has_marker(entry, marker))
    {
        Some(index) => {
            if !force && entries[index] == desired {
                EntryChange::Unchanged
            } else {
                entries[index] = desired;
                EntryChange::Updated
            }
        }
        None => {
            entries.push(desired);
            EntryChange::Installed
        }
    }
}

/// Removes the Brick-owned `PreToolUse` hook entry if present, pruning emptied
/// containers so we don't leave `{"hooks":{"PreToolUse":[]}}` behind.
pub fn uninstall(path: &Path) -> Result<HookAction> {
    if !path.exists() {
        return Ok(HookAction::Absent);
    }
    let mut root = read_settings(path)?;
    let Some(pre_tool_use) = pre_tool_use_array_mut(&mut root) else {
        return Ok(HookAction::Absent);
    };
    let before = pre_tool_use.len();
    pre_tool_use.retain(|entry| !is_brick_entry(entry));
    if pre_tool_use.len() == before {
        return Ok(HookAction::Absent);
    }
    prune_empty_hook_containers(&mut root);
    write_settings(path, &root)?;
    Ok(HookAction::Removed)
}

/// Reports whether the Brick hook is present.
pub fn status(path: &Path) -> Result<HookAction> {
    if !path.exists() {
        return Ok(HookAction::Absent);
    }
    let mut root = read_settings(path)?;
    let present =
        pre_tool_use_array_mut(&mut root).is_some_and(|entries| entries.iter().any(is_brick_entry));
    Ok(if present {
        HookAction::Present
    } else {
        HookAction::Absent
    })
}

/// Reads `settings.json` as a JSON object, returning an empty object when the
/// file does not exist.
fn read_settings(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    let value: Value = serde_json::from_str(&raw)
        .with_context(|| format!("failed to parse {} as JSON", path.display()))?;
    if !value.is_object() {
        anyhow::bail!("{} is not a JSON object", path.display());
    }
    Ok(value)
}

/// Writes `settings.json` atomically (temp file + rename), pretty-printed,
/// creating parent dirs as needed.
fn write_settings(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let rendered = serde_json::to_string_pretty(value)?;
    let tmp = path.with_extension("json.brick-tmp");
    std::fs::write(&tmp, format!("{rendered}\n"))
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to commit {}", path.display()))?;
    Ok(())
}

/// Returns a mutable ref to `hooks.PreToolUse`, creating both as needed.
fn ensure_pre_tool_use_array(root: &mut Value) -> Result<&mut Vec<Value>> {
    let object = root
        .as_object_mut()
        .context("settings root is not a JSON object")?;
    let hooks = object
        .entry("hooks")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .context("settings `hooks` is not a JSON object")?;
    let pre = hooks
        .entry("PreToolUse")
        .or_insert_with(|| json!([]))
        .as_array_mut()
        .context("settings `hooks.PreToolUse` is not a JSON array")?;
    Ok(pre)
}

/// Returns a mutable ref to an existing `hooks.PreToolUse` array, or `None` when
/// either container is missing.
fn pre_tool_use_array_mut(root: &mut Value) -> Option<&mut Vec<Value>> {
    root.get_mut("hooks")?.get_mut("PreToolUse")?.as_array_mut()
}

/// Drops `hooks.PreToolUse` if it became empty, and `hooks` if that left it
/// empty, so uninstall is a clean inverse of install.
fn prune_empty_hook_containers(root: &mut Value) {
    let Some(object) = root.as_object_mut() else {
        return;
    };
    let Some(hooks) = object.get_mut("hooks").and_then(Value::as_object_mut) else {
        return;
    };
    if hooks
        .get("PreToolUse")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
    {
        hooks.remove("PreToolUse");
    }
    if hooks.is_empty() {
        object.remove("hooks");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_settings(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brick-hook-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(".claude").join("settings.json")
    }

    #[test]
    fn install_creates_file_with_hook() {
        let path = temp_settings("create");
        assert_eq!(
            install(&path, "/usr/bin/brick", false).unwrap(),
            HookAction::Installed
        );
        let value = read_settings(&path).unwrap();
        let entries = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert!(entries.iter().all(is_brick_entry));
        assert!(entries.iter().any(|e| e["matcher"] == EXPLAIN_HOOK_MATCHER));
    }

    #[test]
    fn install_twice_is_idempotent() {
        let path = temp_settings("idempotent");
        install(&path, "/usr/bin/brick", false).unwrap();
        assert_eq!(
            install(&path, "/usr/bin/brick", false).unwrap(),
            HookAction::Unchanged
        );
        let value = read_settings(&path).unwrap();
        assert_eq!(value["hooks"]["PreToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn install_preserves_user_settings_and_hooks() {
        let path = temp_settings("preserve");
        let user = json!({
            "model": "claude-opus-4",
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo user" }] }
                ],
                "PostToolUse": [
                    { "matcher": "Edit", "hooks": [{ "type": "command", "command": "echo post" }] }
                ]
            }
        });
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&user).unwrap()).unwrap();

        install(&path, "/usr/bin/brick", false).unwrap();
        let value = read_settings(&path).unwrap();
        assert_eq!(value["model"], "claude-opus-4");
        let pre = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 2); // user's Bash hook + Brick explain hook
        assert!(pre.iter().any(|e| e["matcher"] == "Bash"));
        assert_eq!(pre.iter().filter(|e| is_brick_entry(e)).count(), 1);
        assert_eq!(value["hooks"]["PostToolUse"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn force_updates_command_path() {
        let path = temp_settings("force");
        install(&path, "/old/brick", false).unwrap();
        assert_eq!(
            install(&path, "/new/brick", true).unwrap(),
            HookAction::Updated
        );
        let value = read_settings(&path).unwrap();
        let cmd = value["hooks"]["PreToolUse"][0]["hooks"][0]["command"]
            .as_str()
            .unwrap();
        assert_eq!(cmd, "/new/brick hook-explain");
    }

    #[test]
    fn uninstall_removes_only_brick_hook_and_prunes() {
        let path = temp_settings("uninstall");
        install(&path, "/usr/bin/brick", false).unwrap();
        assert_eq!(uninstall(&path).unwrap(), HookAction::Removed);
        let value = read_settings(&path).unwrap();
        // hooks container pruned because it became empty
        assert!(value.get("hooks").is_none());
    }

    #[test]
    fn uninstall_keeps_user_hooks() {
        let path = temp_settings("uninstall-keep");
        let user = json!({
            "hooks": {
                "PreToolUse": [
                    { "matcher": "Bash", "hooks": [{ "type": "command", "command": "echo user" }] }
                ]
            }
        });
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, serde_json::to_string_pretty(&user).unwrap()).unwrap();
        install(&path, "/usr/bin/brick", false).unwrap();

        assert_eq!(uninstall(&path).unwrap(), HookAction::Removed);
        let value = read_settings(&path).unwrap();
        let pre = value["hooks"]["PreToolUse"].as_array().unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0]["matcher"], "Bash");
    }

    #[test]
    fn status_reports_present_absent() {
        let path = temp_settings("status");
        assert_eq!(status(&path).unwrap(), HookAction::Absent);
        install(&path, "/usr/bin/brick", false).unwrap();
        assert_eq!(status(&path).unwrap(), HookAction::Present);
    }

    #[test]
    fn uninstall_absent_when_no_file() {
        let path = temp_settings("uninstall-absent");
        assert_eq!(uninstall(&path).unwrap(), HookAction::Absent);
    }

    #[test]
    fn settings_path_local_falls_back_to_cwd() {
        // Local scope with no explicit dir must resolve under the current dir,
        // matching the markdown block, not return None (which would wrongly skip
        // the hook as `no_known_global_path`).
        let resolved = settings_path(false, None, None).expect("local path resolves without dir");
        assert!(resolved.ends_with(".claude/settings.json"));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn settings_path_global_needs_home() {
        assert!(settings_path(true, None, None).is_none());
        let home = PathBuf::from("/home/u");
        assert_eq!(
            settings_path(true, None, Some(&home)).unwrap(),
            PathBuf::from("/home/u/.claude/settings.json")
        );
    }
}
