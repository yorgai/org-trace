use anyhow::Result;

use std::path::Path;

use crate::{
    list_native_source_sessions, ActivityChunk, NativeSourceSession, SourcePlanWithEdgesUpsert,
    SourceProfile,
};

use super::{claude_code, codex_app, cursor_ide, gemini, opencode, orgii, windsurf};

const SOURCE_CLAUDE_CODE: &str = "claude_code";
const SOURCE_CODEX_APP: &str = "codex_app";
const SOURCE_CURSOR_IDE: &str = "cursor_ide";
const SOURCE_OPENCODE: &str = "opencode";
const SOURCE_WINDSURF: &str = "windsurf";
const SOURCE_ORGII: &str = "orgii";
const SOURCE_GEMINI: &str = "gemini";

/// Lists native sessions through the app-specific provider for a source profile.
pub fn list_source_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    match profile.name.as_str() {
        SOURCE_CLAUDE_CODE => claude_code::list_sessions(profile, limit),
        SOURCE_CODEX_APP => codex_app::list_sessions(profile, limit),
        SOURCE_CURSOR_IDE => cursor_ide::list_sessions(profile, limit),
        SOURCE_OPENCODE => opencode::list_sessions(profile, limit),
        SOURCE_WINDSURF => windsurf::list_sessions(profile, limit),
        SOURCE_ORGII => orgii::list_sessions(profile, limit),
        SOURCE_GEMINI => gemini::list_sessions(profile, limit),
        _ => list_native_source_sessions(profile, limit),
    }
}

/// Lists source plans and recovered plan-session edges through supported providers.
pub fn list_source_plans(profile: &SourceProfile) -> Result<Vec<SourcePlanWithEdgesUpsert>> {
    match profile.name.as_str() {
        SOURCE_CURSOR_IDE => cursor_ide::list_plans(profile),
        _ => Ok(Vec::new()),
    }
}

/// Formats source records as activity chunk JSON for one source session when supported.
pub fn format_source_session_chunks(
    source_id: &str,
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    match source_id {
        SOURCE_CLAUDE_CODE => claude_code::format_chunks(external_session_id, source_path),
        SOURCE_CODEX_APP => codex_app::format_chunks(external_session_id, source_path),
        SOURCE_CURSOR_IDE => cursor_ide::format_chunks(external_session_id, source_path),
        SOURCE_OPENCODE => opencode::format_chunks(external_session_id, source_path),
        SOURCE_WINDSURF => windsurf::format_chunks(external_session_id, source_path),
        SOURCE_ORGII => orgii::format_chunks(external_session_id, source_path),
        SOURCE_GEMINI => gemini::format_chunks(external_session_id, source_path),
        _ => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use brick_protocol::ActorType;

    use super::*;
    use crate::GENERIC_NATIVE_FILE_PARSER_VERSION;

    fn temp_source_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-source-dispatch-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create source dispatch root");
        path
    }

    fn profile(name: &str, session_log_path: PathBuf) -> SourceProfile {
        SourceProfile {
            name: name.to_string(),
            app_id: Some(name.to_string()),
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
    fn dispatches_claude_code_to_app_specific_provider() {
        let root = temp_source_root("claude");
        fs::write(
            root.join("session-1.jsonl"),
            "{\"type\":\"assistant\",\"timestamp\":\"2026-06-18T01:00:00Z\",\"message\":{\"model\":\"claude-sonnet\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}}\n",
        )
        .expect("write claude fixture");
        let source_profile = profile(SOURCE_CLAUDE_CODE, root);

        let sessions =
            list_source_sessions(&source_profile, Some(10)).expect("list claude sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].parser_version, "claude-code-jsonl-v4");
        assert_eq!(sessions[0].model.as_deref(), Some("claude-sonnet"));
        assert_eq!(sessions[0].input_tokens, Some(11));
        assert_eq!(sessions[0].output_tokens, Some(7));
    }

    #[test]
    fn unknown_sources_use_generic_file_provider() {
        let root = temp_source_root("generic");
        fs::write(root.join("session-1.jsonl"), "{\"message\":\"hello\"}\n")
            .expect("write generic fixture");
        let source_profile = profile("custom_source", root);

        let sessions =
            list_source_sessions(&source_profile, Some(10)).expect("list generic sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].parser_version,
            GENERIC_NATIVE_FILE_PARSER_VERSION
        );
        assert_eq!(sessions[0].source_app_id, "custom_source");
    }
}
