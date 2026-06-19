use std::collections::HashMap;
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::{
    assistant_message_chunk, list_file_source_sessions_with_filter, thinking_chunk,
    tool_call_chunk, user_message_chunk, ActivityChunk, ImportedToolCall, NativeSessionMetadata,
    NativeSourceSession, SourceProfile, FUNCTION_EDIT_FILE, FUNCTION_RUN_COMMAND_LINE,
};

use super::jsonl::{
    read_jsonl_records, read_jsonl_values, set_first_path, set_first_string,
    set_first_string_value, text_from_value, token_value, truncate_title, update_session_times,
};

const CLAUDE_CODE_SOURCE_ID: &str = "claude_code";
const CLAUDE_CODE_JSONL_PARSER_VERSION: &str = "claude-code-jsonl-v3";
const CLAUDE_CODE_PROVIDER_SLUG: &str = "claudecode";

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    let mut sessions = list_file_source_sessions_with_filter(
        profile,
        limit,
        extract_jsonl_metadata,
        is_claude_transcript_file,
    )?;
    attach_subagent_ids_to_parents(&mut sessions);
    Ok(sessions)
}

pub(super) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    let path = source_path.ok_or_else(|| {
        anyhow!("Claude Code source path missing for session: {external_session_id}")
    })?;
    let records = read_jsonl_records(path)?;
    let mut chunks = Vec::new();
    let mut pending_tool_calls = HashMap::<String, (ImportedToolCall, u64)>::new();
    let mut sequence = 0_usize;

    for record in records {
        let value = record.value;
        let created_at = value
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let Some(message) = value.get("message") else {
            continue;
        };
        match value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "user" => {
                if let Some((call_id, output)) = claude_tool_result_text(message.get("content")) {
                    if let Some((call, line_number)) = pending_tool_calls.remove(&call_id) {
                        let mut chunk = tool_call_chunk(
                            external_session_id,
                            CLAUDE_CODE_PROVIDER_SLUG,
                            sequence,
                            &call,
                            &output,
                        );
                        chunk.set_source_pointer(
                            CLAUDE_CODE_SOURCE_ID,
                            path,
                            None,
                            Some(line_number),
                            Some(&call.call_id),
                            None,
                        );
                        chunks.push(chunk);
                        sequence += 1;
                    }
                } else if let Some(text) = message.get("content").and_then(text_from_value) {
                    let mut chunk = user_message_chunk(
                        external_session_id,
                        CLAUDE_CODE_PROVIDER_SLUG,
                        sequence,
                        created_at,
                        &text,
                    );
                    chunk.set_source_pointer(
                        CLAUDE_CODE_SOURCE_ID,
                        path,
                        None,
                        Some(record.line_number),
                        None,
                        None,
                    );
                    chunks.push(chunk);
                    sequence += 1;
                }
            }
            "assistant" => {
                for item in claude_content_items(message.get("content")) {
                    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
                        "text" => {
                            if let Some(text) = item.get("text").and_then(Value::as_str) {
                                let mut chunk = assistant_message_chunk(
                                    external_session_id,
                                    CLAUDE_CODE_PROVIDER_SLUG,
                                    sequence,
                                    created_at,
                                    text,
                                );
                                chunk.set_source_pointer(
                                    CLAUDE_CODE_SOURCE_ID,
                                    path,
                                    None,
                                    Some(record.line_number),
                                    None,
                                    item.get("id").and_then(Value::as_str),
                                );
                                chunks.push(chunk);
                                sequence += 1;
                            }
                        }
                        "thinking" => {
                            if let Some(text) = item.get("thinking").and_then(Value::as_str) {
                                let mut chunk = thinking_chunk(
                                    external_session_id,
                                    CLAUDE_CODE_PROVIDER_SLUG,
                                    sequence,
                                    created_at,
                                    text,
                                );
                                chunk.set_source_pointer(
                                    CLAUDE_CODE_SOURCE_ID,
                                    path,
                                    None,
                                    Some(record.line_number),
                                    None,
                                    item.get("id").and_then(Value::as_str),
                                );
                                chunks.push(chunk);
                                sequence += 1;
                            }
                        }
                        "tool_use" => {
                            if let Some(call) = claude_tool_call_from_item(item, created_at) {
                                pending_tool_calls
                                    .insert(call.call_id.clone(), (call, record.line_number));
                            }
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    for (call, line_number) in pending_tool_calls.into_values() {
        let mut chunk = tool_call_chunk(
            external_session_id,
            CLAUDE_CODE_PROVIDER_SLUG,
            sequence,
            &call,
            "",
        );
        chunk.set_source_pointer(
            CLAUDE_CODE_SOURCE_ID,
            path,
            None,
            Some(line_number),
            Some(&call.call_id),
            None,
        );
        chunks.push(chunk);
        sequence += 1;
    }
    Ok(chunks)
}

fn is_claude_transcript_file(path: &Path) -> bool {
    path.extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
}

fn attach_subagent_ids_to_parents(sessions: &mut [NativeSourceSession]) {
    let mut subagent_ids_by_parent = HashMap::<String, Vec<String>>::new();
    for session in sessions.iter() {
        let Some(metadata) = session.metadata_json.as_ref() else {
            continue;
        };
        if metadata.get("kind").and_then(Value::as_str) != Some("subagent") {
            continue;
        }
        let Some(parent_session_id) = metadata.get("parentSessionId").and_then(Value::as_str)
        else {
            continue;
        };
        subagent_ids_by_parent
            .entry(parent_session_id.to_string())
            .or_default()
            .push(session.external_session_id.clone());
    }
    for session in sessions.iter_mut() {
        let Some(subagent_ids) = subagent_ids_by_parent.remove(&session.external_session_id) else {
            continue;
        };
        let mut metadata = session.metadata_json.take().unwrap_or_else(|| json!({}));
        metadata["subagentSessionIds"] = json!(subagent_ids);
        session.metadata_json = Some(metadata);
    }
}

fn parent_session_id_from_subagent_path(path: &Path) -> Option<String> {
    let subagents_directory = path.parent()?;
    if subagents_directory
        .file_name()
        .and_then(|name| name.to_str())
        != Some("subagents")
    {
        return None;
    }
    subagents_directory
        .parent()
        .and_then(|parent_session_directory| parent_session_directory.file_name())
        .and_then(|name| name.to_str())
        .map(ToOwned::to_owned)
}

fn extract_jsonl_metadata(path: &Path) -> Result<NativeSessionMetadata> {
    if path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_none_or(|extension| !extension.eq_ignore_ascii_case("jsonl"))
    {
        return Ok(NativeSessionMetadata::default());
    }

    let lines = read_jsonl_values(path)?;
    let mut metadata = NativeSessionMetadata {
        parser_version: Some(CLAUDE_CODE_JSONL_PARSER_VERSION.to_string()),
        ..NativeSessionMetadata::default()
    };
    let mut input_tokens = 0_u64;
    let mut output_tokens = 0_u64;
    let mut saw_input_tokens = false;
    let mut saw_output_tokens = false;
    let mut saw_sidechain = parent_session_id_from_subagent_path(path).is_some();
    let mut parent_session_id = parent_session_id_from_subagent_path(path);
    let mut agent_id: Option<String> = None;
    let mut attribution_agent: Option<String> = None;
    let mut touched_files: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    for value in lines {
        update_session_times(&mut metadata, value.get("timestamp"));
        set_first_path(&mut metadata.repo_path, value.get("cwd"));
        set_first_string(&mut metadata.branch, value.get("gitBranch"));
        if value.get("isSidechain").and_then(Value::as_bool) == Some(true) {
            saw_sidechain = true;
        }
        set_first_string_value(
            &mut parent_session_id,
            value
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or_default(),
        );
        set_first_string(&mut agent_id, value.get("agentId"));
        set_first_string(&mut attribution_agent, value.get("attributionAgent"));

        let message = value.get("message");
        if metadata.title.is_none() && is_user_message(&value, message) {
            metadata.title = message
                .and_then(|message_value| message_value.get("content"))
                .and_then(text_from_value)
                .map(truncate_title);
        }

        if let Some(model) = message
            .and_then(|message_value| message_value.get("model"))
            .and_then(Value::as_str)
        {
            set_first_string_value(&mut metadata.model, model);
        }

        if let Some(usage) = message.and_then(|message_value| message_value.get("usage")) {
            let current_input_tokens = token_value(usage, "input_tokens")
                + token_value(usage, "cache_read_input_tokens")
                + token_value(usage, "cache_creation_input_tokens");
            let current_output_tokens = token_value(usage, "output_tokens");
            if current_input_tokens > 0 {
                input_tokens = input_tokens.saturating_add(current_input_tokens);
                saw_input_tokens = true;
            }
            if current_output_tokens > 0 {
                output_tokens = output_tokens.saturating_add(current_output_tokens);
                saw_output_tokens = true;
            }
        }

        // Attribute file edits: a `tool_use` content item for Edit/MultiEdit/
        // Write carries the target in `input.file_path`; Bash commands may write
        // files via redirects/heredocs/in-place edits.
        for item in claude_content_items(message.and_then(|message| message.get("content"))) {
            if item.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let name = item.get("name").and_then(Value::as_str).unwrap_or_default();
            let input = item.get("input");
            match name {
                "Edit" | "MultiEdit" | "Write" => {
                    if let Some(file_path) = input
                        .and_then(|input| input.get("file_path"))
                        .and_then(Value::as_str)
                        .filter(|path| !path.is_empty())
                    {
                        touched_files.insert(file_path.to_string());
                    }
                }
                "Bash" => {
                    if let Some(command) = input
                        .and_then(|input| input.get("command"))
                        .and_then(Value::as_str)
                    {
                        for file in crate::sources::shell_edits::shell_edit_targets(command) {
                            touched_files.insert(file);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    metadata.input_tokens = saw_input_tokens.then_some(input_tokens);
    metadata.output_tokens = saw_output_tokens.then_some(output_tokens);
    if !touched_files.is_empty() {
        metadata.files_changed = Some(touched_files.len() as u64);
        metadata.touched_files = touched_files.into_iter().collect();
    }
    if saw_sidechain {
        if let Some(parent_session_id) = parent_session_id.filter(|value| !value.is_empty()) {
            let subagent_session_id = path
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("session");
            metadata.listable = false;
            metadata.metadata_json = Some(json!({
                "kind": "subagent",
                "subagentSessionId": subagent_session_id,
                "parentSessionId": parent_session_id,
                "claudeAgentId": agent_id,
                "subagentType": attribution_agent,
            }));
        }
    }
    Ok(metadata)
}

fn is_user_message(value: &Value, message: Option<&Value>) -> bool {
    value.get("type").and_then(Value::as_str) == Some("user")
        || message.and_then(|message_value| message_value.get("role"))
            == Some(&Value::String("user".to_string()))
}

fn claude_tool_call_from_item(item: &Value, created_at: &str) -> Option<ImportedToolCall> {
    let call_id = item.get("id")?.as_str()?.to_string();
    let raw_name = item.get("name")?.as_str()?.to_string();
    let args = item.get("input").cloned().unwrap_or_else(|| json!({}));
    let (canonical_name, args) = normalize_claude_tool_call(&raw_name, args);
    Some(ImportedToolCall {
        call_id,
        raw_name,
        canonical_name,
        args,
        created_at: created_at.to_string(),
    })
}

fn normalize_claude_tool_call(raw_name: &str, args: Value) -> (String, Value) {
    match raw_name {
        "Bash" => (
            FUNCTION_RUN_COMMAND_LINE.to_string(),
            normalize_shell_args(args),
        ),
        "Edit" | "MultiEdit" | "Write" => (
            FUNCTION_EDIT_FILE.to_string(),
            normalize_edit_args(raw_name, args),
        ),
        _ => (raw_name.to_string(), args),
    }
}

fn normalize_shell_args(args: Value) -> Value {
    let command = args
        .get("command")
        .and_then(Value::as_str)
        .or_else(|| args.get("cmd").and_then(Value::as_str))
        .unwrap_or_default();
    json!({
        "command": command,
        "cmd": command,
    })
}

fn normalize_edit_args(raw_name: &str, args: Value) -> Value {
    let file_path = args
        .get("file_path")
        .and_then(Value::as_str)
        .or_else(|| args.get("path").and_then(Value::as_str))
        .unwrap_or_default();
    json!({
        "action": raw_name,
        "file_path": file_path,
        "payload": args,
    })
}

fn claude_content_items(content: Option<&Value>) -> Vec<&Value> {
    match content {
        Some(Value::Array(items)) => items.iter().collect(),
        _ => Vec::new(),
    }
}

fn claude_tool_result_text(content: Option<&Value>) -> Option<(String, String)> {
    let Some(Value::Array(items)) = content else {
        return None;
    };
    let result_item = items
        .iter()
        .find(|item| item.get("type").and_then(Value::as_str) == Some("tool_result"))?;
    let call_id = result_item.get("tool_use_id")?.as_str()?.to_string();
    let output = match result_item.get("content") {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    Some((call_id, output))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::{
        ACTION_TYPE_ASSISTANT, ACTION_TYPE_RAW, ACTION_TYPE_TOOL_CALL, FUNCTION_ASSISTANT,
        FUNCTION_USER_MESSAGE,
    };

    fn temp_source_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-claude-source-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create native source root");
        path
    }

    fn profile(root: PathBuf) -> SourceProfile {
        SourceProfile {
            name: "claude_code".to_string(),
            app_id: Some("claude_code".to_string()),
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
    fn formats_claude_code_jsonl_as_source_chunks() {
        let root = temp_source_root("chunks");
        let transcript_path = root.join("claude-session.jsonl");
        fs::write(
            &transcript_path,
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"Run tests\"}}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-06-18T01:01:00Z\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Sure\"},{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"Bash\",\"input\":{\"command\":\"cargo test\"}}]}}\n",
                "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:02:00Z\",\"message\":{\"role\":\"user\",\"content\":[{\"type\":\"tool_result\",\"tool_use_id\":\"tool-1\",\"content\":\"ok\"}]}}\n"
            ),
        )
        .expect("write claude chunks transcript");

        let chunks =
            format_chunks("claude-session", Some(&transcript_path)).expect("format chunks");

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].action_type, ACTION_TYPE_RAW);
        assert_eq!(chunks[0].function, FUNCTION_USER_MESSAGE);
        assert_eq!(chunks[1].action_type, ACTION_TYPE_ASSISTANT);
        assert_eq!(chunks[1].function, FUNCTION_ASSISTANT);
        assert_eq!(chunks[2].action_type, ACTION_TYPE_TOOL_CALL);
        assert_eq!(chunks[2].function, FUNCTION_RUN_COMMAND_LINE);
        assert_eq!(chunks[2].args["command"], "cargo test");
        assert_eq!(chunks[2].result["output"], "ok");
        assert_eq!(chunks[0].source_id.as_deref(), Some(CLAUDE_CODE_SOURCE_ID));
        assert_eq!(
            chunks[0].source_path.as_deref(),
            Some(transcript_path.display().to_string().as_str())
        );
        assert_eq!(chunks[0].source_line_number, Some(1));
        assert_eq!(chunks[2].source_line_number, Some(2));
        assert_eq!(chunks[2].source_message_id.as_deref(), Some("tool-1"));
    }

    #[test]
    fn ignores_non_transcript_files() {
        let root = temp_source_root("non-transcripts");
        fs::write(root.join("settings.json"), "{}").expect("write settings");
        fs::write(root.join("help.md"), "# Help").expect("write help");
        fs::write(root.join("session.meta.json"), "{}").expect("write meta");
        fs::write(
            root.join("claude-session.jsonl"),
            "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"Real session\"}}\n",
        )
        .expect("write transcript");

        let sessions = list_sessions(&profile(root), Some(10)).expect("list sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].external_session_id, "claude-session");
    }

    #[test]
    fn links_claude_subagents_to_parent_sessions() {
        let root = temp_source_root("subagents");
        fs::write(
            root.join("parent-session.jsonl"),
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:00:00Z\",\"message\":{\"role\":\"user\",\"content\":\"Use an agent\"}}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-06-18T01:01:00Z\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"tool-1\",\"name\":\"Agent\"}]}}\n"
            ),
        )
        .expect("write parent transcript");
        let subagent_dir = root.join("parent-session").join("subagents");
        fs::create_dir_all(&subagent_dir).expect("create subagent dir");
        fs::write(
            subagent_dir.join("agent-worker.jsonl"),
            concat!(
                "{\"type\":\"user\",\"isSidechain\":true,\"agentId\":\"worker\",\"sessionId\":\"parent-session\",\"message\":{\"role\":\"user\",\"content\":\"Investigate\"}}\n",
                "{\"type\":\"assistant\",\"isSidechain\":true,\"agentId\":\"worker\",\"sessionId\":\"parent-session\",\"attributionAgent\":\"Explore\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Done\"}]}}\n"
            ),
        )
        .expect("write subagent transcript");

        let sessions = list_sessions(&profile(root), Some(10)).expect("list sessions");
        let parent = sessions
            .iter()
            .find(|session| session.external_session_id == "parent-session")
            .expect("parent session");
        let subagent = sessions
            .iter()
            .find(|session| session.external_session_id == "agent-worker")
            .expect("subagent session");

        assert!(parent.listable);
        assert!(!subagent.listable);
        assert_eq!(
            parent
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("subagentSessionIds")),
            Some(&json!(["agent-worker"]))
        );
        assert_eq!(
            subagent
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("parentSessionId")),
            Some(&json!("parent-session"))
        );
        assert_eq!(
            subagent
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("subagentType")),
            Some(&json!("Explore"))
        );
    }

    #[test]
    fn extracts_claude_code_jsonl_session_metadata() {
        let root = temp_source_root("metadata");
        let transcript_path = root.join("claude-session.jsonl");
        fs::write(
            &transcript_path,
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:00:00Z\",\"cwd\":\"/repo\",\"gitBranch\":\"main\",\"message\":{\"role\":\"user\",\"content\":\"Implement feature\"}}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-06-18T01:02:00Z\",\"message\":{\"model\":\"claude-sonnet\",\"usage\":{\"input_tokens\":10,\"cache_read_input_tokens\":3,\"output_tokens\":7}}}\n"
            ),
        )
        .expect("write claude transcript");

        let sessions = list_sessions(&profile(root), Some(10)).expect("list sessions");

        assert_eq!(sessions[0].title.as_deref(), Some("Implement feature"));
        assert_eq!(sessions[0].model.as_deref(), Some("claude-sonnet"));
        assert_eq!(sessions[0].input_tokens, Some(13));
        assert_eq!(sessions[0].output_tokens, Some(7));
        assert_eq!(sessions[0].repo_path.as_deref(), Some(Path::new("/repo")));
        assert_eq!(sessions[0].branch.as_deref(), Some("main"));
        assert_eq!(sessions[0].parser_version, CLAUDE_CODE_JSONL_PARSER_VERSION);
    }

    #[test]
    fn extracts_claude_code_touched_files_from_edit_and_bash() {
        let root = temp_source_root("touched");
        let transcript_path = root.join("claude-session.jsonl");
        // An assistant turn that Writes a file and runs a Bash redirect.
        fs::write(
            &transcript_path,
            concat!(
                "{\"type\":\"user\",\"timestamp\":\"2026-06-18T01:00:00Z\",\"cwd\":\"/repo\",\"message\":{\"role\":\"user\",\"content\":\"Add files\"}}\n",
                "{\"type\":\"assistant\",\"timestamp\":\"2026-06-18T01:02:00Z\",\"message\":{\"content\":[{\"type\":\"tool_use\",\"id\":\"t1\",\"name\":\"Write\",\"input\":{\"file_path\":\"src/main.rs\",\"content\":\"fn main(){}\"}},{\"type\":\"tool_use\",\"id\":\"t2\",\"name\":\"Bash\",\"input\":{\"command\":\"echo hi > notes.txt\"}}]}}\n"
            ),
        )
        .expect("write claude transcript");

        let sessions = list_sessions(&profile(root), Some(10)).expect("list sessions");

        assert_eq!(
            sessions[0].touched_files,
            vec!["notes.txt".to_string(), "src/main.rs".to_string()]
        );
        assert_eq!(sessions[0].files_changed, Some(2));
    }
}
