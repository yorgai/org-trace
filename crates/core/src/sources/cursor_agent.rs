//! cursor-agent CLI native history source provider.
//!
//! The `cursor-agent` command-line tool stores each session as its own SQLite
//! file at `~/.cursor/chats/<workspaceHash>/<agentId>/store.db`. Unlike the
//! Cursor IDE desktop app (which moved conversation bubbles into an encrypted
//! `conversationState` blob), the cursor-agent store keeps every message as
//! plaintext JSON, including tool inputs AND outputs.
//!
//! Storage model (a content-addressed Merkle DAG):
//! - `meta` table: one row, key=`"0"`, value = hex-encoded JSON
//!   `{agentId, latestRootBlobId, name, createdAt, mode}`.
//! - `blobs(id, data)`: `id` is a sha256; `data` is either a message JSON
//!   (`{role, content, ...}`), a large file body, or a protobuf "root" node.
//! - The root blob (`meta.latestRootBlobId`) is a protobuf carrying an ORDERED
//!   list of message blob ids as repeated length-delimited field #1 entries
//!   (`0a 20 <32-byte id> …`). Walking that list in order reconstructs the
//!   conversation; the JSON blobs themselves carry no ordering.
//!
//! Because blobs carry no per-message timestamp (only the session-level
//! `meta.createdAt`), we synthesize monotonic timestamps from `createdAt +
//! sequence` so the shared turn-selection logic still works.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{Connection, OpenFlags};
use serde_json::{json, Value};

use crate::{
    assistant_message_chunk, thinking_chunk, tool_call_chunk, user_message_chunk, ActivityChunk,
    ImportedToolCall, NativeSourceSession, SourceProfile, FUNCTION_EDIT_FILE,
    FUNCTION_RUN_COMMAND_LINE,
};

use super::jsonl::normalize_title;

const CURSOR_AGENT_SOURCE_ID: &str = "cursor_agent";
const CURSOR_AGENT_PARSER_VERSION: &str = "cursor-agent-store-v1";
const CURSOR_AGENT_PROVIDER_SLUG: &str = "cursor-agent";
const STORE_DB_FILE: &str = "store.db";

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<&str>,
) -> Result<Vec<NativeSourceSession>> {
    let root = chats_root(profile)?;
    let since_time = crate::since_to_system_time(since);
    let app_id = profile
        .app_id
        .clone()
        .unwrap_or_else(|| CURSOR_AGENT_SOURCE_ID.to_string());

    let mut store_paths = Vec::new();
    collect_store_dbs(&root, &mut store_paths)?;

    let mut sessions = Vec::new();
    for path in store_paths {
        // Incremental skip: a store.db whose mtime is at/under the watermark
        // cannot have changed since the last index.
        if let Some(since) = since_time {
            let unchanged = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .map(|mtime| mtime <= since)
                .unwrap_or(false);
            if unchanged {
                continue;
            }
        }
        match session_from_store(&path, &app_id) {
            Ok(Some(session)) => sessions.push(session),
            Ok(None) => {}
            // A single unreadable/locked store must not abort the whole scan.
            Err(_) => continue,
        }
    }

    sessions.sort_by_key(|session| std::cmp::Reverse(session.modified_at));
    if let Some(limit) = limit {
        sessions.truncate(limit);
    }
    Ok(sessions)
}

