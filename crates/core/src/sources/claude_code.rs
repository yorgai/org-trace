use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::{list_file_source_sessions, NativeSessionMetadata, NativeSourceSession, SourceProfile};

use super::jsonl::{
    read_jsonl_values, set_first_path, set_first_string, set_first_string_value, text_from_value,
    token_value, truncate_title, update_session_times,
};

const CLAUDE_CODE_JSONL_PARSER_VERSION: &str = "claude-code-jsonl-v1";

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    list_file_source_sessions(profile, limit, extract_jsonl_metadata)
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

    for value in lines {
        update_session_times(&mut metadata, value.get("timestamp"));
        set_first_path(&mut metadata.repo_path, value.get("cwd"));
        set_first_string(&mut metadata.branch, value.get("gitBranch"));

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
    }

    metadata.input_tokens = saw_input_tokens.then_some(input_tokens);
    metadata.output_tokens = saw_output_tokens.then_some(output_tokens);
    Ok(metadata)
}

fn is_user_message(value: &Value, message: Option<&Value>) -> bool {
    value.get("type").and_then(Value::as_str) == Some("user")
        || message.and_then(|message_value| message_value.get("role"))
            == Some(&Value::String("user".to_string()))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;

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
}
