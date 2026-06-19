//! Brick MCP-server entry injection into agent MCP config files.
//!
//! Claude Code and Cursor both discover MCP servers from a JSON file whose
//! `mcpServers` object maps a server name to a `{ "command", "args" }` launch
//! spec. This module merges a Brick-owned `brick` entry into that object,
//! leaving every other server untouched, and writes atomically — so a user's
//! existing MCP servers (playwright, brave-search, …) are never clobbered.
//!
//! Used by `brick agent install` to make `brick mcp-serve` callable by any
//! MCP-capable agent with zero manual config.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use crate::claude_hook::HookAction;

/// The server name Brick registers under in `mcpServers`.
pub const SERVER_NAME: &str = "brick";

/// Builds the Brick MCP server launch spec: the absolute `brick` binary path
/// plus the `mcp-serve` subcommand, so it runs regardless of the user's `PATH`.
fn brick_entry(brick_bin: &str) -> Value {
    json!({
        "command": brick_bin,
        "args": ["mcp-serve"],
    })
}

/// Reads an MCP config file as a JSON object, returning an empty object when the
/// file is absent. Errors on malformed JSON so we never silently overwrite a file
/// we failed to parse.
fn read_config(path: &Path) -> Result<Value> {
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

/// Writes the config atomically (temp file + rename), creating parent dirs.
fn write_config(path: &Path, value: &Value) -> Result<()> {
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

/// Borrows the root object's `mcpServers` map, creating it if missing.
fn ensure_mcp_servers(root: &mut Value) -> Result<&mut Map<String, Value>> {
    if !root.is_object() {
        anyhow::bail!("MCP config root is not a JSON object");
    }
    let map = root.as_object_mut().expect("checked object");
    let entry = map
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        anyhow::bail!("`mcpServers` is not a JSON object");
    }
    Ok(entry.as_object_mut().expect("checked object"))
}

/// Installs or refreshes the Brick MCP entry, leaving all other servers intact.
pub fn install(path: &Path, brick_bin: &str, force: bool) -> Result<HookAction> {
    let mut root = read_config(path)?;
    let desired = brick_entry(brick_bin);
    let servers = ensure_mcp_servers(&mut root)?;
    let action = match servers.get(SERVER_NAME) {
        Some(existing) if existing == &desired && !force => return Ok(HookAction::Unchanged),
        Some(_) => HookAction::Updated,
        None => HookAction::Installed,
    };
    servers.insert(SERVER_NAME.to_string(), desired);
    write_config(path, &root)?;
    Ok(action)
}

/// Removes the Brick MCP entry if present, leaving other servers intact.
pub fn uninstall(path: &Path) -> Result<HookAction> {
    let mut root = read_config(path)?;
    if !root.is_object() {
        return Ok(HookAction::Absent);
    }
    let Some(servers) = root
        .as_object_mut()
        .and_then(|map| map.get_mut("mcpServers"))
        .and_then(Value::as_object_mut)
    else {
        return Ok(HookAction::Absent);
    };
    if servers.remove(SERVER_NAME).is_none() {
        return Ok(HookAction::Absent);
    }
    write_config(path, &root)?;
    Ok(HookAction::Removed)
}

/// Reports whether a current/stale/absent Brick MCP entry is present.
pub fn status(path: &Path, brick_bin: &str) -> Result<HookAction> {
    let root = read_config(path)?;
    let entry = root
        .get("mcpServers")
        .and_then(|servers| servers.get(SERVER_NAME));
    match entry {
        Some(existing) if existing == &brick_entry(brick_bin) => Ok(HookAction::Present),
        Some(_) => Ok(HookAction::Updated), // present but stale (path/args differ)
        None => Ok(HookAction::Absent),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_config(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brick-mcpcfg-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join("mcp.json")
    }

    #[test]
    fn install_creates_entry_in_empty_file() {
        let path = temp_config("create");
        assert_eq!(
            install(&path, "/usr/local/bin/brick", false).unwrap(),
            HookAction::Installed
        );
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            value["mcpServers"]["brick"]["command"],
            "/usr/local/bin/brick"
        );
        assert_eq!(value["mcpServers"]["brick"]["args"][0], "mcp-serve");
    }

    #[test]
    fn install_is_idempotent() {
        let path = temp_config("idempotent");
        install(&path, "/bin/brick", false).unwrap();
        assert_eq!(
            install(&path, "/bin/brick", false).unwrap(),
            HookAction::Unchanged
        );
    }

    #[test]
    fn install_preserves_other_servers() {
        let path = temp_config("preserve");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"playwright":{"command":"npx","args":["@playwright/mcp@latest"]}}}"#,
        )
        .unwrap();
        install(&path, "/bin/brick", false).unwrap();
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        // both present
        assert_eq!(value["mcpServers"]["playwright"]["command"], "npx");
        assert_eq!(value["mcpServers"]["brick"]["command"], "/bin/brick");
    }

    #[test]
    fn install_updates_stale_path() {
        let path = temp_config("update");
        install(&path, "/old/brick", false).unwrap();
        assert_eq!(
            install(&path, "/new/brick", false).unwrap(),
            HookAction::Updated
        );
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(value["mcpServers"]["brick"]["command"], "/new/brick");
    }

    #[test]
    fn uninstall_removes_only_brick() {
        let path = temp_config("uninstall");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"playwright":{"command":"npx","args":[]}}}"#,
        )
        .unwrap();
        install(&path, "/bin/brick", false).unwrap();
        assert_eq!(uninstall(&path).unwrap(), HookAction::Removed);
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(value["mcpServers"]["playwright"].is_object());
        assert!(value["mcpServers"]["brick"].is_null());
    }

    #[test]
    fn uninstall_absent_is_reported() {
        let path = temp_config("uninstall-absent");
        assert_eq!(uninstall(&path).unwrap(), HookAction::Absent);
    }

    #[test]
    fn status_reports_present_stale_absent() {
        let path = temp_config("status");
        assert_eq!(status(&path, "/bin/brick").unwrap(), HookAction::Absent);
        install(&path, "/bin/brick", false).unwrap();
        assert_eq!(status(&path, "/bin/brick").unwrap(), HookAction::Present);
        // a different path is stale
        assert_eq!(status(&path, "/other/brick").unwrap(), HookAction::Updated);
    }
}