pub(super) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    let path = source_path.ok_or_else(|| {
        anyhow!("cursor-agent source path missing for session: {external_session_id}")
    })?;
    let connection = open_store_db(path)?;
    let meta = read_meta(&connection)?;
    let Some(root_id) = meta.latest_root_blob_id.as_deref() else {
        return Ok(Vec::new());
    };
    let message_ids = ordered_message_ids(&connection, root_id)?;
    let base_ms = meta.created_at_ms.unwrap_or(0);

    let mut chunks = Vec::new();
    let mut sequence = 0_usize;
    for blob_id in message_ids {
        let Some(message) = read_json_blob(&connection, &blob_id)? else {
            continue;
        };
        let Some(role) = message.get("role").and_then(Value::as_str) else {
            continue;
        };
        let created_at = synthetic_timestamp(base_ms, sequence);
        match role {
            "user" => {
                if let Some(text) = user_text(&message) {
                    let chunk = user_message_chunk(
                        external_session_id,
                        CURSOR_AGENT_PROVIDER_SLUG,
                        sequence,
                        &created_at,
                        &text,
                    );
                    chunks.push(chunk);
                    sequence += 1;
                }
            }
            "assistant" => {
                // One assistant blob can carry reasoning, text, and tool calls;
                // emit each as its own chunk in declaration order.
                for chunk in
                    assistant_chunks(external_session_id, &message, &created_at, &mut sequence)
                {
                    chunks.push(chunk);
                }
            }
            "tool" => {
                for chunk in
                    tool_result_chunks(external_session_id, &message, &created_at, &mut sequence)
                {
                    chunks.push(chunk);
                }
            }
            _ => {}
        }
    }
    Ok(chunks)
}

// --- metadata -------------------------------------------------------------

struct StoreMeta {
    agent_id: Option<String>,
    latest_root_blob_id: Option<String>,
    name: Option<String>,
    created_at_ms: Option<u64>,
}

fn read_meta(connection: &Connection) -> Result<StoreMeta> {
    let raw: Option<String> = connection
        .query_row("SELECT value FROM meta WHERE key = '0'", [], |row| {
            row.get(0)
        })
        .ok();
    let Some(raw) = raw else {
        return Ok(StoreMeta {
            agent_id: None,
            latest_root_blob_id: None,
            name: None,
            created_at_ms: None,
        });
    };
    // The meta value is hex-encoded JSON text.
    let json_text = hex_decode(&raw)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .unwrap_or(raw);
    let value: Value =
        serde_json::from_str(&json_text).context("failed to parse cursor-agent meta JSON")?;
    Ok(StoreMeta {
        agent_id: value
            .get("agentId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        latest_root_blob_id: value
            .get("latestRootBlobId")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        name: value
            .get("name")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned),
        created_at_ms: value.get("createdAt").and_then(Value::as_u64),
    })
}

