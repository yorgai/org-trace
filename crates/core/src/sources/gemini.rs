//! Gemini CLI native history source provider.
//!
//! The Gemini CLI persists chat history under `~/.gemini/tmp/<projectHash>/`:
//! a flat `logs.json` (slash-command level) and richer
//! `chats/session-<timestamp>-<id>.json` files. Each chat file is a JSON object
//! `{ sessionId, projectHash, startTime, lastUpdated, messages: [...] }` where a
//! message of `type: "gemini"` may carry a `toolCalls` array. File edits surface
//! as `write_file` / `replace` tool calls (target in `args.file_path`) and shell
//! writes inside `run_shell_command` (`args.command`). This provider reads those
//! files read-only and projects each chat session into the shared
//! `NativeSourceSession`, populating `touched_files` so `file-session-blame`
//! works against Gemini history.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{
    list_file_source_sessions_with_filter, NativeSessionMetadata, NativeSourceSession,
    SourceProfile,
};

use super::jsonl::truncate_title;
use super::shell_edits::shell_edit_targets;

const GEMINI_PARSER_VERSION: &str = "gemini-chat-json-v1";
const GEMINI_PROVIDER_SLUG: &str = "gemini";

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<&str>,
) -> Result<Vec<NativeSourceSession>> {
    list_file_source_sessions_with_filter(
        profile,
        limit,
        crate::since_to_system_time(since),
        extract_chat_metadata,
        is_gemini_chat_file,
    )
}

pub(super) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<crate::ActivityChunk>> {
    let path = source_path.ok_or_else(|| {
        anyhow::anyhow!("Gemini source path missing for session: {external_session_id}")
    })?;
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read Gemini chat file {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse Gemini chat JSON {}", path.display()))?;

    let mut chunks = Vec::new();
    let mut sequence = 0_usize;
    let Some(messages) = value.get("messages").and_then(serde_json::Value::as_array) else {
        return Ok(chunks);
    };
    for message in messages {
        let created_at = message
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let kind = message.get("type").and_then(serde_json::Value::as_str);
        match kind {
            Some("user") => {
                if let Some(text) = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    chunks.push(crate::user_message_chunk(
                        external_session_id,
                        GEMINI_PROVIDER_SLUG,
                        sequence,
                        created_at,
                        text,
                    ));
                    sequence += 1;
                }
            }
            Some("gemini") => {
                if let Some(text) = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .map(str::trim)
                    .filter(|text| !text.is_empty())
                {
                    chunks.push(crate::assistant_message_chunk(
                        external_session_id,
                        GEMINI_PROVIDER_SLUG,
                        sequence,
                        created_at,
                        text,
                    ));
                    sequence += 1;
                }
                for tool_call in message
                    .get("toolCalls")
                    .and_then(serde_json::Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let name = tool_call
                        .get("name")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("tool")
                        .to_string();
                    let call = crate::ImportedToolCall {
                        call_id: format!("{external_session_id}-{sequence}"),
                        raw_name: name.clone(),
                        canonical_name: name,
                        args: tool_call
                            .get("args")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        created_at: created_at.to_string(),
                    };
                    chunks.push(crate::tool_call_chunk(
                        external_session_id,
                        GEMINI_PROVIDER_SLUG,
                        sequence,
                        &call,
                        "",
                    ));
                    sequence += 1;
                }
            }
            _ => {}
        }
    }
    Ok(chunks)
}

/// Matches Gemini chat session files: `.../chats/session-*.json`.
fn is_gemini_chat_file(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_none_or(|extension| !extension.eq_ignore_ascii_case("json"))
    {
        return false;
    }
    let in_chats_dir = path
        .parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        == Some("chats");
    let named_session = path
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("session-"));
    in_chats_dir && named_session
}

