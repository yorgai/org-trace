use std::collections::HashMap;
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

use super::{
    cursor_value_text, open_state_db, read_kv_entries_with_prefix, read_kv_value,
    CursorContentResolver,
};

const COMPOSER_DATA_PREFIX: &str = "composerData:";
const BUBBLE_PREFIX: &str = "bubbleId:";
const BUBBLE_TYPE_USER: i64 = 1;
const BUBBLE_TYPE_ASSISTANT: i64 = 2;

pub(in crate::sources) struct ComposerSessionOptions<'a> {
    pub source_id: &'a str,
    pub parser_version: &'a str,
    pub source_label: &'a str,
    pub include_context_tokens: bool,
    pub skip_best_of_n: bool,
}

pub(in crate::sources) fn list_sessions_from_composer_data(
    profile: &SourceProfile,
    limit: Option<usize>,
    options: ComposerSessionOptions<'_>,
) -> Result<Vec<NativeSourceSession>> {
    let state_db_path = cursor_family_state_db_path(profile, options.source_id)?;
    let connection = open_state_db(&state_db_path)?;
    let composer_rows = read_kv_entries_with_prefix(&connection, COMPOSER_DATA_PREFIX)?;
    let db_metadata = fs::metadata(&state_db_path).with_context(|| {
        format!(
            "failed to read {} state DB metadata at {}",
            options.source_label,
            state_db_path.display()
        )
    })?;
    let source_app_id = profile
        .app_id
        .clone()
        .unwrap_or_else(|| options.source_id.to_string());
    let mut sessions = composer_rows
        .iter()
        .filter_map(|(key, composer_json)| {
            composer_data_session(
                key,
                composer_json,
                &state_db_path,
                &source_app_id,
                &db_metadata,
                &options,
            )
            .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    attach_subagent_ids_to_parents(&mut sessions);
    sessions.sort_by(|left, right| {
        right
            .session_updated_at
            .or(right.modified_at)
            .cmp(&left.session_updated_at.or(left.modified_at))
    });
    sessions.truncate(limit.unwrap_or(50));
    Ok(sessions)
}

pub(in crate::sources) fn composer_header_session(
    composer_id: &str,
    composer: &Value,
    state_db_path: &Path,
    source_app_id: &str,
    db_metadata: &fs::Metadata,
    options: &ComposerSessionOptions<'_>,
) -> Result<Option<NativeSourceSession>> {
    if options.skip_best_of_n
        && composer
            .get("isBestOfNSubcomposer")
            .and_then(Value::as_bool)
            == Some(true)
    {
        return Ok(None);
    }
    let title = composer
        .get("name")
        .and_then(Value::as_str)
        .map(normalize_title)
        .or_else(|| Some(composer_id.to_string()));
    let session_created_at = composer.get("createdAt").and_then(value_to_system_time_ms);
    let session_updated_at = composer
        .get("lastUpdatedAt")
        .and_then(value_to_system_time_ms)
        .or(session_created_at);
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
        parser_version: options.parser_version.to_string(),
        session_created_at,
        session_updated_at,
        model,
        input_tokens: options
            .include_context_tokens
            .then(|| composer.get("contextTokensUsed").and_then(value_to_u64))
            .flatten(),
        output_tokens: None,
        repo_path,
        branch,
        files_changed: first_u64(
            composer,
            &["filesChangedCount", "fileChanges", "changedFilesCount"],
        ),
        lines_added: first_u64(composer, &["totalLinesAdded", "linesAdded"]),
        lines_removed: first_u64(composer, &["totalLinesRemoved", "linesRemoved"]),
        touched_files: touched_files_from_composer(composer),
        listable: true,
        metadata_json: None,
        cwd: None,
        liveness: crate::Liveness::Unknown,
        last_activity: None,
    }))
}