fn session_from_store(path: &Path, app_id: &str) -> Result<Option<NativeSourceSession>> {
    let connection = open_store_db(path)?;
    let meta = read_meta(&connection)?;
    let Some(root_id) = meta.latest_root_blob_id.as_deref() else {
        return Ok(None);
    };
    // External id is the agentId; fall back to the parent directory name (the
    // agent dir) so the id is stable even if meta is sparse.
    let external_session_id = meta
        .agent_id
        .clone()
        .or_else(|| {
            path.parent()
                .and_then(|parent| parent.file_name())
                .and_then(|name| name.to_str())
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| anyhow!("cursor-agent store missing agentId: {}", path.display()))?;

    let message_ids = ordered_message_ids(&connection, root_id)?;
    let mut impact = BTreeSet::new();
    let mut repo_path: Option<String> = None;
    let mut first_user_text: Option<String> = None;
    for blob_id in &message_ids {
        let Some(message) = read_json_blob(&connection, blob_id)? else {
            continue;
        };
        match message.get("role").and_then(Value::as_str) {
            Some("user") => {
                if let Some(text) = user_text(&message) {
                    if repo_path.is_none() {
                        repo_path = workspace_path_from_user_info(&text);
                    }
                    if first_user_text.is_none() {
                        if let Some(query) = user_query(&text) {
                            first_user_text = Some(query);
                        }
                    }
                }
            }
            Some("assistant") => collect_assistant_edits(&message, &mut impact),
            _ => {}
        }
    }

    let file_metadata = std::fs::metadata(path).with_context(|| {
        format!(
            "failed to read cursor-agent store metadata for {}",
            path.display()
        )
    })?;
    let created_at = meta
        .created_at_ms
        .map(|ms| UNIX_EPOCH + Duration::from_millis(ms));
    let title = meta
        .name
        .as_deref()
        .filter(|name| !name.is_empty() && *name != "New Agent")
        .map(ToOwned::to_owned)
        .or(first_user_text)
        .map(normalize_title)
        .or_else(|| meta.name.clone());

    let touched_files: Vec<String> = impact.into_iter().collect();
    let files_changed = (!touched_files.is_empty()).then_some(touched_files.len() as u64);

    Ok(Some(NativeSourceSession {
        external_session_id,
        source_app_id: app_id.to_string(),
        title,
        path: path.to_path_buf(),
        size_bytes: file_metadata.len(),
        modified_at: file_metadata.modified().ok(),
        parser_version: CURSOR_AGENT_PARSER_VERSION.to_string(),
        session_created_at: created_at,
        session_updated_at: file_metadata.modified().ok(),
        model: None,
        input_tokens: None,
        output_tokens: None,
        repo_path: repo_path.clone().map(PathBuf::from),
        branch: None,
        files_changed,
        lines_added: None,
        lines_removed: None,
        touched_files,
        listable: true,
        metadata_json: None,
        cwd: repo_path.map(PathBuf::from),
        liveness: crate::Liveness::Unknown,
        last_activity: file_metadata.modified().ok(),
    }))
}

// --- chunk builders -------------------------------------------------------

fn assistant_chunks(
    session_id: &str,
    message: &Value,
    created_at: &str,
    sequence: &mut usize,
) -> Vec<ActivityChunk> {
    let mut chunks = Vec::new();
    let content = message.get("content");
    // Plain string content → a single assistant message.
    if let Some(text) = content.and_then(Value::as_str) {
        if !text.trim().is_empty() {
            chunks.push(assistant_message_chunk(
                session_id,
                CURSOR_AGENT_PROVIDER_SLUG,
                *sequence,
                created_at,
                text,
            ));
            *sequence += 1;
        }
        return chunks;
    }
    let Some(parts) = content.and_then(Value::as_array) else {
        return chunks;
    };
    for part in parts {
        match part.get("type").and_then(Value::as_str) {
            Some("text") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        chunks.push(assistant_message_chunk(
                            session_id,
                            CURSOR_AGENT_PROVIDER_SLUG,
                            *sequence,
                            created_at,
                            text,
                        ));
                        *sequence += 1;
                    }
                }
            }
            Some("reasoning") => {
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    if !text.trim().is_empty() {
                        chunks.push(thinking_chunk(
                            session_id,
                            CURSOR_AGENT_PROVIDER_SLUG,
                            *sequence,
                            created_at,
                            text,
                        ));
                        *sequence += 1;
                    }
                }
            }
            Some("tool-call") => {
                if let Some(call) = tool_call_from_part(part, created_at) {
                    let chunk = tool_call_chunk(
                        session_id,
                        CURSOR_AGENT_PROVIDER_SLUG,
                        *sequence,
                        &call,
                        "",
                    );
                    chunks.push(chunk);
                    *sequence += 1;
                }
            }
            _ => {}
        }
    }
    chunks
}

fn tool_result_chunks(
    session_id: &str,
    message: &Value,
    created_at: &str,
    sequence: &mut usize,
) -> Vec<ActivityChunk> {
    let mut chunks = Vec::new();
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return chunks;
    };
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("tool-result") {
            continue;
        }
        let tool_name = part
            .get("toolName")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let call_id = part
            .get("toolCallId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let output = tool_result_text(part);
        let (canonical_name, _) = normalize_cursor_agent_tool(tool_name, json!({}));
        // A result with no preceding call (rare) still surfaces as a tool chunk
        // so its output is not lost; args are empty in that case.
        let call = ImportedToolCall {
            call_id: call_id.to_string(),
            raw_name: tool_name.to_string(),
            canonical_name,
            args: json!({}),
            created_at: created_at.to_string(),
        };
        let chunk = tool_call_chunk(
            session_id,
            CURSOR_AGENT_PROVIDER_SLUG,
            *sequence,
            &call,
            &output,
        );
        chunks.push(chunk);
        *sequence += 1;
    }
    chunks
}

