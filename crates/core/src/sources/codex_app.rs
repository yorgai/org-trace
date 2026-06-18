use std::collections::BTreeSet;
use std::path::Path;

use anyhow::Result;
use serde_json::Value;

use crate::{list_file_source_sessions, NativeSessionMetadata, NativeSourceSession, SourceProfile};

use super::jsonl::{
    read_jsonl_values, set_first_path, set_first_string, text_from_value, token_value,
    truncate_title, update_session_times,
};

const CODEX_APP_JSONL_PARSER_VERSION: &str = "codex-app-jsonl-v1";

#[derive(Debug, Default)]
struct PatchImpact {
    touched_files: BTreeSet<String>,
    lines_added: u64,
    lines_removed: u64,
}

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
    for line in patch.lines() {
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

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use super::*;

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

        let sessions = list_sessions(&profile(root), Some(10)).expect("list sessions");

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
}