pub(in crate::sources) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
    source_id: &str,
    provider_slug: &str,
    source_label: &str,
) -> Result<Vec<ActivityChunk>> {
    let state_db_path = source_path.ok_or_else(|| {
        anyhow!(
            "{} source path missing for session: {external_session_id}",
            source_label
        )
    })?;
    let connection = open_state_db(state_db_path)?;
    let composer_key = format!("{COMPOSER_DATA_PREFIX}{external_session_id}");
    let Some(composer_json) = read_kv_value(&connection, &composer_key)? else {
        return Ok(Vec::new());
    };
    let composer: Value = serde_json::from_str(&composer_json)
        .with_context(|| format!("failed to parse {source_label} composer data JSON"))?;
    let bubble_ids = bubble_ids(&composer);
    let mut chunks = Vec::new();
    let mut content_resolver = CursorContentResolver::new(&connection);
    for (sequence, bubble_id) in bubble_ids.iter().enumerate() {
        let bubble_key = format!("{BUBBLE_PREFIX}{external_session_id}:{bubble_id}");
        let Some(bubble_json) = read_kv_value(&connection, &bubble_key)? else {
            continue;
        };
        let bubble: Value = serde_json::from_str(&bubble_json)
            .with_context(|| format!("failed to parse {source_label} bubble JSON"))?;
        let bubble = content_resolver.resolve_value(&bubble)?;
        if let Some(tool_call) = tool_call_from_bubble(bubble_id, &bubble) {
            let mut chunk = tool_call_chunk(
                external_session_id,
                provider_slug,
                sequence,
                &tool_call,
                &tool_result_text(&bubble),
            );
            chunk.set_source_pointer(
                source_id,
                state_db_path,
                Some(&bubble_key),
                None,
                Some(&tool_call.call_id),
                Some(bubble_id),
            );
            chunks.push(chunk);
            continue;
        }
        let created_at = created_at(&bubble);
        if bubble_is_user(&bubble) {
            if let Some(message) = text(&bubble) {
                let mut chunk = user_message_chunk(
                    external_session_id,
                    provider_slug,
                    sequence,
                    &created_at,
                    &message,
                );
                chunk.set_source_pointer(
                    source_id,
                    state_db_path,
                    Some(&bubble_key),
                    None,
                    None,
                    Some(bubble_id),
                );
                chunks.push(chunk);
            }
        } else if bubble_is_assistant(&bubble) {
            if let Some(message) = text(&bubble) {
                let mut chunk = assistant_message_chunk(
                    external_session_id,
                    provider_slug,
                    sequence,
                    &created_at,
                    &message,
                );
                chunk.set_source_pointer(
                    source_id,
                    state_db_path,
                    Some(&bubble_key),
                    None,
                    None,
                    Some(bubble_id),
                );
                chunks.push(chunk);
            }
        }
    }
    Ok(chunks)
}

pub(in crate::sources) fn cursor_family_state_db_path(
    profile: &SourceProfile,
    source_id: &str,
) -> Result<PathBuf> {
    profile
        .cursor_state_db_path
        .clone()
        .or_else(|| profile.session_db_path.clone())
        .ok_or_else(|| {
            anyhow!("{source_id} source requires cursor_state_db_path or session_db_path")
        })
}

fn attach_subagent_ids_to_parents(sessions: &mut [NativeSourceSession]) {
    let mut parent_by_subagent_id = HashMap::<String, String>::new();
    let mut subagent_ids_by_parent = HashMap::<String, Vec<String>>::new();

    for session in sessions.iter() {
        let Some(metadata) = session.metadata_json.as_ref() else {
            continue;
        };
        if let Some(parent_session_id) = metadata.get("parentSessionId").and_then(Value::as_str) {
            parent_by_subagent_id.insert(
                session.external_session_id.clone(),
                parent_session_id.to_string(),
            );
        }
        if let Some(subagent_ids) = metadata.get("subagentSessionIds").and_then(Value::as_array) {
            for subagent_id in subagent_ids.iter().filter_map(Value::as_str) {
                parent_by_subagent_id
                    .entry(subagent_id.to_string())
                    .or_insert_with(|| session.external_session_id.clone());
            }
        }
    }

    for (subagent_id, parent_session_id) in &parent_by_subagent_id {
        subagent_ids_by_parent
            .entry(parent_session_id.clone())
            .or_default()
            .push(subagent_id.clone());
    }

    for session in sessions.iter_mut() {
        if let Some(parent_session_id) = parent_by_subagent_id.get(&session.external_session_id) {
            session.listable = false;
            let mut metadata = session.metadata_json.take().unwrap_or_else(|| json!({}));
            if metadata.get("kind").and_then(Value::as_str).is_none() {
                metadata["kind"] = json!("subagent");
            }
            metadata["subagentSessionId"] = json!(session.external_session_id);
            metadata["parentSessionId"] = json!(parent_session_id);
            session.metadata_json = Some(metadata);
        }

        let Some(subagent_ids) = subagent_ids_by_parent.remove(&session.external_session_id) else {
            continue;
        };
        let mut metadata = session.metadata_json.take().unwrap_or_else(|| json!({}));
        metadata["subagentSessionIds"] = json!(subagent_ids);
        session.metadata_json = Some(metadata);
    }
}