fn tool_call_from_part(part: &Value, created_at: &str) -> Option<ImportedToolCall> {
    let raw_name = part.get("toolName").and_then(Value::as_str)?.to_string();
    let call_id = part
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let args = part.get("args").cloned().unwrap_or_else(|| json!({}));
    let (canonical_name, args) = normalize_cursor_agent_tool(&raw_name, args);
    Some(ImportedToolCall {
        call_id,
        raw_name,
        canonical_name,
        args,
        created_at: created_at.to_string(),
    })
}

/// Maps cursor-agent (Claude-style) tool names onto Brick's canonical function
/// names where there is a clean equivalent, so downstream consumers see the same
/// `run_command_line` / `edit_file_by_replace` vocabulary as other sources.
fn normalize_cursor_agent_tool(raw_name: &str, args: Value) -> (String, Value) {
    match raw_name {
        "Bash" | "run_terminal_command" | "Shell" => {
            let command = args
                .get("command")
                .and_then(Value::as_str)
                .or_else(|| args.get("cmd").and_then(Value::as_str))
                .unwrap_or_default()
                .to_string();
            (
                FUNCTION_RUN_COMMAND_LINE.to_string(),
                json!({ "command": command, "cmd": command }),
            )
        }
        "Edit" | "Write" | "MultiEdit" | "create_file" | "edit_file" => {
            (FUNCTION_EDIT_FILE.to_string(), args)
        }
        _ => (raw_name.to_string(), args),
    }
}

fn collect_assistant_edits(message: &Value, impact: &mut BTreeSet<String>) {
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return;
    };
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("tool-call") {
            continue;
        }
        let tool_name = part.get("toolName").and_then(Value::as_str).unwrap_or("");
        let args = part.get("args");
        match tool_name {
            "Edit" | "Write" | "MultiEdit" | "create_file" | "edit_file" => {
                if let Some(path) = args
                    .and_then(|a| {
                        a.get("path")
                            .or_else(|| a.get("file_path"))
                            .or_else(|| a.get("target_file"))
                    })
                    .and_then(Value::as_str)
                    .filter(|path| !path.is_empty())
                {
                    impact.insert(path.to_string());
                }
            }
            "Bash" | "run_terminal_command" | "Shell" => {
                if let Some(command) = args
                    .and_then(|a| a.get("command").or_else(|| a.get("cmd")))
                    .and_then(Value::as_str)
                {
                    for file in super::shell_edits::shell_edit_targets(command) {
                        impact.insert(file);
                    }
                }
            }
            _ => {}
        }
    }
}

// --- content extraction ---------------------------------------------------

fn user_text(message: &Value) -> Option<String> {
    let content = message.get("content")?;
    if let Some(text) = content.as_str() {
        return Some(text.to_string());
    }
    let parts = content.as_array()?;
    let joined = parts
        .iter()
        .filter_map(|part| part.get("text").and_then(Value::as_str))
        .collect::<Vec<_>>()
        .join("\n");
    (!joined.is_empty()).then_some(joined)
}

