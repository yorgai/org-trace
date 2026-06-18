use std::path::Path;

use anyhow::Result;

use crate::{ActivityChunk, NativeSourceSession, SourceProfile};

use super::cursor_family::{
    format_chunks as format_cursor_family_chunks, list_sessions_from_composer_data,
    ComposerSessionOptions,
};

const WINDSURF_SOURCE_ID: &str = "windsurf";
const WINDSURF_PARSER_VERSION: &str = "windsurf-composer-data-v1";
const WINDSURF_PROVIDER_SLUG: &str = "windsurf";

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    list_sessions_from_composer_data(
        profile,
        limit,
        ComposerSessionOptions {
            source_id: WINDSURF_SOURCE_ID,
            parser_version: WINDSURF_PARSER_VERSION,
            source_label: "Windsurf",
            include_context_tokens: true,
            skip_best_of_n: true,
        },
    )
}

pub(super) fn format_chunks(
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    format_cursor_family_chunks(
        external_session_id,
        source_path,
        WINDSURF_SOURCE_ID,
        WINDSURF_PROVIDER_SLUG,
        "Windsurf",
    )
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use rusqlite::Connection;

    use super::*;
    use crate::{
        ACTION_TYPE_ASSISTANT, ACTION_TYPE_RAW, ACTION_TYPE_TOOL_CALL, FUNCTION_ASSISTANT,
        FUNCTION_EDIT_FILE, FUNCTION_USER_MESSAGE,
    };

    fn temp_state_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "brick-windsurf-state-{name}-{}.vscdb",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    fn profile(path: PathBuf) -> SourceProfile {
        SourceProfile {
            name: WINDSURF_SOURCE_ID.to_string(),
            app_id: Some(WINDSURF_SOURCE_ID.to_string()),
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
    fn extracts_sessions_from_windsurf_composer_data_rows() {
        let path = temp_state_db("metadata");
        let connection = Connection::open(&path).expect("open temp windsurf state DB");
        connection
            .execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .expect("create cursorDiskKV");
        let composer = serde_json::json!({
            "composerId": "windsurf-composer-1",
            "name": "Implement Windsurf provider",
            "createdAt": 1_766_100_000_000_u64,
            "lastUpdatedAt": 1_766_100_060_000_u64,
            "modelConfig": { "modelName": "windsurf-model" },
            "contextTokensUsed": 42_000_u64,
            "trackedGitRepos": [
                {
                    "repoPath": "/workspace/windsurf-repo",
                    "branches": [{ "branchName": "feature/windsurf" }]
                }
            ],
            "workspaceIdentifier": {
                "uri": { "fsPath": "/workspace/fallback" }
            },
            "filesChangedCount": 4,
            "totalLinesAdded": 120,
            "totalLinesRemoved": 9,
            "touchedFiles": ["src/provider.rs", { "path": "README.md" }],
            "subagentInfo": { "kind": "worker" }
        });
        let older = serde_json::json!({
            "composerId": "windsurf-composer-older",
            "name": "Older session",
            "createdAt": 1_766_000_000_000_u64,
            "lastUpdatedAt": 1_766_000_000_000_u64
        });
        let subcomposer = serde_json::json!({
            "composerId": "windsurf-subcomposer",
            "name": "Subcomposer",
            "isBestOfNSubcomposer": true
        });
        let rows = [
            ("composerData:windsurf-composer-1", composer),
            ("composerData:windsurf-composer-older", older),
            ("composerData:windsurf-subcomposer", subcomposer),
        ];
        for (key, value) in rows {
            connection
                .execute(
                    "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                    (key, value.to_string()),
                )
                .expect("insert windsurf composer row");
        }
        drop(connection);

        let sessions = list_sessions(&profile(path), Some(10)).expect("list windsurf sessions");

        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].external_session_id, "windsurf-composer-1");
        assert_eq!(
            sessions[0].title.as_deref(),
            Some("Implement Windsurf provider")
        );
        assert_eq!(
            sessions[0].repo_path.as_deref(),
            Some(Path::new("/workspace/windsurf-repo"))
        );
        assert_eq!(sessions[0].branch.as_deref(), Some("feature/windsurf"));
        assert_eq!(sessions[0].model.as_deref(), Some("windsurf-model"));
        assert_eq!(sessions[0].input_tokens, Some(42_000));
        assert_eq!(sessions[0].output_tokens, None);
        assert_eq!(sessions[0].files_changed, Some(4));
        assert_eq!(sessions[0].lines_added, Some(120));
        assert_eq!(sessions[0].lines_removed, Some(9));
        assert_eq!(
            sessions[0].touched_files,
            vec!["src/provider.rs".to_string(), "README.md".to_string()]
        );
        assert_eq!(sessions[0].parser_version, WINDSURF_PARSER_VERSION);
    }

    #[test]
    fn formats_windsurf_composer_bubbles_with_shared_cursor_family_logic() {
        let path = temp_state_db("chunks");
        let connection = Connection::open(&path).expect("open temp windsurf state DB");
        connection
            .execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .expect("create cursorDiskKV");
        let composer = serde_json::json!({
            "composerId": "windsurf-composer-1",
            "fullConversationHeadersOnly": [
                { "bubbleId": "user-1" },
                { "bubbleId": "assistant-1" },
                { "bubbleId": "tool-1" }
            ]
        });
        let user_bubble = serde_json::json!({
            "bubbleId": "user-1",
            "type": 1,
            "createdAt": 1_766_100_000_000_u64,
            "text": "please update the provider"
        });
        let assistant_bubble = serde_json::json!({
            "bubbleId": "assistant-1",
            "type": 2,
            "createdAt": 1_766_100_001_000_u64,
            "text": "I will make the change."
        });
        let tool_bubble = serde_json::json!({
            "bubbleId": "tool-1",
            "type": 2,
            "createdAt": 1_766_100_002_000_u64,
            "toolFormerData": {
                "name": "edit_file_v2",
                "params": { "target_file": "src/provider.rs" },
                "result": "updated"
            }
        });
        let rows = [
            ("composerData:windsurf-composer-1", composer),
            ("bubbleId:windsurf-composer-1:user-1", user_bubble),
            ("bubbleId:windsurf-composer-1:assistant-1", assistant_bubble),
            ("bubbleId:windsurf-composer-1:tool-1", tool_bubble),
        ];
        for (key, value) in rows {
            connection
                .execute(
                    "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                    (key, value.to_string()),
                )
                .expect("insert windsurf KV row");
        }
        drop(connection);

        let chunks =
            format_chunks("windsurf-composer-1", Some(&path)).expect("format windsurf chunks");

        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].action_type, ACTION_TYPE_RAW);
        assert_eq!(chunks[0].function, FUNCTION_USER_MESSAGE);
        assert_eq!(chunks[1].action_type, ACTION_TYPE_ASSISTANT);
        assert_eq!(chunks[1].function, FUNCTION_ASSISTANT);
        assert_eq!(chunks[2].action_type, ACTION_TYPE_TOOL_CALL);
        assert_eq!(chunks[2].function, FUNCTION_EDIT_FILE);
        assert_eq!(chunks[2].args["target_file"], "src/provider.rs");
        assert_eq!(chunks[2].result["output"], "updated");
    }
}
