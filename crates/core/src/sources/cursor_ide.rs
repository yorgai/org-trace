use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::{
    assistant_message_chunk, parse_inner_json, tool_call_chunk, user_message_chunk, ActivityChunk,
    ImportedToolCall, NativeSourceSession, SourceProfile, FUNCTION_EDIT_FILE,
    FUNCTION_RUN_COMMAND_LINE,
};

use super::cursor_family::{open_state_db, read_kv_value};

const CURSOR_IDE_SOURCE_ID: &str = "cursor_ide";
const CURSOR_COMPOSER_HEADERS_KEY: &str = "composer.composerHeaders";
const CURSOR_IDE_HEADERS_PARSER_VERSION: &str = "cursor-ide-composer-headers-v1";
const CURSOR_IDE_PROVIDER_SLUG: &str = "cursor";
const CURSOR_COMPOSER_DATA_PREFIX: &str = "composerData:";
const CURSOR_BUBBLE_PREFIX: &str = "bubbleId:";
const TITLE_LIMIT: usize = 200;
const CURSOR_BUBBLE_TYPE_USER: i64 = 1;
const CURSOR_BUBBLE_TYPE_ASSISTANT: i64 = 2;

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    let state_db_path = cursor_state_db_path(profile)?;
    let connection = open_state_db(&state_db_path)?;
    let Some(headers_json) = read_kv_value(&connection, CURSOR_COMPOSER_HEADERS_KEY)? else {
        return Ok(Vec::new());
    };
    let headers: Value = serde_json::from_str(&headers_json)
        .context("failed to parse Cursor composer headers JSON")?;
    let all_composers = headers
        .get("allComposers")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow!("Cursor composer headers missing allComposers object"))?;
    let db_metadata = fs::metadata(&state_db_path).with_context(|| {
        format!(
            "failed to read Cursor state DB metadata at {}",
            state_db_path.display()
        )
    })?;
    let source_app_id = profile
        .app_id
        .clone()
        .unwrap_or_else(|| CURSOR_IDE_SOURCE_ID.to_string());
    let mut sessions = all_composers
        .iter()
        .filter_map(|(composer_id, composer)| {
            composer_header_session(
                composer_id,
                composer,
                &state_db_path,
                &source_app_id,
                &db_metadata,
            )
            .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    sessions.sort_by(|left, right| {
        right
            .session_updated_at
            .or(right.modified_at)
            .cmp(&left.session_updated_at.or(left.modified_at))
    });
    sessions.truncate(limit.unwrap_or(50));
    Ok(sessions)
}

pub(super) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    let state_db_path = source_path.ok_or_else(|| {
        anyhow!("Cursor IDE source path missing for session: {external_session_id}")
    })?;
    let connection = open_state_db(state_db_path)?;
    let composer_key = format!("{CURSOR_COMPOSER_DATA_PREFIX}{external_session_id}");
    let Some(composer_json) = read_kv_value(&connection, &composer_key)? else {
        return Ok(Vec::new());
    };
    let composer: Value = serde_json::from_str(&composer_json)
        .context("failed to parse Cursor composer data JSON")?;
    let bubble_ids = cursor_bubble_ids(&composer);
    let mut chunks = Vec::new();
    for (sequence, bubble_id) in bubble_ids.iter().enumerate() {
        let bubble_key = format!("{CURSOR_BUBBLE_PREFIX}{external_session_id}:{bubble_id}");
        let Some(bubble_json) = read_kv_value(&connection, &bubble_key)? else {
            continue;
        };
        let bubble: Value =
            serde_json::from_str(&bubble_json).context("failed to parse Cursor bubble JSON")?;
        if let Some(tool_call) = cursor_tool_call_from_bubble(bubble_id, &bubble) {
            chunks.push(tool_call_chunk(
                external_session_id,
                CURSOR_IDE_PROVIDER_SLUG,
                sequence,
                &tool_call,
                &cursor_tool_result_text(&bubble),
            ));
            continue;
        }
        let created_at = cursor_created_at(&bubble);
        if cursor_bubble_is_user(&bubble) {
            if let Some(message) = cursor_text(&bubble) {
                chunks.push(user_message_chunk(
                    external_session_id,
                    CURSOR_IDE_PROVIDER_SLUG,
                    sequence,
                    &created_at,
                    &message,
                ));
            }
        } else if cursor_bubble_is_assistant(&bubble) {
            if let Some(message) = cursor_text(&bubble) {
                chunks.push(assistant_message_chunk(
                    external_session_id,
                    CURSOR_IDE_PROVIDER_SLUG,
                    sequence,
                    &created_at,
                    &message,
                ));
            }
        }
    }
    Ok(chunks)
}

