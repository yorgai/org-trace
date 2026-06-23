//! Native hook registration for non-Claude agents.
//!
//! Claude Code keeps its dedicated settings manager in `claude_hook.rs`. This
//! module covers agents with different native hook config formats that can still
//! call Brick's existing hook adapters.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Value};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

use crate::claude_hook::HookAction;

const EXPLAIN_MARKER: &str = "brick hook-explain";
const CODEX_EXPLAIN_MATCHER: &str = "Read|Grep|Glob|mcp__.*read.*|mcp__.*grep.*|mcp__.*glob.*";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookClient {
    Codex,
    Windsurf,
}

impl HookClient {
    pub fn label(self) -> &'static str {
        match self {
            HookClient::Codex => "codex_hook",
            HookClient::Windsurf => "windsurf_hook",
        }
    }
}

pub fn install(
    client: HookClient,
    path: &Path,
    brick_bin: &str,
    force: bool,
) -> Result<HookAction> {
    match client {
        HookClient::Codex => codex_install(path, brick_bin, force),
        HookClient::Windsurf => windsurf_install(path, brick_bin, force),
    }
}

pub fn uninstall(client: HookClient, path: &Path) -> Result<HookAction> {
    match client {
        HookClient::Codex => codex_uninstall(path),
        HookClient::Windsurf => windsurf_uninstall(path),
    }
}

pub fn status(client: HookClient, path: &Path, brick_bin: &str) -> Result<HookAction> {
    match client {
        HookClient::Codex => codex_status(path, brick_bin),
        HookClient::Windsurf => windsurf_status(path, brick_bin),
    }
}

fn explain_command(brick_bin: &str) -> String {
    format!("{brick_bin} hook-explain")
}

fn is_brick_command(command: &str) -> bool {
    command.contains(EXPLAIN_MARKER)
}

// ---- Codex TOML hooks ------------------------------------------------------

fn read_toml(path: &Path) -> Result<DocumentMut> {
    if !path.exists() {
        return Ok(DocumentMut::new());
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    raw.parse::<DocumentMut>()
        .with_context(|| format!("failed to parse {}", path.display()))
}

fn write_toml(path: &Path, doc: &DocumentMut) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("brick-tmp");
    std::fs::write(&tmp, doc.to_string())
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to commit {}", path.display()))?;
    Ok(())
}

fn codex_hook_table(matcher: &str, command: String, status_message: &str) -> Table {
    let mut entry = Table::new();
    entry.insert("matcher", value(matcher));

    let mut hook = Table::new();
    hook.insert("type", value("command"));
    hook.insert("command", value(command));
    hook.insert("timeout", value(30));
    hook.insert("statusMessage", value(status_message));

    let mut hooks = ArrayOfTables::new();
    hooks.push(hook);
    entry.insert("hooks", Item::ArrayOfTables(hooks));
    entry
}

fn codex_desired(brick_bin: &str) -> Vec<Table> {
    vec![codex_hook_table(
        CODEX_EXPLAIN_MATCHER,
        explain_command(brick_bin),
        "Loading Brick causal context",
    )]
}