fn tool_result_text(part: &Value) -> String {
    if let Some(text) = part.get("result").and_then(Value::as_str) {
        return text.to_string();
    }
    // Some results carry an `experimental_content` array of `{type,text}` parts.
    if let Some(parts) = part.get("experimental_content").and_then(Value::as_array) {
        return parts
            .iter()
            .filter_map(|p| p.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n");
    }
    match part.get("result") {
        Some(value) if !value.is_null() => value.to_string(),
        _ => String::new(),
    }
}

/// Extracts the workspace path from a cursor-agent `<user_info>` block whose body
/// contains a `Workspace Path: <path>` line.
fn workspace_path_from_user_info(text: &str) -> Option<String> {
    for line in text.lines() {
        if let Some(rest) = line.trim().strip_prefix("Workspace Path:") {
            let path = rest.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

/// Pulls the human prompt out of a `<user_query>…</user_query>` wrapper.
fn user_query(text: &str) -> Option<String> {
    let start = text.find("<user_query>")? + "<user_query>".len();
    let end = text[start..].find("</user_query>")? + start;
    let query = text[start..end].trim();
    (!query.is_empty()).then(|| query.to_string())
}

// --- DAG / blob primitives ------------------------------------------------

fn ordered_message_ids(connection: &Connection, root_id: &str) -> Result<Vec<String>> {
    let Some(root) = read_raw_blob(connection, root_id)? else {
        return Ok(Vec::new());
    };
    Ok(parse_root_blob_ids(&root))
}

/// Decodes the root blob's protobuf: repeated length-delimited field #1 entries,
/// each a 32-byte blob id. Other fields/wire-types are skipped. This is a
/// minimal, dependency-free reader for exactly the shape cursor-agent emits.
fn parse_root_blob_ids(bytes: &[u8]) -> Vec<String> {
    let mut ids = Vec::new();
    let mut index = 0usize;
    while index < bytes.len() {
        let tag = bytes[index];
        index += 1;
        let field = tag >> 3;
        let wire = tag & 0x07;
        match wire {
            0 => {
                // varint: advance past it
                while index < bytes.len() && bytes[index] & 0x80 != 0 {
                    index += 1;
                }
                index += 1;
            }
            2 => {
                let Some((len, consumed)) = read_varint(&bytes[index..]) else {
                    break;
                };
                index += consumed;
                let end = index.saturating_add(len as usize);
                if end > bytes.len() {
                    break;
                }
                let chunk = &bytes[index..end];
                index = end;
                if field == 1 && chunk.len() == 32 {
                    ids.push(hex_encode(chunk));
                }
            }
            // Fixed64 / Fixed32 are not used by this format; stop on anything
            // unexpected rather than risk misaligned reads.
            _ => break,
        }
    }
    ids
}

fn read_varint(bytes: &[u8]) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    for (offset, byte) in bytes.iter().enumerate() {
        value |= ((byte & 0x7f) as u64) << shift;
        if byte & 0x80 == 0 {
            return Some((value, offset + 1));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
    None
}

fn read_raw_blob(connection: &Connection, blob_id: &str) -> Result<Option<Vec<u8>>> {
    let data: Option<Vec<u8>> = connection
        .query_row("SELECT data FROM blobs WHERE id = ?1", [blob_id], |row| {
            row.get(0)
        })
        .ok();
    Ok(data)
}

fn read_json_blob(connection: &Connection, blob_id: &str) -> Result<Option<Value>> {
    let Some(raw) = read_raw_blob(connection, blob_id)? else {
        return Ok(None);
    };
    Ok(serde_json::from_slice::<Value>(&raw).ok())
}

fn synthetic_timestamp(base_ms: u64, sequence: usize) -> String {
    let millis = base_ms.saturating_add(sequence as u64 * 1000);
    let time = UNIX_EPOCH + Duration::from_millis(millis);
    let datetime: DateTime<Utc> = time.into();
    datetime.to_rfc3339_opts(SecondsFormat::Millis, true)
}

// --- filesystem -----------------------------------------------------------

fn chats_root(profile: &SourceProfile) -> Result<PathBuf> {
    if let Some(path) = &profile.session_log_path {
        return Ok(path.clone());
    }
    if let Some(path) = &profile.evidence_root {
        return Ok(path.clone());
    }
    Err(anyhow!(
        "cursor-agent source requires session_log_path pointing at ~/.cursor/chats"
    ))
}

/// Walks `root` collecting every `store.db` file (one per session).
fn collect_store_dbs(root: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = match std::fs::read_dir(&directory) {
            Ok(entries) => entries,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.file_name().and_then(|name| name.to_str()) == Some(STORE_DB_FILE) {
                out.push(path);
            }
        }
    }
    Ok(())
}

fn open_store_db(path: &Path) -> Result<Connection> {
    Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
    .with_context(|| format!("failed to open cursor-agent store at {}", path.display()))
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        s.push_str(&format!("{byte:02x}"));
    }
    s
}

fn hex_decode(text: &str) -> Option<Vec<u8>> {
    let text = text.trim();
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let mut out = Vec::with_capacity(text.len() / 2);
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        let hi = (bytes[index] as char).to_digit(16)?;
        let lo = (bytes[index + 1] as char).to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
        index += 2;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::*;
    use crate::{
        ACTION_TYPE_ASSISTANT, ACTION_TYPE_RAW, ACTION_TYPE_THINKING, ACTION_TYPE_TOOL_CALL,
        FUNCTION_ASSISTANT, FUNCTION_USER_MESSAGE,
    };

    fn temp_store(name: &str) -> PathBuf {
        // Mirror the real layout: <chats_root>/<workspaceHash>/<agentId>/store.db
        // so list_sessions' directory walk starts at a dedicated chats root and
        // never picks up sibling tests' stores from the shared temp dir.
        let chats_root = std::env::temp_dir().join(format!(
            "brick-cursor-agent-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let agent_dir = chats_root.join("workspacehash").join("agent-123");
        std::fs::create_dir_all(&agent_dir).expect("create store dir");
        agent_dir.join(STORE_DB_FILE)
    }

    fn sha_id(seed: u8) -> Vec<u8> {
        vec![seed; 32]
    }

    /// Builds a store.db with the given ordered (blob_id, message_json) pairs and
    /// a root blob listing those ids in order.
    fn seed_store(path: &Path, name: &str, created_at_ms: u64, messages: &[(Vec<u8>, Value)]) {
        let connection = Connection::open(path).expect("open store db");
        connection
            .execute_batch(
                "CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT);
                 CREATE TABLE blobs (id TEXT PRIMARY KEY, data BLOB);",
            )
            .expect("create tables");

        // root blob: 0a 20 <id> repeated
        let mut root = Vec::new();
        for (id, _) in messages {
            root.push(0x0a);
            root.push(0x20);
            root.extend_from_slice(id);
        }
        let root_id = sha_id(0xAA);
        connection
            .execute(
                "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
                rusqlite::params![hex_encode(&root_id), root],
            )
            .expect("insert root blob");

        for (id, message) in messages {
            connection
                .execute(
                    "INSERT INTO blobs (id, data) VALUES (?1, ?2)",
                    rusqlite::params![hex_encode(id), message.to_string().into_bytes()],
                )
                .expect("insert message blob");
        }

        let meta = json!({
            "agentId": "agent-123",
            "latestRootBlobId": hex_encode(&root_id),
            "name": name,
            "createdAt": created_at_ms,
            "mode": "default",
        });
        let meta_hex = hex_encode(meta.to_string().as_bytes());
        connection
            .execute("INSERT INTO meta (key, value) VALUES ('0', ?1)", [meta_hex])
            .expect("insert meta");
    }

    fn profile(root: PathBuf) -> SourceProfile {
        SourceProfile {
            name: CURSOR_AGENT_SOURCE_ID.to_string(),
            app_id: Some(CURSOR_AGENT_SOURCE_ID.to_string()),
            actor_id: None,
            actor_type: None,
            store_root: None,
            session_db_path: None,
            session_log_path: Some(root),
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        }
    }

    #[test]
    fn reconstructs_conversation_in_root_order() {
        let path = temp_store("chunks");
        seed_store(
            &path,
            "New Agent",
            1_770_000_000_000,
            &[
                (
                    sha_id(1),
                    json!({"role": "system", "content": "You are an AI"}),
                ),
                (
                    sha_id(2),
                    json!({"role": "user", "content": "<user_query>\nfix the bug\n</user_query>"}),
                ),
                (
                    sha_id(3),
                    json!({"role": "assistant", "content": [
                        {"type": "reasoning", "text": "Let me think"},
                        {"type": "text", "text": "I'll fix it"},
                        {"type": "tool-call", "toolCallId": "c1", "toolName": "Bash",
                         "args": {"command": "cargo test"}}
                    ]}),
                ),
                (
                    sha_id(4),
                    json!({"role": "tool", "content": [
                        {"type": "tool-result", "toolCallId": "c1", "toolName": "Bash",
                         "result": "ok"}
                    ]}),
                ),
            ],
        );

        let chunks = format_chunks("agent-123", Some(&path)).expect("format chunks");

        // system has no chunk; user, reasoning, text, tool-call, tool-result.
        assert_eq!(chunks.len(), 5);
        assert_eq!(chunks[0].action_type, ACTION_TYPE_RAW);
        assert_eq!(chunks[0].function, FUNCTION_USER_MESSAGE);
        assert_eq!(chunks[1].action_type, ACTION_TYPE_THINKING);
        assert_eq!(chunks[2].action_type, ACTION_TYPE_ASSISTANT);
        assert_eq!(chunks[2].function, FUNCTION_ASSISTANT);
        assert_eq!(chunks[2].result["content"], "I'll fix it");
        assert_eq!(chunks[3].action_type, ACTION_TYPE_TOOL_CALL);
        assert_eq!(chunks[3].function, FUNCTION_RUN_COMMAND_LINE);
        assert_eq!(chunks[3].args["command"], "cargo test");
        assert_eq!(chunks[4].action_type, ACTION_TYPE_TOOL_CALL);
        assert_eq!(chunks[4].result["output"], "ok");
    }

    #[test]
    fn lists_session_metadata_from_store() {
        let path = temp_store("metadata");
        seed_store(
            &path,
            "New Agent",
            1_770_000_000_000,
            &[
                (
                    sha_id(1),
                    json!({"role": "user", "content":
                        "<user_info>\nWorkspace Path: /repo/project\n</user_info>\n\n<user_query>\nhelp me\n</user_query>"}),
                ),
                (
                    sha_id(2),
                    json!({"role": "assistant", "content": [
                        {"type": "tool-call", "toolCallId": "c1", "toolName": "Edit",
                         "args": {"path": "/repo/project/src/lib.rs"}}
                    ]}),
                ),
            ],
        );
        let root = path
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();

        let sessions = list_sessions(&profile(root), Some(10), None).expect("list sessions");

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.external_session_id, "agent-123");
        assert_eq!(session.title.as_deref(), Some("help me"));
        assert_eq!(
            session.repo_path.as_deref(),
            Some(Path::new("/repo/project"))
        );
        assert_eq!(
            session.touched_files,
            vec!["/repo/project/src/lib.rs".to_string()]
        );
        assert_eq!(session.files_changed, Some(1));
        assert_eq!(session.parser_version, CURSOR_AGENT_PARSER_VERSION);
    }

    #[test]
    fn turn_final_message_recovers_from_store() {
        let path = temp_store("turnfinal");
        seed_store(
            &path,
            "New Agent",
            1_770_000_000_000,
            &[
                (sha_id(1), json!({"role": "user", "content": "do it"})),
                (
                    sha_id(2),
                    json!({"role": "assistant", "content": [
                        {"type": "text", "text": "Done — serialized the refresh to fix the race."}
                    ]}),
                ),
            ],
        );

        let chunks = format_chunks("agent-123", Some(&path)).expect("format chunks");
        let note = crate::select_turn_final_message(&chunks, "2999-01-01T00:00:00Z");
        assert_eq!(
            note.as_deref(),
            Some("Done — serialized the refresh to fix the race.")
        );
    }

    #[test]
    fn parses_root_blob_protobuf_ids() {
        let id_a = sha_id(7);
        let id_b = sha_id(9);
        let mut root = Vec::new();
        root.push(0x0a);
        root.push(0x20);
        root.extend_from_slice(&id_a);
        root.push(0x0a);
        root.push(0x20);
        root.extend_from_slice(&id_b);

        let ids = parse_root_blob_ids(&root);
        assert_eq!(ids, vec![hex_encode(&id_a), hex_encode(&id_b)]);
    }
}