fn composer_data_session(
    key: &str,
    composer_json: &str,
    state_db_path: &Path,
    source_app_id: &str,
    db_metadata: &fs::Metadata,
    options: &ComposerSessionOptions<'_>,
) -> Result<Option<NativeSourceSession>> {
    let composer: Value = serde_json::from_str(composer_json).with_context(|| {
        format!(
            "failed to parse {} composer data JSON for key {key}",
            options.source_label
        )
    })?;
    let composer_id = composer
        .get("composerId")
        .or_else(|| composer.get("id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            key.strip_prefix(COMPOSER_DATA_PREFIX)
                .map(ToOwned::to_owned)
        })
        .ok_or_else(|| {
            anyhow!(
                "{} composer data key missing composer ID: {key}",
                options.source_label
            )
        })?;
    let Some(mut session) = composer_header_session(
        &composer_id,
        &composer,
        state_db_path,
        source_app_id,
        db_metadata,
        options,
    )?
    else {
        return Ok(None);
    };
    apply_composer_data_flags(&mut session, &composer_id, &composer);
    Ok(Some(session))
}

/// Merges draft/subagent/parent-link listability flags derived from a
/// `composerData:` row into a session. Shared by the composerData-first path
/// and the headers-authoritative path so flag semantics stay identical no
/// matter which path produced the base session.
pub(in crate::sources) fn apply_composer_data_flags(
    session: &mut NativeSourceSession,
    composer_id: &str,
    composer: &Value,
) {
    if let Some(parent_link_metadata) = parent_link_metadata(composer) {
        session.metadata_json = Some(merge_metadata(
            session.metadata_json.take(),
            parent_link_metadata,
        ));
    }
    if let Some(subagent_metadata) = subagent_metadata(composer_id, composer) {
        session.listable = false;
        session.metadata_json = Some(merge_metadata(
            session.metadata_json.take(),
            subagent_metadata,
        ));
    } else if let Some(non_listable_metadata) =
        non_listable_composer_metadata(composer_id, composer)
    {
        session.listable = false;
        session.metadata_json = Some(merge_metadata(
            session.metadata_json.take(),
            non_listable_metadata,
        ));
    }
}

fn parent_link_metadata(composer: &Value) -> Option<Value> {
    let subagent_ids: Vec<&str> = composer
        .get("subagentComposerIds")
        .and_then(Value::as_array)?
        .iter()
        .filter_map(Value::as_str)
        .filter(|subagent_id| !subagent_id.is_empty())
        .collect();
    if subagent_ids.is_empty() {
        return None;
    }
    Some(json!({ "subagentSessionIds": subagent_ids }))
}

fn merge_metadata(base: Option<Value>, extra: Value) -> Value {
    let mut metadata = base.unwrap_or_else(|| json!({}));
    if let (Some(metadata_object), Some(extra_object)) =
        (metadata.as_object_mut(), extra.as_object())
    {
        for (key, value) in extra_object {
            metadata_object.insert(key.clone(), value.clone());
        }
        metadata
    } else {
        extra
    }
}

fn subagent_metadata(composer_id: &str, composer: &Value) -> Option<Value> {
    let subagent_info = composer.get("subagentInfo")?.as_object()?;
    let parent_session_id = subagent_info
        .get("parentComposerId")
        .or_else(|| subagent_info.get("parentSessionId"))
        .and_then(Value::as_str)
        .unwrap_or_default();
    if parent_session_id.is_empty() {
        return None;
    }
    Some(json!({
        "kind": "subagent",
        "subagentSessionId": composer_id,
        "parentSessionId": parent_session_id,
        "cursorToolCallId": subagent_info.get("toolCallId").and_then(Value::as_str),
        "subagentType": subagent_info.get("subagentTypeName").and_then(Value::as_str),
    }))
}

fn non_listable_composer_metadata(composer_id: &str, composer: &Value) -> Option<Value> {
    if composer.get("isDraft").and_then(Value::as_bool) == Some(true) {
        return Some(json!({
            "kind": "draft",
            "composerId": composer_id,
            "reason": "cursor_draft_composer",
        }));
    }
    let has_no_bubbles = bubble_ids(composer).is_empty();
    let has_empty_conversation_map = composer
        .get("conversationMap")
        .and_then(Value::as_object)
        .is_none_or(serde_json::Map::is_empty);
    if composer.get("status").and_then(Value::as_str) == Some("none")
        && has_no_bubbles
        && has_empty_conversation_map
    {
        return Some(json!({
            "kind": "empty_composer",
            "composerId": composer_id,
            "reason": "cursor_empty_composer",
        }));
    }
    if composer.get("status").and_then(Value::as_str) == Some("aborted")
        && has_no_bubbles
        && has_empty_conversation_map
    {
        return Some(json!({
            "kind": "empty_composer",
            "composerId": composer_id,
            "reason": "cursor_aborted_empty_composer",
        }));
    }
    None
}

