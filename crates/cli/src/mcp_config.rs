//! Brick MCP-server entry injection into agent MCP config files.
//!
//! MCP-capable agents discover servers from a config file, but the format is not
//! universal. Brick supports three families, all merged non-destructively so a
//! user's existing servers are never clobbered:
//!
//! - **`mcpServers` JSON** — Claude Code, Cursor, ORGII, Windsurf, Claude
//!   Desktop. `{ "mcpServers": { "<name>": { "type", "command", "args" } } }`.
//! - **`servers` JSON** — VS Code (Copilot). Same shape, different root key.
//! - **Codex TOML** — `~/.codex/config.toml`, `[mcp_servers.<name>]` tables.
//!   Edited format-preservingly (comments + existing tables kept intact).
//!
//! Used by `brick agent install` to make `brick mcp-serve` callable by any
//! MCP-capable agent with zero manual config.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};

use crate::claude_hook::HookAction;

/// The server name Brick registers under, in every format.
pub const SERVER_NAME: &str = "brick";

/// The config file format / schema a given target uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// JSON with a `mcpServers` root object (Claude, Cursor, ORGII, Windsurf, …).
    JsonMcpServers,
    /// JSON with a `servers` root object (VS Code Copilot).
    JsonServers,
    /// Codex `config.toml` with `[mcp_servers.<name>]` tables.
    CodexToml,
}

impl Format {
    /// The root object key for JSON formats. Unused for TOML.
    fn json_root_key(self) -> &'static str {
        match self {
            Format::JsonServers => "servers",
            // Both TOML and mcpServers nominally map here; TOML never calls this.
            _ => "mcpServers",
        }
    }
}

/// Builds the Brick MCP server launch spec for JSON formats: a stdio server
/// running the absolute `brick` binary + `mcp-serve`, so it works regardless of
/// the user's `PATH`.
fn brick_json_entry(brick_bin: &str) -> Value {
    json!({
        "type": "stdio",
        "command": brick_bin,
        "args": ["mcp-serve"],
    })
}

/// Installs or refreshes the Brick MCP entry, leaving other servers intact.
pub fn install(path: &Path, brick_bin: &str, format: Format, force: bool) -> Result<HookAction> {
    match format {
        Format::CodexToml => toml_install(path, brick_bin, force),
        _ => json_install(path, brick_bin, format, force),
    }
}

/// Removes the Brick MCP entry if present, leaving other servers intact.
pub fn uninstall(path: &Path, format: Format) -> Result<HookAction> {
    match format {
        Format::CodexToml => toml_uninstall(path),
        _ => json_uninstall(path, format),
    }
}

/// Reports whether a current/stale/absent Brick MCP entry is present.
pub fn status(path: &Path, brick_bin: &str, format: Format) -> Result<HookAction> {
    match format {
        Format::CodexToml => toml_status(path, brick_bin),
        _ => json_status(path, brick_bin, format),
    }
}

// ---- JSON formats (mcpServers / servers) ----------------------------------

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

/// Borrows the root object's server map (creating it), keyed per format.
fn ensure_servers<'a>(root: &'a mut Value, format: Format) -> Result<&'a mut Map<String, Value>> {
    if !root.is_object() {
        anyhow::bail!("MCP config root is not a JSON object");
    }
    let key = format.json_root_key();
    let map = root.as_object_mut().expect("checked object");
    let entry = map.entry(key).or_insert_with(|| Value::Object(Map::new()));
    if !entry.is_object() {
        anyhow::bail!("`{key}` is not a JSON object");
    }
    Ok(entry.as_object_mut().expect("checked object"))
}