fn codex_install(path: &Path, brick_bin: &str, force: bool) -> Result<HookAction> {
    let mut doc = read_toml(path)?;
    let existed = codex_has_brick_hooks(&doc);
    if existed && !force && codex_matches(&doc, brick_bin) {
        return Ok(HookAction::Unchanged);
    }

    ensure_hooks_table(&mut doc);
    let hooks = doc["hooks"].as_table_mut().expect("hooks table exists");
    if hooks
        .get("PreToolUse")
        .and_then(Item::as_array_of_tables)
        .is_none()
    {
        hooks.insert("PreToolUse", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let pre = hooks
        .get_mut("PreToolUse")
        .and_then(Item::as_array_of_tables_mut)
        .context("hooks.PreToolUse is not an array of tables")?;
    pre.retain(|entry| !codex_entry_is_brick(entry));
    for entry in codex_desired(brick_bin) {
        pre.push(entry);
    }

    write_toml(path, &doc)?;
    Ok(if existed {
        HookAction::Updated
    } else {
        HookAction::Installed
    })
}

fn codex_uninstall(path: &Path) -> Result<HookAction> {
    let mut doc = read_toml(path)?;
    let Some(pre) = doc
        .get_mut("hooks")
        .and_then(Item::as_table_mut)
        .and_then(|hooks| hooks.get_mut("PreToolUse"))
        .and_then(Item::as_array_of_tables_mut)
    else {
        return Ok(HookAction::Absent);
    };
    let before = pre.len();
    pre.retain(|entry| !codex_entry_is_brick(entry));
    if pre.len() == before {
        return Ok(HookAction::Absent);
    }
    prune_empty_toml_hooks(&mut doc);
    write_toml(path, &doc)?;
    Ok(HookAction::Removed)
}

fn codex_status(path: &Path, brick_bin: &str) -> Result<HookAction> {
    let doc = read_toml(path)?;
    if !codex_has_brick_hooks(&doc) {
        return Ok(HookAction::Absent);
    }
    if codex_matches(&doc, brick_bin) {
        Ok(HookAction::Present)
    } else {
        Ok(HookAction::Updated)
    }
}

fn ensure_hooks_table(doc: &mut DocumentMut) {
    if doc.get("hooks").and_then(Item::as_table).is_none() {
        let mut hooks = Table::new();
        hooks.set_implicit(true);
        doc["hooks"] = Item::Table(hooks);
    }
}

fn codex_has_brick_hooks(doc: &DocumentMut) -> bool {
    doc.get("hooks")
        .and_then(Item::as_table)
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Item::as_array_of_tables)
        .is_some_and(|entries| entries.iter().any(codex_entry_is_brick))
}

fn codex_matches(doc: &DocumentMut, brick_bin: &str) -> bool {
    let Some(entries) = doc
        .get("hooks")
        .and_then(Item::as_table)
        .and_then(|hooks| hooks.get("PreToolUse"))
        .and_then(Item::as_array_of_tables)
    else {
        return false;
    };
    let desired = codex_desired(brick_bin);
    desired.iter().all(|desired| {
        desired
            .get("matcher")
            .and_then(Item::as_str)
            .is_some_and(|matcher| {
                desired
                    .get("hooks")
                    .and_then(Item::as_array_of_tables)
                    .and_then(|hooks| hooks.iter().next())
                    .and_then(|hook| {
                        Some((
                            matcher,
                            hook.get("command")?.as_str()?,
                            hook.get("statusMessage")?.as_str()?,
                        ))
                    })
                    .is_some_and(|(matcher, command, status)| {
                        entries
                            .iter()
                            .any(|entry| codex_entry_matches(entry, matcher, command, status))
                    })
            })
    })
}

fn codex_entry_matches(entry: &Table, matcher: &str, command: &str, status: &str) -> bool {
    entry.get("matcher").and_then(Item::as_str) == Some(matcher)
        && entry
            .get("hooks")
            .and_then(Item::as_array_of_tables)
            .is_some_and(|hooks| {
                hooks.iter().any(|hook| {
                    hook.get("type").and_then(Item::as_str) == Some("command")
                        && hook.get("command").and_then(Item::as_str) == Some(command)
                        && hook.get("timeout").and_then(Item::as_integer) == Some(30)
                        && hook.get("statusMessage").and_then(Item::as_str) == Some(status)
                })
            })
}

fn codex_entry_is_brick(entry: &Table) -> bool {
    entry
        .get("hooks")
        .and_then(Item::as_array_of_tables)
        .is_some_and(|hooks| {
            hooks.iter().any(|hook| {
                hook.get("command")
                    .and_then(Item::as_str)
                    .is_some_and(is_brick_command)
            })
        })
}

fn prune_empty_toml_hooks(doc: &mut DocumentMut) {
    let Some(hooks) = doc.get_mut("hooks").and_then(Item::as_table_mut) else {
        return;
    };
    let empty_pre = hooks
        .get("PreToolUse")
        .and_then(Item::as_array_of_tables)
        .is_some_and(|pre| pre.is_empty());
    if empty_pre {
        hooks.remove("PreToolUse");
    }
    if hooks.is_empty() {
        doc.as_table_mut().remove("hooks");
    }
}

// ---- Windsurf hooks.json ---------------------------------------------------

fn read_json(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(json!({}));
    }
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    if raw.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&raw).with_context(|| format!("failed to parse {}", path.display()))
}

