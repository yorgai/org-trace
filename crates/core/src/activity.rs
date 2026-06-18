//! External history chunk JSON formatting helpers.

use std::path::Path;

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;

pub const ACTION_TYPE_RAW: &str = "raw";
pub const ACTION_TYPE_ASSISTANT: &str = "assistant";
pub const ACTION_TYPE_THINKING: &str = "thinking";
pub const ACTION_TYPE_TOOL_CALL: &str = "tool_call";
pub const FUNCTION_USER_MESSAGE: &str = "user_message";
pub const FUNCTION_ASSISTANT: &str = "assistant";
pub const FUNCTION_THINKING: &str = "thinking";
pub const FUNCTION_RUN_COMMAND_LINE: &str = "run_command_line";
pub const FUNCTION_EDIT_FILE: &str = "edit_file_by_replace";
pub const IMPORTED_STATUS_COMPLETED: &str = "completed";

/// One normalized activity chunk, matching ORGII's canonical wire shape.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ActivityChunk {
    pub chunk_id: String,
    pub session_id: String,
    pub action_type: String,
    pub function: String,
    pub args: Value,
    pub result: Value,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thread_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub process_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_record_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_line_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_message_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_part_id: Option<String>,
}

impl ActivityChunk {
    pub fn new(session_id: &str, action_type: &str, function: &str) -> Self {
        Self {
            chunk_id: Uuid::new_v4().to_string(),
            session_id: session_id.to_string(),
            action_type: action_type.to_string(),
            function: function.to_string(),
            args: json!({}),
            result: json!({}),
            created_at: chrono::Utc::now().to_rfc3339(),
            thread_id: None,
            process_id: None,
            source_id: None,
            source_path: None,
            source_record_key: None,
            source_line_number: None,
            source_message_id: None,
            source_part_id: None,
        }
    }

    pub fn set_source_pointer(
        &mut self,
        source_id: &str,
        source_path: &Path,
        source_record_key: Option<&str>,
        source_line_number: Option<u64>,
        source_message_id: Option<&str>,
        source_part_id: Option<&str>,
    ) {
        self.source_id = Some(source_id.to_string());
        self.source_path = Some(source_path.display().to_string());
        self.source_record_key = source_record_key.map(ToOwned::to_owned);
        self.source_line_number = source_line_number;
        self.source_message_id = source_message_id.map(ToOwned::to_owned);
        self.source_part_id = source_part_id.map(ToOwned::to_owned);
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ImportedToolCall {
    pub call_id: String,
    pub raw_name: String,
    pub canonical_name: String,
    pub args: Value,
    pub created_at: String,
}

pub fn user_message_chunk(
    session_id: &str,
    provider_slug: &str,
    sequence: usize,
    created_at: &str,
    message: &str,
) -> ActivityChunk {
    let mut chunk = ActivityChunk::new(session_id, ACTION_TYPE_RAW, FUNCTION_USER_MESSAGE);
    chunk.chunk_id = format!("{provider_slug}-user-{sequence}");
    chunk.created_at = normalize_created_at(created_at);
    chunk.result = json!({
        "type": "user",
        "message": { "content": message, "role": "user" },
    });
    chunk
}

pub fn assistant_message_chunk(
    session_id: &str,
    provider_slug: &str,
    sequence: usize,
    created_at: &str,
    message: &str,
) -> ActivityChunk {
    let mut chunk = ActivityChunk::new(session_id, ACTION_TYPE_ASSISTANT, FUNCTION_ASSISTANT);
    chunk.chunk_id = format!("{provider_slug}-asst-{sequence}");
    chunk.created_at = normalize_created_at(created_at);
    chunk.result = json!({
        "observation": message,
        "content": message,
        "role": "assistant",
        "is_delta": false,
        "is_full_content": true,
    });
    chunk
}

pub fn thinking_chunk(
    session_id: &str,
    provider_slug: &str,
    sequence: usize,
    created_at: &str,
    thought: &str,
) -> ActivityChunk {
    let mut chunk = ActivityChunk::new(session_id, ACTION_TYPE_THINKING, FUNCTION_THINKING);
    chunk.chunk_id = format!("{provider_slug}-thinking-{sequence}");
    chunk.created_at = normalize_created_at(created_at);
    chunk.result = json!({
        "thought": thought,
        "content": thought,
        "observation": thought,
        "is_delta": false,
    });
    chunk
}

pub fn tool_call_chunk(
    session_id: &str,
    provider_slug: &str,
    sequence: usize,
    call: &ImportedToolCall,
    output: &str,
) -> ActivityChunk {
    let mut chunk = ActivityChunk::new(session_id, ACTION_TYPE_TOOL_CALL, &call.canonical_name);
    chunk.chunk_id = format!("{provider_slug}-tool-{sequence}-{}", call.call_id);
    chunk.created_at = normalize_created_at(&call.created_at);
    chunk.args = call.args.clone();
    chunk.result = json!({
        "success": true,
        "status": IMPORTED_STATUS_COMPLETED,
        "call_id": call.call_id,
        "output": output,
        "observation": output,
        "raw_tool_name": call.raw_name,
    });
    chunk
}

pub fn parse_inner_json(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    serde_json::from_str(raw).unwrap_or_else(|_| json!({ "input": raw }))
}

pub fn normalize_created_at(raw: &str) -> String {
    if raw.is_empty() {
        return chrono::Utc::now().to_rfc3339();
    }
    if let Ok(datetime) = chrono::DateTime::parse_from_rfc3339(raw) {
        datetime.with_timezone(&chrono::Utc).to_rfc3339()
    } else {
        raw.to_string()
    }
}