fn bubble_ids(composer: &Value) -> Vec<String> {
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

fn bubble_is_user(bubble: &Value) -> bool {
    bubble_type(bubble) == Some(BUBBLE_TYPE_USER)
        || bubble.get("role").and_then(Value::as_str) == Some("user")
}

fn bubble_is_assistant(bubble: &Value) -> bool {
    bubble_type(bubble) == Some(BUBBLE_TYPE_ASSISTANT)
        || bubble.get("role").and_then(Value::as_str) == Some("assistant")
}

fn bubble_type(bubble: &Value) -> Option<i64> {
    bubble
        .get("type")
        .and_then(|value| value.as_i64().or_else(|| value.as_str()?.parse().ok()))
}

fn text(bubble: &Value) -> Option<String> {
    ["text", "content", "message", "richText"]
        .iter()
        .filter_map(|key| bubble.get(key))
        .find_map(cursor_value_text)
}

fn tool_call_from_bubble(bubble_id: &str, bubble: &Value) -> Option<ImportedToolCall> {
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
        .map(tool_payload_json)
        .unwrap_or_else(|| json!({}));
    Some(ImportedToolCall {
        call_id: tool
            .get("callId")
            .or_else(|| tool.get("toolCallId"))
            .and_then(Value::as_str)
            .unwrap_or(bubble_id)
            .to_string(),
        raw_name: raw_name.clone(),
        canonical_name: canonical_tool_name(&raw_name),
        args,
        created_at: created_at(bubble),
    })
}

fn tool_payload_json(value: &Value) -> Value {
    value.as_str().map(parse_inner_json).unwrap_or_else(|| {
        if let Some(text) = cursor_value_text(value) {
            parse_inner_json(&text)
        } else {
            value.clone()
        }
    })
}

fn tool_result_text(bubble: &Value) -> String {
    bubble
        .get("toolFormerData")
        .and_then(|tool| tool.get("result").or_else(|| tool.get("output")))
        .and_then(cursor_value_text)
        .or_else(|| text(bubble))
        .unwrap_or_default()
}

fn canonical_tool_name(raw_name: &str) -> String {
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

fn created_at(bubble: &Value) -> String {
    bubble
        .get("createdAt")
        .or_else(|| bubble.get("timestamp"))
        .and_then(time_value_to_rfc3339)
        .unwrap_or_default()
}

fn time_value_to_rfc3339(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        return Some(text.to_string());
    }
    let millis = value_to_u64(value)?;
    let time = UNIX_EPOCH + Duration::from_millis(millis);
    let datetime: chrono::DateTime<chrono::Utc> = time.into();
    Some(datetime.to_rfc3339())
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
    let millis = value_to_u64(value)?;
    Some(UNIX_EPOCH + Duration::from_millis(millis))
}

fn value_to_u64(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|number| u64::try_from(number).ok()))
        .or_else(|| value.as_str()?.parse().ok())
}

fn first_u64(composer: &Value, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .filter_map(|key| composer.get(key))
        .find_map(value_to_u64)
}

fn touched_files_from_composer(composer: &Value) -> Vec<String> {
    let mut touched_files = ["touchedFiles", "filesChanged", "changedFiles"]
        .iter()
        .filter_map(|key| composer.get(key))
        .find_map(string_array)
        .unwrap_or_default();

    if let Some(original_file_states) = composer
        .get("originalFileStates")
        .and_then(Value::as_object)
    {
        touched_files.extend(
            original_file_states
                .keys()
                .map(|path| cursor_file_path(path)),
        );
    }

    if let Some(newly_created_files) = composer.get("newlyCreatedFiles").and_then(string_array) {
        touched_files.extend(newly_created_files);
    }

    touched_files.sort();
    touched_files.dedup();
    touched_files
}

fn string_array(value: &Value) -> Option<Vec<String>> {
    value.as_array().map(|items| {
        items
            .iter()
            .filter_map(|item| {
                item.as_str()
                    .map(cursor_file_path)
                    .or_else(|| {
                        item.get("path")
                            .and_then(Value::as_str)
                            .map(cursor_file_path)
                    })
                    .or_else(|| {
                        item.get("filePath")
                            .and_then(Value::as_str)
                            .map(cursor_file_path)
                    })
                    .or_else(|| {
                        item.get("uri")
                            .and_then(|uri| uri.get("fsPath"))
                            .and_then(Value::as_str)
                            .map(ToOwned::to_owned)
                    })
            })
            .collect()
    })
}

fn cursor_file_path(value: &str) -> String {
    value.strip_prefix("file://").unwrap_or(value).to_string()
}

fn normalize_title(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