fn write_json(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let tmp = path.with_extension("brick-tmp");
    let body = serde_json::to_string_pretty(value)?;
    std::fs::write(&tmp, format!("{body}\n"))
        .with_context(|| format!("failed to write {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("failed to commit {}", path.display()))?;
    Ok(())
}

fn windsurf_entry(command: String) -> Value {
    json!({
        "command": command,
        "show_output": true,
    })
}

fn windsurf_desired(brick_bin: &str) -> Vec<(&'static str, Value)> {
    vec![("pre_read_code", windsurf_entry(explain_command(brick_bin)))]
}

fn windsurf_install(path: &Path, brick_bin: &str, force: bool) -> Result<HookAction> {
    let mut root = read_json(path)?;
    let existed = windsurf_has_brick_hooks(&root);
    if existed && !force && windsurf_matches(&root, brick_bin) {
        return Ok(HookAction::Unchanged);
    }

    let hooks = ensure_json_hooks_object(&mut root)?;
    for (event, desired) in windsurf_desired(brick_bin) {
        let entries = hooks
            .entry(event.to_string())
            .or_insert_with(|| Value::Array(Vec::new()))
            .as_array_mut()
            .with_context(|| format!("hooks.{event} is not an array"))?;
        entries.retain(|entry| !windsurf_entry_is_brick(entry));
        entries.push(desired);
    }

    write_json(path, &root)?;
    Ok(if existed {
        HookAction::Updated
    } else {
        HookAction::Installed
    })
}

fn windsurf_uninstall(path: &Path) -> Result<HookAction> {
    let mut root = read_json(path)?;
    let Some(hooks) = root.get_mut("hooks").and_then(Value::as_object_mut) else {
        return Ok(HookAction::Absent);
    };
    let mut removed = false;
    for entries in hooks.values_mut().filter_map(Value::as_array_mut) {
        let before = entries.len();
        entries.retain(|entry| !windsurf_entry_is_brick(entry));
        removed |= entries.len() != before;
    }
    if !removed {
        return Ok(HookAction::Absent);
    }
    hooks.retain(|_, value| value.as_array().is_none_or(|entries| !entries.is_empty()));
    if hooks.is_empty() {
        root.as_object_mut()
            .expect("json root object")
            .remove("hooks");
    }
    write_json(path, &root)?;
    Ok(HookAction::Removed)
}

fn windsurf_status(path: &Path, brick_bin: &str) -> Result<HookAction> {
    let root = read_json(path)?;
    if !windsurf_has_brick_hooks(&root) {
        return Ok(HookAction::Absent);
    }
    if windsurf_matches(&root, brick_bin) {
        Ok(HookAction::Present)
    } else {
        Ok(HookAction::Updated)
    }
}

fn ensure_json_hooks_object(root: &mut Value) -> Result<&mut serde_json::Map<String, Value>> {
    if !root.is_object() {
        anyhow::bail!("hooks config root is not a JSON object");
    }
    let object = root.as_object_mut().expect("checked object");
    let hooks = object.entry("hooks").or_insert_with(|| json!({}));
    hooks.as_object_mut().context("hooks is not a JSON object")
}

fn windsurf_has_brick_hooks(root: &Value) -> bool {
    root.get("hooks")
        .and_then(Value::as_object)
        .is_some_and(|hooks| {
            hooks
                .values()
                .filter_map(Value::as_array)
                .flatten()
                .any(windsurf_entry_is_brick)
        })
}

fn windsurf_matches(root: &Value, brick_bin: &str) -> bool {
    let Some(hooks) = root.get("hooks").and_then(Value::as_object) else {
        return false;
    };
    windsurf_desired(brick_bin).iter().all(|(event, desired)| {
        hooks
            .get(*event)
            .and_then(Value::as_array)
            .is_some_and(|entries| entries.iter().any(|entry| entry == desired))
    })
}

fn windsurf_entry_is_brick(entry: &Value) -> bool {
    entry
        .get("command")
        .and_then(Value::as_str)
        .is_some_and(is_brick_command)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str, ext: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brick-native-hook-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(format!("config.{ext}"))
    }

    #[test]
    fn codex_install_preserves_existing_config() {
        let path = temp_path("codex", "toml");
        std::fs::write(
            &path,
            r#"model = "gpt-5"

[[hooks.PreToolUse]]
matcher = "Bash"
[[hooks.PreToolUse.hooks]]
type = "command"
command = "echo user"
"#,
        )
        .unwrap();

        assert_eq!(
            install(HookClient::Codex, &path, "/bin/brick", false).unwrap(),
            HookAction::Installed
        );
        let doc = read_toml(&path).unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        let entries = doc["hooks"]["PreToolUse"].as_array_of_tables().unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(
            status(HookClient::Codex, &path, "/bin/brick").unwrap(),
            HookAction::Present
        );
    }

    #[test]
    fn codex_install_is_idempotent_and_uninstalls_only_brick() {
        let path = temp_path("codex-idem", "toml");
        install(HookClient::Codex, &path, "/bin/brick", false).unwrap();
        assert_eq!(
            install(HookClient::Codex, &path, "/bin/brick", false).unwrap(),
            HookAction::Unchanged
        );
        assert_eq!(
            uninstall(HookClient::Codex, &path).unwrap(),
            HookAction::Removed
        );
        assert_eq!(
            status(HookClient::Codex, &path, "/bin/brick").unwrap(),
            HookAction::Absent
        );
    }

    #[test]
    fn windsurf_install_preserves_existing_hooks() {
        let path = temp_path("windsurf", "json");
        std::fs::write(
            &path,
            r#"{
  "hooks": {
    "pre_run_command": [
      { "command": "echo user", "show_output": false }
    ]
  }
}
"#,
        )
        .unwrap();

        assert_eq!(
            install(HookClient::Windsurf, &path, "/bin/brick", false).unwrap(),
            HookAction::Installed
        );
        let root = read_json(&path).unwrap();
        assert_eq!(
            root["hooks"]["pre_run_command"].as_array().unwrap().len(),
            1
        );
        assert_eq!(root["hooks"]["pre_read_code"].as_array().unwrap().len(), 1);
        assert_eq!(
            status(HookClient::Windsurf, &path, "/bin/brick").unwrap(),
            HookAction::Present
        );
    }

    #[test]
    fn windsurf_install_is_idempotent_and_uninstalls_only_brick() {
        let path = temp_path("windsurf-idem", "json");
        install(HookClient::Windsurf, &path, "/bin/brick", false).unwrap();
        assert_eq!(
            install(HookClient::Windsurf, &path, "/bin/brick", false).unwrap(),
            HookAction::Unchanged
        );
        assert_eq!(
            uninstall(HookClient::Windsurf, &path).unwrap(),
            HookAction::Removed
        );
        assert_eq!(
            status(HookClient::Windsurf, &path, "/bin/brick").unwrap(),
            HookAction::Absent
        );
    }
}
