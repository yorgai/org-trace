use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use anyhow::{anyhow, Result};
use serde_json::{json, Value};

use crate::{
    assistant_message_chunk, list_file_source_sessions, parse_inner_json, thinking_chunk,
    tool_call_chunk, user_message_chunk, ActivityChunk, ImportedToolCall, NativeSessionMetadata,
    NativeSourceSession, SourceProfile, FUNCTION_EDIT_FILE, FUNCTION_RUN_COMMAND_LINE,
};

use super::jsonl::{
    read_jsonl_records, read_jsonl_values, set_first_path, set_first_string, text_from_value,
    token_value, truncate_title, update_session_times,
};

const CODEX_APP_SOURCE_ID: &str = "codex_app";
const CODEX_APP_JSONL_PARSER_VERSION: &str = "codex-app-jsonl-v4";
const CODEX_APP_PROVIDER_SLUG: &str = "codex";

#[derive(Debug, Default)]
struct PatchImpact {
    touched_files: BTreeSet<String>,
    lines_added: u64,
    lines_removed: u64,
}

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<&str>,
) -> Result<Vec<NativeSourceSession>> {
    list_file_source_sessions(
        profile,
        limit,
        crate::since_to_system_time(since),
        extract_jsonl_metadata,
    )
}

pub(super) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    let path = source_path.ok_or_else(|| {
        anyhow!("Codex App source path missing for session: {external_session_id}")
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
        let Some(payload) = value.get("payload") else {
            continue;
        };
        match payload
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "user_message" => {
                if let Some(message) = user_message_from_payload(payload) {
                    let mut chunk = user_message_chunk(
                        external_session_id,
                        CODEX_APP_PROVIDER_SLUG,
                        sequence,
                        created_at,
                        &message,
                    );
                    chunk.set_source_pointer(
                        CODEX_APP_SOURCE_ID,
                        path,
                        None,
                        Some(record.line_number),
                        payload.get("id").and_then(Value::as_str),
                        None,
                    );
                    chunks.push(chunk);
                    sequence += 1;
                }
            }
            "agent_message" => {
                if let Some(message) = payload.get("message").and_then(Value::as_str) {
                    let mut chunk = assistant_message_chunk(
                        external_session_id,
                        CODEX_APP_PROVIDER_SLUG,
                        sequence,
                        created_at,
                        message,
                    );
                    chunk.set_source_pointer(
                        CODEX_APP_SOURCE_ID,
                        path,
                        None,
                        Some(record.line_number),
                        payload.get("id").and_then(Value::as_str),
                        None,
                    );
                    chunks.push(chunk);
                    sequence += 1;
                }
            }
            "message" => {
                if payload.get("role").and_then(Value::as_str) == Some("assistant") {
                    if let Some(text) = content_text_from_payload(payload) {
                        let mut chunk = assistant_message_chunk(
                            external_session_id,
                            CODEX_APP_PROVIDER_SLUG,
                            sequence,
                            created_at,
                            &text,
                        );
                        chunk.set_source_pointer(
                            CODEX_APP_SOURCE_ID,
                            path,
                            None,
                            Some(record.line_number),
                            payload.get("id").and_then(Value::as_str),
                            None,
                        );
                        chunks.push(chunk);
                        sequence += 1;
                    }
                }
            }
            "reasoning" | "agent_reasoning" => {
                if let Some(text) = reasoning_text_from_payload(payload) {
                    let mut chunk = thinking_chunk(
                        external_session_id,
                        CODEX_APP_PROVIDER_SLUG,
                        sequence,
                        created_at,
                        &text,
                    );
                    chunk.set_source_pointer(
                        CODEX_APP_SOURCE_ID,
                        path,
                        None,
                        Some(record.line_number),
                        payload.get("id").and_then(Value::as_str),
                        None,
                    );
                    chunks.push(chunk);
                    sequence += 1;
                }
            }
            "function_call" => {
                if let Some(call) = pending_tool_call_from_payload(payload, created_at) {
                    pending_tool_calls.insert(call.call_id.clone(), (call, record.line_number));
                }
            }
            "custom_tool_call" => {
                if let Some(call) = pending_custom_tool_call_from_payload(payload, created_at) {
                    pending_tool_calls.insert(call.call_id.clone(), (call, record.line_number));
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                if let Some(call_id) = payload.get("call_id").and_then(Value::as_str) {
                    if let Some((call, line_number)) = pending_tool_calls.remove(call_id) {
                        let output = payload
                            .get("output")
                            .and_then(Value::as_str)
                            .unwrap_or_default();
                        let mut chunk = tool_call_chunk(
                            external_session_id,
                            CODEX_APP_PROVIDER_SLUG,
                            sequence,
                            &call,
                            output,
                        );
                        chunk.set_source_pointer(
                            CODEX_APP_SOURCE_ID,
                            path,
                            None,
                            Some(line_number),
                            Some(&call.call_id),
                            None,
                        );
                        chunks.push(chunk);
                        sequence += 1;
                    }
                }
            }
            _ => {}
        }
    }

    for (call, line_number) in pending_tool_calls.into_values() {
        let mut chunk = tool_call_chunk(
            external_session_id,
            CODEX_APP_PROVIDER_SLUG,
            sequence,
            &call,
            "",
        );
        chunk.set_source_pointer(
            CODEX_APP_SOURCE_ID,
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
        parser_version: Some(CODEX_APP_JSONL_PARSER_VERSION.to_string()),
        ..NativeSessionMetadata::default()
    };
    let mut patch_impact = PatchImpact::default();

    for value in lines {
        update_session_times(&mut metadata, value.get("timestamp"));
        let Some(payload) = value.get("payload") else {
            continue;
        };
        let payload_type = payload.get("type").and_then(Value::as_str);

        set_first_path(&mut metadata.repo_path, payload.get("cwd"));
        set_first_path(&mut metadata.cwd, payload.get("cwd"));
        set_first_string(&mut metadata.model, payload.get("model"));

        if metadata.title.is_none() && payload_type == Some("user_message") {
            metadata.title = payload
                .get("message")
                .and_then(text_from_value)
                .map(truncate_title);
        }

        if payload_type == Some("token_count") {
            if let Some(usage) = payload.get("total_token_usage") {
                let input_tokens = token_value(usage, "input_tokens");
                let output_tokens = token_value(usage, "output_tokens");
                if input_tokens > 0 {
                    metadata.input_tokens = Some(input_tokens);
                }
                if output_tokens > 0 {
                    metadata.output_tokens = Some(output_tokens);
                }
            }
        }

        if matches!(payload_type, Some("function_call" | "custom_tool_call")) {
            collect_patch_impact(payload, &mut patch_impact);
            collect_shell_edits(payload, &mut patch_impact);
        }
    }

    if !patch_impact.touched_files.is_empty() {
        metadata.files_changed = Some(patch_impact.touched_files.len() as u64);
        metadata.lines_added = Some(patch_impact.lines_added);
        metadata.lines_removed = Some(patch_impact.lines_removed);
        metadata.touched_files = patch_impact.touched_files.into_iter().collect();
    }

    Ok(metadata)
}

fn collect_shell_edits(payload: &Value, impact: &mut PatchImpact) {
    let name = payload.get("name").and_then(Value::as_str).unwrap_or("");
    if !matches!(name, "shell" | "exec_command" | "local_shell") {
        return;
    }
    // The command may be a string, or a list of argv tokens, inside
    // `arguments` (a JSON string) or directly on the payload.
    let arguments = payload
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok());
    let command_value = arguments
        .as_ref()
        .and_then(|args| args.get("command").cloned())
        .or_else(|| payload.get("command").cloned());
    let command = match command_value {
        Some(Value::String(text)) => text,
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(" "),
        _ => return,
    };
    for file in crate::sources::shell_edits::shell_edit_targets(&command) {
        impact.touched_files.insert(file);
    }
}