fn extract_chat_metadata(path: &Path) -> Result<NativeSessionMetadata> {
    if !is_gemini_chat_file(path) {
        return Ok(NativeSessionMetadata::default());
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read Gemini chat file {}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse Gemini chat JSON {}", path.display()))?;

    let mut metadata = NativeSessionMetadata {
        parser_version: Some(GEMINI_PARSER_VERSION.to_string()),
        ..NativeSessionMetadata::default()
    };
    metadata.session_created_at = value
        .get("startTime")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_rfc3339);
    metadata.session_updated_at = value
        .get("lastUpdated")
        .and_then(serde_json::Value::as_str)
        .and_then(parse_rfc3339);

    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut saw_tokens = false;
    let mut touched_files: BTreeSet<String> = BTreeSet::new();
    let mut repo_path: Option<String> = None;

    if let Some(messages) = value.get("messages").and_then(serde_json::Value::as_array) {
        for message in messages {
            if metadata.title.is_none()
                && message.get("type").and_then(serde_json::Value::as_str) == Some("user")
            {
                metadata.title = message
                    .get("content")
                    .and_then(serde_json::Value::as_str)
                    .filter(|text| !text.is_empty())
                    .map(|text| truncate_title(text.to_string()));
            }
            if metadata.model.is_none() {
                if let Some(model) = message.get("model").and_then(serde_json::Value::as_str) {
                    metadata.model = Some(model.to_string());
                }
            }
            if let Some(tokens) = message.get("tokens") {
                let current_input = token_value(tokens, "input");
                let current_output = token_value(tokens, "output");
                if current_input > 0 || current_output > 0 {
                    input_tokens = input_tokens.saturating_add(current_input);
                    output_tokens = output_tokens.saturating_add(current_output);
                    saw_tokens = true;
                }
            }
            for tool_call in message
                .get("toolCalls")
                .and_then(serde_json::Value::as_array)
                .into_iter()
                .flatten()
            {
                collect_tool_call_edits(tool_call, &mut touched_files, &mut repo_path);
            }
        }
    }

    metadata.input_tokens = saw_tokens.then_some(input_tokens);
    metadata.output_tokens = saw_tokens.then_some(output_tokens);
    metadata.repo_path = repo_path.map(PathBuf::from);
    if !touched_files.is_empty() {
        metadata.files_changed = Some(touched_files.len() as u64);
        metadata.touched_files = touched_files.into_iter().collect();
    }
    Ok(metadata)
}