fn json_install(path: &Path, brick_bin: &str, format: Format, force: bool) -> Result<HookAction> {
    let mut root = read_json(path)?;
    let desired = brick_json_entry(brick_bin);
    let servers = ensure_servers(&mut root, format)?;
    let action = match servers.get(SERVER_NAME) {
        Some(existing) if existing == &desired && !force => return Ok(HookAction::Unchanged),
        Some(_) => HookAction::Updated,
        None => HookAction::Installed,
    };
    servers.insert(SERVER_NAME.to_string(), desired);
    write_json(path, &root)?;
    Ok(action)
}

fn json_uninstall(path: &Path, format: Format) -> Result<HookAction> {
    let mut root = read_json(path)?;
    if !root.is_object() {
        return Ok(HookAction::Absent);
    }
    let key = format.json_root_key();
    let Some(servers) = root
        .as_object_mut()
        .and_then(|map| map.get_mut(key))
        .and_then(Value::as_object_mut)
    else {
        return Ok(HookAction::Absent);
    };
    if servers.remove(SERVER_NAME).is_none() {
        return Ok(HookAction::Absent);
    }
    write_json(path, &root)?;
    Ok(HookAction::Removed)
}

fn json_status(path: &Path, brick_bin: &str, format: Format) -> Result<HookAction> {
    let root = read_json(path)?;
    let entry = root
        .get(format.json_root_key())
        .and_then(|servers| servers.get(SERVER_NAME));
    match entry {
        Some(existing) if existing == &brick_json_entry(brick_bin) => Ok(HookAction::Present),
        Some(_) => Ok(HookAction::Updated),
        None => Ok(HookAction::Absent),
    }
}

// ---- Codex TOML format ----------------------------------------------------

use toml_edit::{value, Array, DocumentMut, Item, Table};

/// Reads the Codex config as an editable TOML document, empty when absent.
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

/// Builds the `[mcp_servers.brick]` table contents Codex expects.
fn brick_toml_table(brick_bin: &str) -> Table {
    let mut table = Table::new();
    table.insert("command", value(brick_bin));
    let mut args = Array::new();
    args.push("mcp-serve");
    table.insert("args", value(args));
    table
}

/// Returns true when an existing `[mcp_servers.brick]` table already matches the
/// desired command + args (so install can short-circuit to Unchanged).
fn toml_entry_matches(doc: &DocumentMut, brick_bin: &str) -> bool {
    let Some(servers) = doc.get("mcp_servers").and_then(Item::as_table) else {
        return false;
    };
    let Some(brick) = servers.get(SERVER_NAME).and_then(Item::as_table) else {
        return false;
    };
    let cmd_ok = brick
        .get("command")
        .and_then(Item::as_str)
        .map(|c| c == brick_bin)
        .unwrap_or(false);
    let args_ok = brick
        .get("args")
        .and_then(Item::as_array)
        .map(|a| a.len() == 1 && a.get(0).and_then(|v| v.as_str()) == Some("mcp-serve"))
        .unwrap_or(false);
    cmd_ok && args_ok
}

fn toml_install(path: &Path, brick_bin: &str, force: bool) -> Result<HookAction> {
    let mut doc = read_toml(path)?;
    let existed = doc
        .get("mcp_servers")
        .and_then(Item::as_table)
        .map(|t| t.contains_key(SERVER_NAME))
        .unwrap_or(false);
    if existed && !force && toml_entry_matches(&doc, brick_bin) {
        return Ok(HookAction::Unchanged);
    }

    // Ensure [mcp_servers] is a dotted-table parent (implicit) so the emitted
    // form is the idiomatic `[mcp_servers.brick]` header, not an inline table.
    if doc.get("mcp_servers").and_then(Item::as_table).is_none() {
        let mut parent = Table::new();
        parent.set_implicit(true);
        doc["mcp_servers"] = Item::Table(parent);
    }
    let servers = doc["mcp_servers"]
        .as_table_mut()
        .expect("mcp_servers is a table");
    servers.insert(SERVER_NAME, Item::Table(brick_toml_table(brick_bin)));

    write_toml(path, &doc)?;
    Ok(if existed {
        HookAction::Updated
    } else {
        HookAction::Installed
    })
}