fn collect_patch_impact(payload: &Value, impact: &mut PatchImpact) {
    if payload.get("name").and_then(Value::as_str) != Some("apply_patch") {
        return;
    }
    let patch = payload
        .get("arguments")
        .and_then(Value::as_str)
        .and_then(|arguments| serde_json::from_str::<Value>(arguments).ok())
        .and_then(|arguments| {
            arguments
                .get("patch")
                .or_else(|| arguments.get("input"))
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        })
        .or_else(|| {
            payload
                .get("input")
                .and_then(Value::as_str)
                .map(ToOwned::to_owned)
        });
    if let Some(patch_text) = patch {
        parse_patch_impact(&patch_text, impact);
    }
}

fn parse_patch_impact(patch: &str, impact: &mut PatchImpact) {
    // Codex's `apply_patch` uses its own envelope rather than a git diff:
    //   *** Begin Patch
    //   *** Add File: <path>      (or Update File / Delete File / Move to:)
    //   +<added line>             (no +++/--- headers)
    //   *** End Patch
    // We recognize those `*** … File:` headers for touched_files and only count
    // body +/- lines, while still understanding plain git-diff output from other
    // emitters.
    for line in patch.lines() {
        if line.starts_with("*** Begin Patch") || line.starts_with("*** End Patch") {
            continue;
        }
        if let Some(path) = codex_patch_header_path(line) {
            if let Some(normalized) = normalize_diff_path(&path) {
                impact.touched_files.insert(normalized);
            }
            continue;
        }
        if line.starts_with("*** ") {
            // Other Codex patch directives (e.g. hunk markers) carry no path.
            continue;
        }
        if let Some(path) = line.strip_prefix("diff --git ") {
            for part in path.split_whitespace().take(2) {
                if let Some(normalized) = normalize_diff_path(part) {
                    impact.touched_files.insert(normalized);
                }
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("+++ ") {
            if let Some(normalized) = normalize_diff_path(path) {
                impact.touched_files.insert(normalized);
            }
            continue;
        }
        if line.starts_with('+') && !line.starts_with("+++") {
            impact.lines_added = impact.lines_added.saturating_add(1);
            continue;
        }
        if line.starts_with('-') && !line.starts_with("---") {
            impact.lines_removed = impact.lines_removed.saturating_add(1);
        }
    }
}

/// Extracts the target path from a Codex apply_patch file header line such as
/// `*** Add File: src/lib.rs` / `*** Update File: …` / `*** Delete File: …` /
/// `*** Move to: …`.
fn codex_patch_header_path(line: &str) -> Option<String> {
    let rest = line.strip_prefix("*** ")?;
    for marker in ["Add File:", "Update File:", "Delete File:", "Move to:"] {
        if let Some(path) = rest.strip_prefix(marker) {
            let path = path.trim();
            if !path.is_empty() {
                return Some(path.to_string());
            }
        }
    }
    None
}

fn normalize_diff_path(path: &str) -> Option<String> {
    let trimmed = path.trim().trim_matches('"');
    if trimmed == "/dev/null" {
        return None;
    }
    let normalized = trimmed
        .strip_prefix("a/")
        .or_else(|| trimmed.strip_prefix("b/"))
        .unwrap_or(trimmed);
    (!normalized.is_empty()).then(|| normalized.to_string())
}

fn pending_tool_call_from_payload(payload: &Value, created_at: &str) -> Option<ImportedToolCall> {
    let call_id = payload.get("call_id")?.as_str()?.to_string();
    let raw_name = payload.get("name")?.as_str()?.to_string();
    let arguments = payload
        .get("arguments")
        .and_then(Value::as_str)
        .map(parse_inner_json)
        .unwrap_or_else(|| json!({}));
    let (canonical_name, args) = normalize_codex_tool_call(&raw_name, arguments);
    Some(ImportedToolCall {
        call_id,
        raw_name,
        canonical_name,
        args,
        created_at: created_at.to_string(),
    })
}

fn pending_custom_tool_call_from_payload(
    payload: &Value,
    created_at: &str,
) -> Option<ImportedToolCall> {
    let call_id = payload.get("call_id")?.as_str()?.to_string();
    let raw_name = payload.get("name")?.as_str()?.to_string();
    let input = payload
        .get("input")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let args = if raw_name == "apply_patch" {
        json!({ "patch": input })
    } else {
        json!({ "input": input })
    };
    let (canonical_name, args) = normalize_codex_tool_call(&raw_name, args);
    Some(ImportedToolCall {
        call_id,
        raw_name,
        canonical_name,
        args,
        created_at: created_at.to_string(),
    })
}

fn normalize_codex_tool_call(raw_name: &str, args: Value) -> (String, Value) {
    match raw_name {
        "shell" => (
            FUNCTION_RUN_COMMAND_LINE.to_string(),
            normalize_shell_args(args),
        ),
        "apply_patch" => (
            FUNCTION_EDIT_FILE.to_string(),
            normalize_apply_patch_args(args),
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

fn normalize_apply_patch_args(args: Value) -> Value {
    let patch = args
        .get("patch")
        .and_then(Value::as_str)
        .or_else(|| args.get("input").and_then(Value::as_str))
        .unwrap_or_default();
    json!({
        "action": "apply_patch",
        "patch": patch,
    })
}

fn user_message_from_payload(payload: &Value) -> Option<String> {
    payload
        .get("message")
        .and_then(Value::as_str)
        .map(ToString::to_string)
}

fn content_text_from_payload(payload: &Value) -> Option<String> {
    let content = payload.get("content")?;
    match content {
        Value::String(text) => Some(text.clone()),
        Value::Array(items) => {
            let parts = items
                .iter()
                .filter_map(content_part_text)
                .collect::<Vec<_>>();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        _ => None,
    }
}

fn content_part_text(part: &Value) -> Option<&str> {
    part.get("text")
        .and_then(Value::as_str)
        .or_else(|| part.get("content").and_then(Value::as_str))
}

fn reasoning_text_from_payload(payload: &Value) -> Option<String> {
    if let Some(text) = payload.get("content").and_then(Value::as_str) {
        if !text.trim().is_empty() {
            return Some(text.to_string());
        }
    }
    let summary = payload.get("summary")?.as_array()?;
    let parts = summary
        .iter()
        .filter_map(content_part_text)
        .collect::<Vec<_>>();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;
    use crate::{
        ACTION_TYPE_ASSISTANT, ACTION_TYPE_RAW, ACTION_TYPE_TOOL_CALL, FUNCTION_ASSISTANT,
        FUNCTION_USER_MESSAGE,
    };

    fn temp_source_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-codex-source-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create native source root");
        path
    }

    fn profile(root: PathBuf) -> SourceProfile {
        SourceProfile {
            name: "codex_app".to_string(),
            app_id: Some("codex_app".to_string()),
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
    fn formats_codex_app_jsonl_as_source_chunks() {
        let root = temp_source_root("chunks");
        let transcript_path = root.join("codex-session.jsonl");
        fs::write(
            &transcript_path,
            concat!(
                "{\"timestamp\":\"2026-06-18T01:00:00Z\",\"payload\":{\"type\":\"user_message\",\"message\":\"Patch bug\"}}\n",
                "{\"timestamp\":\"2026-06-18T01:01:00Z\",\"payload\":{\"type\":\"agent_message\",\"message\":\"Working\"}}\n",
                "{\"timestamp\":\"2026-06-18T01:02:00Z\",\"payload\":{\"type\":\"function_call\",\"call_id\":\"call-1\",\"name\":\"shell\",\"arguments\":\"{\\\"command\\\":\\\"cargo test\\\"}\"}}\n",
                "{\"timestamp\":\"2026-06-18T01:03:00Z\",\"payload\":{\"type\":\"function_call_output\",\"call_id\":\"call-1\",\"output\":\"ok\"}}\n"
            ),
        )
        .expect("write codex chunks transcript");

        let chunks = format_chunks("codex-session", Some(&transcript_path)).expect("format chunks");

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].action_type, ACTION_TYPE_RAW);
        assert_eq!(chunks[0].function, FUNCTION_USER_MESSAGE);
        assert_eq!(chunks[1].action_type, ACTION_TYPE_ASSISTANT);
        assert_eq!(chunks[1].function, FUNCTION_ASSISTANT);
        assert_eq!(chunks[2].action_type, ACTION_TYPE_TOOL_CALL);
        assert_eq!(chunks[2].function, FUNCTION_RUN_COMMAND_LINE);
        assert_eq!(chunks[2].args["command"], "cargo test");
        assert_eq!(chunks[2].result["output"], "ok");
    }

    #[test]
    fn extracts_codex_app_jsonl_session_metadata() {
        let root = temp_source_root("metadata");
        let transcript_path = root.join("codex-session.jsonl");
        fs::write(
            &transcript_path,
            concat!(
                "{\"timestamp\":\"2026-06-18T01:00:00Z\",\"payload\":{\"type\":\"user_message\",\"message\":\"Patch bug\",\"cwd\":\"/repo\",\"model\":\"gpt-5\"}}\n",
                "{\"timestamp\":\"2026-06-18T01:01:00Z\",\"payload\":{\"type\":\"token_count\",\"total_token_usage\":{\"input_tokens\":11,\"output_tokens\":5}}}\n",
                "{\"timestamp\":\"2026-06-18T01:02:00Z\",\"payload\":{\"type\":\"function_call\",\"name\":\"apply_patch\",\"arguments\":\"{\\\"patch\\\":\\\"diff --git a/src/lib.rs b/src/lib.rs\\\\n+++ b/src/lib.rs\\\\n+new\\\\n-old\\\"}\"}}\n"
            ),
        )
        .expect("write codex transcript");

        let sessions = list_sessions(&profile(root), Some(10), None).expect("list sessions");

        assert_eq!(sessions[0].title.as_deref(), Some("Patch bug"));
        assert_eq!(sessions[0].model.as_deref(), Some("gpt-5"));
        assert_eq!(sessions[0].input_tokens, Some(11));
        assert_eq!(sessions[0].output_tokens, Some(5));
        assert_eq!(sessions[0].files_changed, Some(1));
        assert_eq!(sessions[0].lines_added, Some(1));
        assert_eq!(sessions[0].lines_removed, Some(1));
        assert_eq!(sessions[0].touched_files, vec!["src/lib.rs".to_string()]);
        assert_eq!(sessions[0].parser_version, CODEX_APP_JSONL_PARSER_VERSION);
    }

    #[test]
    fn extracts_codex_apply_patch_envelope_touched_files() {
        let root = temp_source_root("apply-patch");
        let transcript_path = root.join("codex-session.jsonl");
        fs::write(
            &transcript_path,
            concat!(
                "{\"timestamp\":\"2026-06-18T01:00:00Z\",\"payload\":{\"type\":\"user_message\",\"message\":\"Add a CSV\",\"cwd\":\"/repo\"}}\n",
                "{\"timestamp\":\"2026-06-18T01:02:00Z\",\"payload\":{\"type\":\"custom_tool_call\",\"call_id\":\"c1\",\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** Add File: data/sample.csv\\n+a,b\\n+1,2\\n*** End Patch\\n\"}}\n",
                "{\"timestamp\":\"2026-06-18T01:03:00Z\",\"payload\":{\"type\":\"custom_tool_call\",\"call_id\":\"c2\",\"name\":\"apply_patch\",\"input\":\"*** Begin Patch\\n*** Update File: src/main.rs\\n-old\\n+new\\n*** End Patch\\n\"}}\n"
            ),
        )
        .expect("write codex apply_patch transcript");

        let sessions = list_sessions(&profile(root), Some(10), None).expect("list sessions");

        assert_eq!(
            sessions[0].touched_files,
            vec!["data/sample.csv".to_string(), "src/main.rs".to_string()]
        );
        assert_eq!(sessions[0].files_changed, Some(2));
    }

    #[test]
    fn codex_patch_header_path_parses_all_directives() {
        assert_eq!(
            codex_patch_header_path("*** Add File: a/b.rs").as_deref(),
            Some("a/b.rs")
        );
        assert_eq!(
            codex_patch_header_path("*** Update File: src/lib.rs").as_deref(),
            Some("src/lib.rs")
        );
        assert_eq!(
            codex_patch_header_path("*** Delete File: old.txt").as_deref(),
            Some("old.txt")
        );
        assert_eq!(
            codex_patch_header_path("*** Move to: new/path.rs").as_deref(),
            Some("new/path.rs")
        );
        assert_eq!(codex_patch_header_path("*** End Patch"), None);
        assert_eq!(codex_patch_header_path("+some content"), None);
    }
}