/// Extracts a touched file path (and an inferred repo root) from one tool call.
fn collect_tool_call_edits(
    tool_call: &serde_json::Value,
    touched_files: &mut BTreeSet<String>,
    repo_path: &mut Option<String>,
) {
    let name = tool_call
        .get("name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let args = tool_call.get("args");
    match name {
        "write_file" | "replace" | "edit" => {
            if let Some(file_path) = args
                .and_then(|args| args.get("file_path"))
                .and_then(serde_json::Value::as_str)
                .filter(|path| !path.is_empty())
            {
                touched_files.insert(file_path.to_string());
            }
        }
        "run_shell_command" => {
            if let Some(command) = args
                .and_then(|args| args.get("command"))
                .and_then(serde_json::Value::as_str)
            {
                for file in shell_edit_targets(command) {
                    touched_files.insert(file);
                }
            }
            // `run_shell_command` may carry a `directory` arg naming the repo.
            if repo_path.is_none() {
                if let Some(directory) = args
                    .and_then(|args| args.get("directory"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|path| !path.is_empty())
                {
                    *repo_path = Some(directory.to_string());
                }
            }
        }
        _ => {}
    }
}

fn token_value(tokens: &serde_json::Value, key: &str) -> u64 {
    tokens
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0)
}

fn parse_rfc3339(value: &str) -> Option<std::time::SystemTime> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|dt| std::time::SystemTime::from(dt.with_timezone(&chrono::Utc)))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use brick_protocol::ActorType;

    fn temp_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-gemini-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create gemini test root");
        path
    }

    fn profile(session_log_path: PathBuf) -> SourceProfile {
        SourceProfile {
            name: "gemini".to_string(),
            app_id: Some("gemini".to_string()),
            actor_id: None,
            actor_type: Some(ActorType::Agent),
            store_root: None,
            session_db_path: None,
            session_log_path: Some(session_log_path),
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        }
    }

    #[test]
    fn extracts_gemini_chat_touched_files() {
        let root = temp_root("chat");
        let chats_dir = root.join("projecthash").join("chats");
        fs::create_dir_all(&chats_dir).expect("create chats dir");
        let session = serde_json::json!({
            "sessionId": "abc-123",
            "projectHash": "projecthash",
            "startTime": "2025-12-12T09:20:57.219Z",
            "lastUpdated": "2025-12-12T09:40:38.334Z",
            "messages": [
                { "type": "user", "content": "Write a hello script" },
                {
                    "type": "gemini",
                    "content": "Done",
                    "model": "gemini-2.5-flash",
                    "tokens": { "input": 8096, "output": 29 },
                    "toolCalls": [
                        { "name": "write_file", "args": { "file_path": "hello.py", "content": "print(1)" } },
                        { "name": "replace", "args": { "file_path": "/repo/src/lib.rs", "instruction": "x" } },
                        { "name": "run_shell_command", "args": { "command": "echo done > notes.txt" } },
                        { "name": "read_file", "args": { "file_path": "ignored.py" } }
                    ]
                }
            ]
        });
        fs::write(
            chats_dir.join("session-2025-12-12T09-20-abc.json"),
            session.to_string(),
        )
        .expect("write gemini chat file");

        let sessions = list_sessions(&profile(root), Some(10), None).expect("list gemini sessions");

        assert_eq!(sessions.len(), 1);
        let session = &sessions[0];
        assert_eq!(session.title.as_deref(), Some("Write a hello script"));
        assert_eq!(session.model.as_deref(), Some("gemini-2.5-flash"));
        assert_eq!(session.input_tokens, Some(8096));
        assert_eq!(session.output_tokens, Some(29));
        assert_eq!(
            session.touched_files,
            vec![
                "/repo/src/lib.rs".to_string(),
                "hello.py".to_string(),
                "notes.txt".to_string(),
            ]
        );
        assert_eq!(session.files_changed, Some(3));
        assert_eq!(session.parser_version, GEMINI_PARSER_VERSION);
    }

    #[test]
    fn format_chunks_recovers_turn_final_rationale() {
        let root = temp_root("chunks");
        let chats_dir = root.join("projecthash").join("chats");
        fs::create_dir_all(&chats_dir).expect("create chats dir");
        let session = serde_json::json!({
            "sessionId": "s-rationale",
            "startTime": "2025-12-12T09:20:57.219Z",
            "lastUpdated": "2025-12-12T09:40:38.334Z",
            "messages": [
                { "type": "user", "content": "Make it faster", "timestamp": "2025-12-12T09:21:00Z" },
                {
                    "type": "gemini",
                    "content": "I cached the result to avoid the repeated scan.",
                    "timestamp": "2025-12-12T09:39:00Z",
                    "toolCalls": [
                        { "name": "write_file", "args": { "file_path": "cache.py" } }
                    ]
                }
            ]
        });
        let file = chats_dir.join("session-2025-12-12T09-20-rationale.json");
        fs::write(&file, session.to_string()).expect("write gemini chat");

        let chunks = format_chunks("s-rationale", Some(&file)).expect("render chunks");
        // user + assistant + tool_call = 3.
        assert_eq!(chunks.len(), 3);
        let final_msg = crate::select_turn_final_message(&chunks, "2025-12-12T09:40:38.334Z");
        assert_eq!(
            final_msg.as_deref(),
            Some("I cached the result to avoid the repeated scan.")
        );
    }
}