fn cursor_bubble_ids(composer: &Value) -> Vec<String> {
    composer
        .get("fullConversationHeadersOnly")
        .and_then(Value::as_array)
        .map(|headers| {
            headers
                .iter()
                .filter_map(|header| {
                    header
                        .get("bubbleId")
                        .or_else(|| header.get("id"))
                        .and_then(Value::as_str)
                        .map(ToOwned::to_owned)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn cursor_bubble_is_user(bubble: &Value) -> bool {
    cursor_bubble_type(bubble) == Some(CURSOR_BUBBLE_TYPE_USER)
        || bubble.get("role").and_then(Value::as_str) == Some("user")
}

fn cursor_bubble_is_assistant(bubble: &Value) -> bool {
    cursor_bubble_type(bubble) == Some(CURSOR_BUBBLE_TYPE_ASSISTANT)
        || bubble.get("role").and_then(Value::as_str) == Some("assistant")
}

fn cursor_bubble_type(bubble: &Value) -> Option<i64> {
    bubble
        .get("type")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn cursor_text(bubble: &Value) -> Option<String> {
    ["text", "content", "message", "richText"]
        .iter()
        .filter_map(|key| bubble.get(key))
        .find_map(cursor_value_text)
}

fn cursor_value_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        if text.trim().is_empty() {
            None
        } else {
            Some(text.to_string())
        }
    } else {
        value
            .get("text")
            .or_else(|| value.get("content"))
            .and_then(cursor_value_text)
    }
}

fn cursor_tool_call_from_bubble(bubble_id: &str, bubble: &Value) -> Option<ImportedToolCall> {
    let tool = bubble.get("toolFormerData")?;
    let raw_name = tool
        .get("name")
        .or_else(|| tool.get("toolName"))
        .or_else(|| tool.get("type"))
        .and_then(Value::as_str)
        .unwrap_or("tool")
        .to_string();
    let args = tool
        .get("params")
        .or_else(|| tool.get("arguments"))
        .or_else(|| tool.get("args"))
        .map(cursor_tool_payload_json)
        .unwrap_or_else(|| json!({}));
    Some(ImportedToolCall {
        call_id: tool
            .get("callId")
            .or_else(|| tool.get("toolCallId"))
            .and_then(Value::as_str)
            .unwrap_or(bubble_id)
            .to_string(),
        raw_name: raw_name.clone(),
        canonical_name: cursor_canonical_tool_name(&raw_name),
        args,
        created_at: cursor_created_at(bubble),
    })
}

fn cursor_tool_payload_json(value: &Value) -> Value {
    value
        .as_str()
        .map(parse_inner_json)
        .unwrap_or_else(|| value.clone())
}

fn cursor_tool_result_text(bubble: &Value) -> String {
    bubble
        .get("toolFormerData")
        .and_then(|tool| tool.get("result").or_else(|| tool.get("output")))
        .and_then(cursor_value_text)
        .or_else(|| cursor_text(bubble))
        .unwrap_or_default()
}

fn cursor_canonical_tool_name(raw_name: &str) -> String {
    let normalized = raw_name.to_ascii_lowercase();
    if normalized.contains("shell")
        || normalized.contains("terminal")
        || normalized.contains("command")
        || normalized.contains("bash")
    {
        FUNCTION_RUN_COMMAND_LINE.to_string()
    } else if normalized.contains("edit")
        || normalized.contains("write")
        || normalized.contains("patch")
    {
        FUNCTION_EDIT_FILE.to_string()
    } else {
        raw_name.to_string()
    }
}

fn cursor_created_at(bubble: &Value) -> String {
    bubble
        .get("createdAt")
        .or_else(|| bubble.get("timestamp"))
        .and_then(cursor_time_value_to_rfc3339)
        .unwrap_or_default()
}

fn cursor_time_value_to_rfc3339(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let millis = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))?;
    let time = UNIX_EPOCH + Duration::from_millis(millis);
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    Some(datetime.to_rfc3339())
}

fn cursor_state_db_path(profile: &SourceProfile) -> Result<PathBuf> {
    profile
        .cursor_state_db_path
        .clone()
        .or_else(|| profile.session_db_path.clone())
        .ok_or_else(|| {
            anyhow!("cursor_ide source requires cursor_state_db_path or session_db_path")
        })
}

fn composer_header_session(
    composer_id: &str,
    composer: &Value,
    state_db_path: &Path,
    source_app_id: &str,
    db_metadata: &fs::Metadata,
) -> Result<Option<NativeSourceSession>> {
    if composer
        .get("isBestOfNSubcomposer")
        .and_then(Value::as_bool)
        == Some(true)
    {
        return Ok(None);
    }
    let title = composer
        .get("name")
        .and_then(Value::as_str)
        .map(truncate_title)
        .or_else(|| Some(composer_id.to_string()));
    let session_created_at = composer.get("createdAt").and_then(value_to_system_time_ms);
    let session_updated_at = composer
        .get("lastUpdatedAt")
        .and_then(value_to_system_time_ms);
    let repo_path = repo_path_from_header(composer);
    let branch = branch_from_header(composer);
    let model = composer
        .get("modelConfig")
        .and_then(|model_config| model_config.get("modelName"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned);
    Ok(Some(NativeSourceSession {
        external_session_id: composer_id.to_string(),
        source_app_id: source_app_id.to_string(),
        title,
        path: state_db_path.to_path_buf(),
        size_bytes: db_metadata.len(),
        modified_at: db_metadata.modified().ok(),
        parser_version: CURSOR_IDE_HEADERS_PARSER_VERSION.to_string(),
        session_created_at,
        session_updated_at,
        model,
        input_tokens: None,
        output_tokens: None,
        repo_path,
        branch,
        files_changed: composer.get("filesChangedCount").and_then(Value::as_u64),
        lines_added: composer.get("totalLinesAdded").and_then(Value::as_u64),
        lines_removed: composer.get("totalLinesRemoved").and_then(Value::as_u64),
        touched_files: Vec::new(),
    }))
}

fn repo_path_from_header(composer: &Value) -> Option<PathBuf> {
    composer
        .get("trackedGitRepos")
        .and_then(Value::as_array)
        .and_then(|repos| repos.first())
        .and_then(|repo| repo.get("repoPath"))
        .and_then(Value::as_str)
        .map(PathBuf::from)
        .or_else(|| {
            composer
                .get("workspaceIdentifier")
                .and_then(|workspace| workspace.get("uri"))
                .and_then(|uri| uri.get("fsPath").or_else(|| uri.get("path")))
                .and_then(Value::as_str)
                .map(PathBuf::from)
        })
}

fn branch_from_header(composer: &Value) -> Option<String> {
    composer
        .get("trackedGitRepos")
        .and_then(Value::as_array)
        .and_then(|repos| repos.first())
        .and_then(|repo| repo.get("branches"))
        .and_then(Value::as_array)
        .and_then(|branches| branches.first())
        .and_then(|branch| branch.get("branchName"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn value_to_system_time_ms(value: &Value) -> Option<SystemTime> {
    let millis = value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))?;
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

fn truncate_title(value: &str) -> String {
    value.chars().take(TITLE_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::{
        ACTION_TYPE_ASSISTANT, ACTION_TYPE_RAW, ACTION_TYPE_TOOL_CALL, FUNCTION_ASSISTANT,
        FUNCTION_RUN_COMMAND_LINE, FUNCTION_USER_MESSAGE,
    };

    fn temp_state_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "brick-cursor-state-{name}-{}.vscdb",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn profile(path: PathBuf) -> SourceProfile {
        SourceProfile {
            name: CURSOR_IDE_SOURCE_ID.to_string(),
            app_id: Some(CURSOR_IDE_SOURCE_ID.to_string()),
            actor_id: None,
            actor_type: None,
            store_root: None,
            session_db_path: None,
            session_log_path: None,
            evidence_root: None,
            cursor_state_db_path: Some(path),
            default_full_evidence_upload: None,
            notes: None,
        }
    }

    #[test]
    fn extracts_sessions_from_cursor_composer_headers() {
        let path = temp_state_db("headers");
        let connection = Connection::open(&path).expect("open temp cursor state DB");
        connection
            .execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .expect("create cursorDiskKV");
        let headers = serde_json::json!({
            "allComposers": {
                "composer-1": {
                    "name": "Implement Cursor provider",
                    "createdAt": 1_766_000_000_000_u64,
                    "lastUpdatedAt": 1_766_000_060_000_u64,
                    "workspaceIdentifier": {
                        "uri": { "fsPath": "/workspace/fallback" }
                    },
                    "trackedGitRepos": [
                        {
                            "repoPath": "/workspace/repo",
                            "branches": [{ "branchName": "main" }]
                        }
                    ],
                    "subtitle": "Edited files",
                    "mode": "agent",
                    "isArchived": false,
                    "modelConfig": { "modelName": "cursor-model" },
                    "filesChangedCount": 2,
                    "totalLinesAdded": 10,
                    "totalLinesRemoved": 3
                }
            }
        });
        connection
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                [CURSOR_COMPOSER_HEADERS_KEY, &headers.to_string()],
            )
            .expect("insert headers");
        drop(connection);

        let sessions = list_sessions(&profile(path), Some(10)).expect("list cursor sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].external_session_id, "composer-1");
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("Implement Cursor provider")
        );
        assert_eq!(
            sessions[0].repo_path.as_deref(),
            Some(Path::new("/workspace/repo"))
        );
        assert_eq!(sessions[0].branch.as_deref(), Some("main"));
        assert_eq!(sessions[0].model.as_deref(), Some("cursor-model"));
        assert_eq!(sessions[0].files_changed, Some(2));
        assert_eq!(sessions[0].lines_added, Some(10));
        assert_eq!(sessions[0].lines_removed, Some(3));
        assert_eq!(
            sessions[0].parser_version,
            CURSOR_IDE_HEADERS_PARSER_VERSION
        );
    }

    #[test]
    fn formats_cursor_composer_bubbles_as_source_chunks() {
        let path = temp_state_db("chunks");
        let connection = Connection::open(&path).expect("open temp cursor state DB");
        connection
            .execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .expect("create cursorDiskKV");
        let composer = serde_json::json!({
            "composerId": "composer-1",
            "fullConversationHeadersOnly": [
                { "bubbleId": "user-1" },
                { "bubbleId": "assistant-1" },
                { "bubbleId": "tool-1" }
            ]
        });
        let user_bubble = serde_json::json!({
            "bubbleId": "user-1",
            "type": 1,
            "createdAt": 1_766_000_000_000_u64,
            "text": "please list files"
        });
        let assistant_bubble = serde_json::json!({
            "bubbleId": "assistant-1",
            "type": 2,
            "createdAt": 1_766_000_001_000_u64,
            "text": "I will inspect the workspace."
        });
        let tool_bubble = serde_json::json!({
            "bubbleId": "tool-1",
            "type": 2,
            "createdAt": 1_766_000_002_000_u64,
            "toolFormerData": {
                "name": "run_terminal_command",
                "params": "{\"command\":\"ls\"}",
                "result": "README.md"
            }
        });
        let rows = [
            ("composerData:composer-1", composer),
            ("bubbleId:composer-1:user-1", user_bubble),
            ("bubbleId:composer-1:assistant-1", assistant_bubble),
            ("bubbleId:composer-1:tool-1", tool_bubble),
        ];
        for (key, value) in rows {
            connection
                .execute(
                    "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                    (key, value.to_string()),
                )
                .expect("insert cursor KV row");
        }
        drop(connection);

        let chunks = format_chunks("composer-1", Some(&path)).expect("format cursor chunks");

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].action_type, ACTION_TYPE_RAW);
        assert_eq!(chunks[0].function, FUNCTION_USER_MESSAGE);
        assert_eq!(chunks[1].action_type, ACTION_TYPE_ASSISTANT);
        assert_eq!(chunks[1].function, FUNCTION_ASSISTANT);
        assert_eq!(chunks[2].action_type, ACTION_TYPE_TOOL_CALL);
        assert_eq!(chunks[2].function, FUNCTION_RUN_COMMAND_LINE);
        assert_eq!(chunks[2].args["command"], "ls");
        assert_eq!(chunks[2].result["output"], "README.md");
    }
}
