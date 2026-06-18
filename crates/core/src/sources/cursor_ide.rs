use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use crate::{NativeSourceSession, SourceProfile};

use super::cursor_family::{open_state_db, read_kv_value};

const CURSOR_IDE_SOURCE_ID: &str = "cursor_ide";
const CURSOR_COMPOSER_HEADERS_KEY: &str = "composer.composerHeaders";
const CURSOR_IDE_HEADERS_PARSER_VERSION: &str = "cursor-ide-composer-headers-v1";
const TITLE_LIMIT: usize = 200;

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
}