fn toml_uninstall(path: &Path) -> Result<HookAction> {
    let mut doc = read_toml(path)?;
    let Some(servers) = doc.get_mut("mcp_servers").and_then(Item::as_table_mut) else {
        return Ok(HookAction::Absent);
    };
    if servers.remove(SERVER_NAME).is_none() {
        return Ok(HookAction::Absent);
    }
    // Drop an emptied [mcp_servers] parent so we don't leave a bare header.
    if servers.is_empty() {
        doc.as_table_mut().remove("mcp_servers");
    }
    write_toml(path, &doc)?;
    Ok(HookAction::Removed)
}

fn toml_status(path: &Path, brick_bin: &str) -> Result<HookAction> {
    let doc = read_toml(path)?;
    let present = doc
        .get("mcp_servers")
        .and_then(Item::as_table)
        .map(|t| t.contains_key(SERVER_NAME))
        .unwrap_or(false);
    if !present {
        return Ok(HookAction::Absent);
    }
    if toml_entry_matches(&doc, brick_bin) {
        Ok(HookAction::Present)
    } else {
        Ok(HookAction::Updated)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_path(name: &str, ext: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brick-mcpcfg-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir.join(format!("config.{ext}"))
    }

    // ---- mcpServers JSON ----

    #[test]
    fn json_install_creates_entry_with_type() {
        let path = temp_path("create", "json");
        assert_eq!(
            install(&path, "/bin/brick", Format::JsonMcpServers, false).unwrap(),
            HookAction::Installed
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["brick"]["type"], "stdio");
        assert_eq!(v["mcpServers"]["brick"]["command"], "/bin/brick");
        assert_eq!(v["mcpServers"]["brick"]["args"][0], "mcp-serve");
    }

    #[test]
    fn json_install_is_idempotent() {
        let path = temp_path("idem", "json");
        install(&path, "/bin/brick", Format::JsonMcpServers, false).unwrap();
        assert_eq!(
            install(&path, "/bin/brick", Format::JsonMcpServers, false).unwrap(),
            HookAction::Unchanged
        );
    }

    #[test]
    fn json_install_preserves_other_servers() {
        let path = temp_path("preserve", "json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"playwright":{"command":"npx","args":["@playwright/mcp@latest"]}}}"#,
        )
        .unwrap();
        install(&path, "/bin/brick", Format::JsonMcpServers, false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["mcpServers"]["playwright"]["command"], "npx");
        assert_eq!(v["mcpServers"]["brick"]["command"], "/bin/brick");
    }

    #[test]
    fn json_uninstall_removes_only_brick() {
        let path = temp_path("uninstall", "json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"playwright":{"command":"npx","args":[]}}}"#,
        )
        .unwrap();
        install(&path, "/bin/brick", Format::JsonMcpServers, false).unwrap();
        assert_eq!(
            uninstall(&path, Format::JsonMcpServers).unwrap(),
            HookAction::Removed
        );
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["mcpServers"]["playwright"].is_object());
        assert!(v["mcpServers"]["brick"].is_null());
    }

    #[test]
    fn json_status_present_stale_absent() {
        let path = temp_path("status", "json");
        assert_eq!(
            status(&path, "/bin/brick", Format::JsonMcpServers).unwrap(),
            HookAction::Absent
        );
        install(&path, "/bin/brick", Format::JsonMcpServers, false).unwrap();
        assert_eq!(
            status(&path, "/bin/brick", Format::JsonMcpServers).unwrap(),
            HookAction::Present
        );
        assert_eq!(
            status(&path, "/other/brick", Format::JsonMcpServers).unwrap(),
            HookAction::Updated
        );
    }

    // ---- servers JSON (VS Code) ----

    #[test]
    fn vscode_uses_servers_key() {
        let path = temp_path("vscode", "json");
        install(&path, "/bin/brick", Format::JsonServers, false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(v["servers"]["brick"].is_object());
        assert!(v["mcpServers"].is_null());
        assert_eq!(v["servers"]["brick"]["command"], "/bin/brick");
    }

    #[test]
    fn vscode_preserves_existing_servers_key() {
        let path = temp_path("vscode-pre", "json");
        std::fs::write(
            &path,
            r#"{"servers":{"other":{"command":"x"}},"inputs":[]}"#,
        )
        .unwrap();
        install(&path, "/bin/brick", Format::JsonServers, false).unwrap();
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(v["servers"]["other"]["command"], "x");
        assert_eq!(v["servers"]["brick"]["command"], "/bin/brick");
        assert!(v["inputs"].is_array()); // unrelated keys preserved
    }

    // ---- Codex TOML ----

    #[test]
    fn toml_install_creates_table() {
        let path = temp_path("toml-create", "toml");
        assert_eq!(
            install(&path, "/bin/brick", Format::CodexToml, false).unwrap(),
            HookAction::Installed
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[mcp_servers.brick]"));
        assert!(text.contains("command = \"/bin/brick\""));
        assert!(text.contains("mcp-serve"));
    }

    #[test]
    fn toml_install_is_idempotent() {
        let path = temp_path("toml-idem", "toml");
        install(&path, "/bin/brick", Format::CodexToml, false).unwrap();
        assert_eq!(
            install(&path, "/bin/brick", Format::CodexToml, false).unwrap(),
            HookAction::Unchanged
        );
    }

    #[test]
    fn toml_install_preserves_comments_and_other_servers() {
        let path = temp_path("toml-preserve", "toml");
        let original = "# Global settings\nmodel = \"gpt-5\"\n\n\
            [mcp_servers.node_repl]\nargs = []\ncommand = \"/usr/bin/node_repl\"\n\n\
            [mcp_servers.node_repl.env]\nFOO = \"bar\"\n";
        std::fs::write(&path, original).unwrap();
        install(&path, "/bin/brick", Format::CodexToml, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        // Comment preserved
        assert!(text.contains("# Global settings"));
        // Existing top-level key preserved
        assert!(text.contains("model = \"gpt-5\""));
        // Existing server + its nested env table preserved
        assert!(text.contains("[mcp_servers.node_repl]"));
        assert!(text.contains("[mcp_servers.node_repl.env]"));
        assert!(text.contains("FOO = \"bar\""));
        // Brick added
        assert!(text.contains("[mcp_servers.brick]"));
    }

    #[test]
    fn toml_uninstall_removes_only_brick() {
        let path = temp_path("toml-uninstall", "toml");
        let original = "[mcp_servers.node_repl]\ncommand = \"/usr/bin/node_repl\"\nargs = []\n";
        std::fs::write(&path, original).unwrap();
        install(&path, "/bin/brick", Format::CodexToml, false).unwrap();
        assert_eq!(
            uninstall(&path, Format::CodexToml).unwrap(),
            HookAction::Removed
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("[mcp_servers.node_repl]"));
        assert!(!text.contains("[mcp_servers.brick]"));
    }

    #[test]
    fn toml_uninstall_absent_is_reported() {
        let path = temp_path("toml-absent", "toml");
        assert_eq!(
            uninstall(&path, Format::CodexToml).unwrap(),
            HookAction::Absent
        );
    }

    #[test]
    fn toml_status_present_stale_absent() {
        let path = temp_path("toml-status", "toml");
        assert_eq!(
            status(&path, "/bin/brick", Format::CodexToml).unwrap(),
            HookAction::Absent
        );
        install(&path, "/bin/brick", Format::CodexToml, false).unwrap();
        assert_eq!(
            status(&path, "/bin/brick", Format::CodexToml).unwrap(),
            HookAction::Present
        );
        assert_eq!(
            status(&path, "/other/brick", Format::CodexToml).unwrap(),
            HookAction::Updated
        );
    }
}
