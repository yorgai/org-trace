use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use crate::{
    ActivityChunk, NativeSourceSession, SourcePlanSessionEdgeRole, SourcePlanSessionEdgeUpsert,
    SourcePlanUpsert, SourcePlanWithEdgesUpsert, SourceProfile,
};

use super::cursor_family::{
    apply_composer_data_flags, composer_header_session, cursor_family_state_db_path,
    format_chunks as format_cursor_family_chunks, list_sessions_from_composer_data, open_state_db,
    read_kv_value, ComposerSessionOptions,
};

const CURSOR_IDE_SOURCE_ID: &str = "cursor_ide";
const CURSOR_COMPOSER_HEADERS_KEY: &str = "composer.composerHeaders";
const CURSOR_COMPOSER_DATA_PREFIX: &str = "composerData:";
const CURSOR_PLAN_REGISTRY_KEY: &str = "composer.planRegistry";
const CURSOR_IDE_COMPOSER_DATA_PARSER_VERSION: &str = "cursor-ide-composer-data-v1";
const CURSOR_IDE_HEADERS_PARSER_VERSION: &str = "cursor-ide-composer-headers-v1";
const CURSOR_IDE_PLAN_REGISTRY_PARSER_VERSION: &str = "cursor-ide-plan-registry-v1";
const CURSOR_IDE_PROVIDER_SLUG: &str = CURSOR_IDE_SOURCE_ID;

pub(super) fn list_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<&str>,
) -> Result<Vec<NativeSourceSession>> {
    // Composer headers are the authoritative session list and carry the rich
    // metadata (name, model, repo, branch, impact stats). The per-composer
    // `composerData:` rows only hold conversation bubbles plus the
    // draft/subagent/parent listability flags, so we merge those flags onto the
    // header-derived sessions. When headers are absent (older Cursor state DBs)
    // we fall back to the composerData-only path.
    let header_sessions = list_sessions_from_composer_headers(profile, limit)?;
    if !header_sessions.is_empty() {
        return Ok(crate::filter_sessions_since(header_sessions, since));
    }
    let options = ComposerSessionOptions {
        source_id: CURSOR_IDE_SOURCE_ID,
        parser_version: CURSOR_IDE_COMPOSER_DATA_PARSER_VERSION,
        source_label: "Cursor",
        include_context_tokens: true,
        skip_best_of_n: true,
    };
    let sessions = list_sessions_from_composer_data(profile, limit, options)?;
    Ok(crate::filter_sessions_since(sessions, since))
}

fn list_sessions_from_composer_headers(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    let state_db_path = cursor_family_state_db_path(profile, CURSOR_IDE_SOURCE_ID)?;
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
    let options = ComposerSessionOptions {
        source_id: CURSOR_IDE_SOURCE_ID,
        parser_version: CURSOR_IDE_HEADERS_PARSER_VERSION,
        source_label: "Cursor",
        include_context_tokens: false,
        skip_best_of_n: true,
    };
    let mut sessions = all_composers
        .iter()
        .filter_map(|(composer_id, composer)| {
            composer_header_session(
                composer_id,
                composer,
                &state_db_path,
                &source_app_id,
                &db_metadata,
                &options,
            )
            .transpose()
        })
        .collect::<Result<Vec<_>>>()?;
    // Merge draft/subagent/parent-link flags from each session's composerData
    // row; headers alone do not expose listability state.
    for session in sessions.iter_mut() {
        let composer_data_key = format!(
            "{CURSOR_COMPOSER_DATA_PREFIX}{}",
            session.external_session_id
        );
        let Some(composer_data_json) = read_kv_value(&connection, &composer_data_key)? else {
            continue;
        };
        let Ok(composer_data) = serde_json::from_str::<Value>(&composer_data_json) else {
            continue;
        };
        apply_composer_data_flags(
            session,
            &session.external_session_id.clone(),
            &composer_data,
        );
    }
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
    format_cursor_family_chunks(
        external_session_id,
        source_path,
        CURSOR_IDE_SOURCE_ID,
        CURSOR_IDE_PROVIDER_SLUG,
        "Cursor IDE",
    )
}

pub(super) fn list_plans(profile: &SourceProfile) -> Result<Vec<SourcePlanWithEdgesUpsert>> {
    let state_db_path = cursor_family_state_db_path(profile, CURSOR_IDE_SOURCE_ID)?;
    let connection = open_state_db(&state_db_path)?;
    let plan_entries = read_plan_registry_entries(&connection)?;
    if plan_entries.is_empty() {
        return Ok(Vec::new());
    }
    let db_metadata = fs::metadata(&state_db_path).with_context(|| {
        format!(
            "failed to read Cursor state DB metadata at {}",
            state_db_path.display()
        )
    })?;
    let now = chrono::Utc::now();
    let source_mtime = db_metadata
        .modified()
        .ok()
        .map(chrono::DateTime::<chrono::Utc>::from);
    let source_id = profile.name.clone();
    let mut plans = Vec::new();
    for (plan_id, plan) in plan_entries {
        let source_path = plan_path(&plan);
        let source_uri = plan_uri(&plan).or_else(|| {
            source_path
                .as_ref()
                .map(|path| format!("file://{}", path.display()))
        });
        let title = plan_title(&plan).or_else(|| Some(plan_id.clone()));
        let edges = plan_session_edges(&source_id, &plan_id, &plan, now);
        plans.push(SourcePlanWithEdgesUpsert {
            plan: SourcePlanUpsert {
                source_id: source_id.clone(),
                external_plan_id: plan_id,
                title,
                source_path,
                source_uri,
                source_mtime,
                parser_version: Some(CURSOR_IDE_PLAN_REGISTRY_PARSER_VERSION.to_string()),
                discovered_at: now,
                last_seen_at: now,
                metadata_json: Some(json!({
                    "sourceRecordKey": CURSOR_PLAN_REGISTRY_KEY,
                    "raw": plan,
                })),
            },
            edges,
        });
    }
    Ok(plans)
}

fn read_plan_registry_entries(connection: &rusqlite::Connection) -> Result<Vec<(String, Value)>> {
    if let Some(registry_json) = read_kv_value(connection, CURSOR_PLAN_REGISTRY_KEY)? {
        let registry: Value = serde_json::from_str(&registry_json)
            .context("failed to parse Cursor plan registry JSON")?;
        if let Some(object) = registry.as_object() {
            if let Some(plans) = object.get("plans").and_then(Value::as_object) {
                return Ok(plans
                    .iter()
                    .filter_map(|(plan_id, plan)| plan_object(plan_id, plan))
                    .collect());
            }
            return Ok(object
                .iter()
                .filter_map(|(plan_id, plan)| plan_object(plan_id, plan))
                .collect());
        }
    }
    Ok(Vec::new())
}

fn plan_object(plan_id: &str, plan: &Value) -> Option<(String, Value)> {
    plan.as_object()?;
    Some((
        plan.get("planId")
            .or_else(|| plan.get("id"))
            .and_then(Value::as_str)
            .unwrap_or(plan_id)
            .to_string(),
        plan.clone(),
    ))
}

fn plan_session_edges(
    source_id: &str,
    plan_id: &str,
    plan: &Value,
    seen_at: chrono::DateTime<chrono::Utc>,
) -> Vec<SourcePlanSessionEdgeUpsert> {
    let mut edges: BTreeMap<(String, SourcePlanSessionEdgeRole), SourcePlanSessionEdgeUpsert> =
        BTreeMap::new();
    if let Some(session_id) = plan.get("createdBy").and_then(Value::as_str) {
        insert_plan_edge(
            &mut edges,
            source_id,
            plan_id,
            session_id,
            SourcePlanSessionEdgeRole::CreatedBy,
            None,
            seen_at,
        );
    }
    for session_id in plan
        .get("editedBy")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        insert_plan_edge(
            &mut edges,
            source_id,
            plan_id,
            session_id,
            SourcePlanSessionEdgeRole::EditedBy,
            None,
            seen_at,
        );
    }
    for session_id in plan
        .get("referencedBy")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
    {
        insert_plan_edge(
            &mut edges,
            source_id,
            plan_id,
            session_id,
            SourcePlanSessionEdgeRole::ReferencedBy,
            None,
            seen_at,
        );
    }
    if let Some(built_by) = plan.get("builtBy").and_then(Value::as_object) {
        for (session_id, todo_ids) in built_by {
            insert_plan_edge(
                &mut edges,
                source_id,
                plan_id,
                session_id,
                SourcePlanSessionEdgeRole::BuiltBy,
                Some(todo_ids.clone()),
                seen_at,
            );
        }
    }
    edges.into_values().collect()
}

fn insert_plan_edge(
    edges: &mut BTreeMap<(String, SourcePlanSessionEdgeRole), SourcePlanSessionEdgeUpsert>,
    source_id: &str,
    plan_id: &str,
    session_id: &str,
    role: SourcePlanSessionEdgeRole,
    todo_ids_json: Option<Value>,
    seen_at: chrono::DateTime<chrono::Utc>,
) {
    if session_id.trim().is_empty() {
        return;
    }
    edges.insert(
        (session_id.to_string(), role),
        SourcePlanSessionEdgeUpsert {
            source_id: source_id.to_string(),
            external_plan_id: plan_id.to_string(),
            external_session_id: session_id.to_string(),
            role,
            todo_ids_json,
            discovered_at: seen_at,
            last_seen_at: seen_at,
            metadata_json: None,
        },
    );
}

fn plan_path(plan: &Value) -> Option<PathBuf> {
    plan.get("uri")
        .and_then(|uri| uri.get("fsPath").or_else(|| uri.get("path")))
        .and_then(Value::as_str)
        .or_else(|| plan.get("path").and_then(Value::as_str))
        .map(PathBuf::from)
}

fn plan_uri(plan: &Value) -> Option<String> {
    let uri = plan.get("uri")?;
    if let Some(external) = uri.get("external").and_then(Value::as_str) {
        return Some(external.to_string());
    }
    let scheme = uri.get("scheme").and_then(Value::as_str)?;
    let path = uri
        .get("fsPath")
        .or_else(|| uri.get("path"))
        .and_then(Value::as_str)?;
    Some(format!("{scheme}://{path}"))
}

fn plan_title(plan: &Value) -> Option<String> {
    plan.get("title")
        .or_else(|| plan.get("name"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
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

    fn create_cursor_kv_db(path: &Path) -> Connection {
        let connection = Connection::open(path).expect("open temp cursor state DB");
        connection
            .execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
            .expect("create cursorDiskKV");
        connection
    }

    fn insert_cursor_kv(connection: &Connection, key: &str, value: Value) {
        connection
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                (key, value.to_string()),
            )
            .expect("insert cursor KV row");
    }

    #[test]
    fn extracts_plans_and_edges_from_compact_plan_registry() {
        let path = temp_state_db("plan-registry");
        let connection = create_cursor_kv_db(&path);
        let registry = serde_json::json!({
            "plan-a": {
                "title": "Ship plan indexing",
                "uri": {
                    "scheme": "file",
                    "fsPath": "/Users/example/.cursor/plans/plan-a.plan.md"
                },
                "createdBy": "session-created",
                "editedBy": ["session-edited", "session-created"],
                "referencedBy": ["session-referenced", "missing-header-session"],
                "builtBy": {
                    "session-built": ["todo-1", "todo-2"]
                }
            }
        });
        insert_cursor_kv(&connection, CURSOR_PLAN_REGISTRY_KEY, registry);
        drop(connection);

        let plans = list_plans(&profile(path)).expect("list cursor plans");

        assert_eq!(plans.len(), 1);
        assert_eq!(plans[0].plan.external_plan_id, "plan-a");
        assert_eq!(plans[0].plan.title.as_deref(), Some("Ship plan indexing"));
        assert_eq!(
            plans[0].plan.source_path.as_deref(),
            Some(Path::new("/Users/example/.cursor/plans/plan-a.plan.md"))
        );
        assert_eq!(plans[0].edges.len(), 6);
        assert!(plans[0].edges.iter().any(|edge| {
            edge.external_session_id == "missing-header-session"
                && edge.role == SourcePlanSessionEdgeRole::ReferencedBy
        }));
        let built_edge = plans[0]
            .edges
            .iter()
            .find(|edge| {
                edge.external_session_id == "session-built"
                    && edge.role == SourcePlanSessionEdgeRole::BuiltBy
            })
            .expect("built edge");
        assert_eq!(built_edge.todo_ids_json, Some(json!(["todo-1", "todo-2"])));
    }

    #[test]
    fn ignores_per_plan_registry_keys_to_avoid_broad_scan() {
        let path = temp_state_db("plan-registry-keys");
        let connection = create_cursor_kv_db(&path);
        let plan = serde_json::json!({
            "planId": "plan-keyed",
            "name": "Per key plan",
            "path": "/tmp/plan-keyed.plan.md",
            "createdBy": "session-created",
            "builtBy": {
                "session-built": ["todo-a"]
            }
        });
        insert_cursor_kv(&connection, "composer.planRegistry.plan-keyed", plan);
        drop(connection);

        let plans = list_plans(&profile(path)).expect("list cursor plans");

        assert_eq!(plans.len(), 0);
    }

    #[test]
    fn extracts_sessions_from_cursor_composer_data() {
        let path = temp_state_db("composer-data-sessions");
        let connection = create_cursor_kv_db(&path);
        let composer = serde_json::json!({
            "composerId": "composer-data-1",
            "name": "Implement from composer data",
            "createdAt": 1_766_000_000_000_u64,
            "lastUpdatedAt": 1_766_000_060_000_u64,
            "workspaceIdentifier": {
                "uri": { "fsPath": "/workspace/from-composer-data" }
            },
            "trackedGitRepos": [
                {
                    "repoPath": "/workspace/repo",
                    "branches": [{ "branchName": "main" }]
                }
            ],
            "modelConfig": { "modelName": "cursor-model" },
            "contextTokensUsed": 42,
            "filesChangedCount": 2,
            "totalLinesAdded": 10,
            "totalLinesRemoved": 3,
            "originalFileStates": {
                "file:///workspace/repo/src/lib.rs": {
                    "firstEditBubbleId": "bubble-1"
                }
            },
            "newlyCreatedFiles": [
                {
                    "uri": {
                        "fsPath": "/workspace/repo/README.md"
                    }
                }
            ]
        });
        let subagent = serde_json::json!({
            "composerId": "composer-subagent-1",
            "name": "Subagent worker",
            "createdAt": 1_766_000_010_000_u64,
            "lastUpdatedAt": 1_766_000_020_000_u64,
            "subagentInfo": {
                "subagentTypeName": "general",
                "parentComposerId": "composer-data-1",
                "toolCallId": "tool-call-1"
            }
        });
        insert_cursor_kv(&connection, "composerData:composer-data-1", composer);
        insert_cursor_kv(&connection, "composerData:composer-subagent-1", subagent);
        drop(connection);

        let sessions = list_sessions(&profile(path), Some(10), None).expect("list cursor sessions");

        assert_eq!(sessions.len(), 2);
        let parent = sessions
            .iter()
            .find(|session| session.external_session_id == "composer-data-1")
            .expect("parent composer session");
        let subagent = sessions
            .iter()
            .find(|session| session.external_session_id == "composer-subagent-1")
            .expect("subagent composer session");
        assert_eq!(
            parent.title.as_deref(),
            Some("Implement from composer data")
        );
        assert_eq!(parent.input_tokens, Some(42));
        assert_eq!(
            parent.touched_files,
            vec![
                "/workspace/repo/README.md".to_string(),
                "/workspace/repo/src/lib.rs".to_string()
            ]
        );
        assert!(parent.listable);
        assert_eq!(
            parent
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("subagentSessionIds")),
            Some(&json!(["composer-subagent-1"]))
        );
        assert!(!subagent.listable);
        assert_eq!(
            subagent
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("parentSessionId"))
                .and_then(Value::as_str),
            Some("composer-data-1")
        );
        assert_eq!(
            parent.parser_version,
            CURSOR_IDE_COMPOSER_DATA_PARSER_VERSION
        );
    }

    #[test]
    fn links_cursor_subagents_from_parent_side_ids() {
        let path = temp_state_db("parent-side-subagents");
        let connection = create_cursor_kv_db(&path);
        let parent = serde_json::json!({
            "composerId": "parent-composer-1",
            "name": "Parent composer",
            "createdAt": 1_766_000_000_000_u64,
            "lastUpdatedAt": 1_766_000_060_000_u64,
            "subagentComposerIds": ["child-composer-1"],
            "modelConfig": { "modelName": "cursor-model" }
        });
        let child = serde_json::json!({
            "composerId": "child-composer-1",
            "name": "Child composer",
            "createdAt": 1_766_000_010_000_u64,
            "lastUpdatedAt": 1_766_000_020_000_u64,
            "modelConfig": { "modelName": "cursor-model" }
        });
        insert_cursor_kv(&connection, "composerData:parent-composer-1", parent);
        insert_cursor_kv(&connection, "composerData:child-composer-1", child);
        drop(connection);

        let sessions = list_sessions(&profile(path), Some(10), None).expect("list cursor sessions");
        let parent = sessions
            .iter()
            .find(|session| session.external_session_id == "parent-composer-1")
            .expect("parent composer");
        let child = sessions
            .iter()
            .find(|session| session.external_session_id == "child-composer-1")
            .expect("child composer");

        assert!(parent.listable);
        assert!(!child.listable);
        assert_eq!(
            parent
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("subagentSessionIds")),
            Some(&json!(["child-composer-1"]))
        );
        assert_eq!(
            child
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("kind"))
                .and_then(Value::as_str),
            Some("subagent")
        );
        assert_eq!(
            child
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("parentSessionId"))
                .and_then(Value::as_str),
            Some("parent-composer-1")
        );
    }

    #[test]
    fn marks_cursor_aborted_empty_composers_non_listable() {
        let path = temp_state_db("aborted-empty-composer");
        let connection = create_cursor_kv_db(&path);
        let aborted_composer = serde_json::json!({
            "composerId": "aborted-empty-composer-1",
            "name": "Aborted empty composer",
            "status": "aborted",
            "isDraft": false,
            "fullConversationHeadersOnly": [],
            "conversationMap": {},
            "modelConfig": { "modelName": "cursor-model" }
        });
        insert_cursor_kv(
            &connection,
            "composerData:aborted-empty-composer-1",
            aborted_composer,
        );
        drop(connection);

        let sessions = list_sessions(&profile(path), Some(10), None).expect("list cursor sessions");

        assert_eq!(sessions.len(), 1);
        assert!(!sessions[0].listable);
        assert_eq!(
            sessions[0]
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("reason"))
                .and_then(Value::as_str),
            Some("cursor_aborted_empty_composer")
        );
    }

    #[test]
    fn marks_cursor_draft_composers_non_listable() {
        let path = temp_state_db("draft-composer");
        let connection = create_cursor_kv_db(&path);
        let draft_composer = serde_json::json!({
            "composerId": "draft-composer-1",
            "text": "draft prompt",
            "createdAt": 1_766_000_000_000_u64,
            "lastUpdatedAt": 1_766_000_060_000_u64,
            "fullConversationHeadersOnly": [],
            "conversationMap": {},
            "status": "none",
            "isDraft": true,
            "modelConfig": { "modelName": "cursor-model" }
        });
        insert_cursor_kv(&connection, "composerData:draft-composer-1", draft_composer);
        drop(connection);

        let sessions = list_sessions(&profile(path), Some(10), None).expect("list cursor sessions");

        assert_eq!(sessions.len(), 1);
        assert!(!sessions[0].listable);
        assert_eq!(
            sessions[0]
                .metadata_json
                .as_ref()
                .and_then(|metadata| metadata.get("kind"))
                .and_then(Value::as_str),
            Some("draft")
        );
    }

    #[test]
    fn extracts_sessions_from_cursor_composer_headers() {
        let path = temp_state_db("headers");
        let connection = create_cursor_kv_db(&path);
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

        let sessions = list_sessions(&profile(path), Some(10), None).expect("list cursor sessions");

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
    fn merges_composer_data_subagent_flag_onto_header_session() {
        let path = temp_state_db("headers-subagent-merge");
        let connection = create_cursor_kv_db(&path);
        let headers = serde_json::json!({
            "allComposers": {
                "composer-sub-1": {
                    "name": "Subagent run",
                    "createdAt": 1_766_000_000_000_u64,
                    "lastUpdatedAt": 1_766_000_060_000_u64,
                    "trackedGitRepos": [
                        {
                            "repoPath": "/workspace/repo",
                            "branches": [{ "branchName": "main" }]
                        }
                    ],
                    "modelConfig": { "modelName": "cursor-model" }
                }
            }
        });
        connection
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                [CURSOR_COMPOSER_HEADERS_KEY, &headers.to_string()],
            )
            .expect("insert headers");
        let composer_data = serde_json::json!({
            "composerId": "composer-sub-1",
            "fullConversationHeadersOnly": [{ "bubbleId": "user-1" }],
            "subagentInfo": {
                "parentComposerId": "composer-parent-1",
                "toolCallId": "tool-7",
                "subagentTypeName": "explorer"
            }
        });
        insert_cursor_kv(&connection, "composerData:composer-sub-1", composer_data);
        drop(connection);

        let sessions = list_sessions(&profile(path), Some(10), None).expect("list cursor sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].title.as_deref(), Some("Subagent run"));
        assert_eq!(
            sessions[0].repo_path.as_deref(),
            Some(Path::new("/workspace/repo"))
        );
        assert_eq!(
            sessions[0].parser_version,
            CURSOR_IDE_HEADERS_PARSER_VERSION
        );
        assert!(!sessions[0].listable);
        let metadata = sessions[0]
            .metadata_json
            .as_ref()
            .expect("subagent metadata");
        assert_eq!(
            metadata.get("kind").and_then(Value::as_str),
            Some("subagent")
        );
        assert_eq!(
            metadata.get("parentSessionId").and_then(Value::as_str),
            Some("composer-parent-1")
        );
    }

    #[test]
    fn resolves_cursor_content_blob_for_user_message() {
        let path = temp_state_db("content-user");
        let connection = create_cursor_kv_db(&path);
        let composer = serde_json::json!({
            "composerId": "composer-blob",
            "fullConversationHeadersOnly": [{ "bubbleId": "user-blob" }]
        });
        let user_bubble = serde_json::json!({
            "bubbleId": "user-blob",
            "type": 1,
            "createdAt": 1_766_000_003_000_u64,
            "content": "composer.content.0123456789abcdef0123456789abcdef"
        });
        insert_cursor_kv(&connection, "composerData:composer-blob", composer);
        insert_cursor_kv(&connection, "bubbleId:composer-blob:user-blob", user_bubble);
        insert_cursor_kv(
            &connection,
            "composer.content.0123456789abcdef0123456789abcdef",
            Value::String("dereferenced user prompt".to_string()),
        );
        drop(connection);

        let chunks = format_chunks("composer-blob", Some(&path)).expect("format cursor chunks");

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].function, FUNCTION_USER_MESSAGE);
        assert_eq!(
            chunks[0].result["message"]["content"],
            "dereferenced user prompt"
        );
    }

    #[test]
    fn resolves_cursor_content_blobs_for_tool_args_and_result() {
        let path = temp_state_db("content-tool");
        let connection = create_cursor_kv_db(&path);
        let composer = serde_json::json!({
            "composerId": "composer-tool-blob",
            "fullConversationHeadersOnly": [{ "bubbleId": "tool-blob" }]
        });
        let tool_bubble = serde_json::json!({
            "bubbleId": "tool-blob",
            "type": 2,
            "createdAt": 1_766_000_004_000_u64,
            "toolFormerData": {
                "name": "run_terminal_command",
                "params": {
                    "contentId": "89abcdef0123456789abcdef01234567"
                },
                "result": {
                    "contentKey": "composer.content.fedcba9876543210fedcba9876543210"
                }
            }
        });
        insert_cursor_kv(&connection, "composerData:composer-tool-blob", composer);
        insert_cursor_kv(
            &connection,
            "bubbleId:composer-tool-blob:tool-blob",
            tool_bubble,
        );
        insert_cursor_kv(
            &connection,
            "composer.content.89abcdef0123456789abcdef01234567",
            Value::String("{\"command\":\"pwd\"}".to_string()),
        );
        insert_cursor_kv(
            &connection,
            "composer.content.fedcba9876543210fedcba9876543210",
            serde_json::json!({ "text": "workspace path" }),
        );
        drop(connection);

        let chunks =
            format_chunks("composer-tool-blob", Some(&path)).expect("format cursor chunks");

        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].action_type, ACTION_TYPE_TOOL_CALL);
        assert_eq!(chunks[0].function, FUNCTION_RUN_COMMAND_LINE);
        assert_eq!(chunks[0].args["command"], "pwd");
        assert_eq!(chunks[0].result["output"], "workspace path");
    }

    #[test]
    fn formats_cursor_composer_bubbles_as_source_chunks() {
        let path = temp_state_db("chunks");
        let connection = create_cursor_kv_db(&path);
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
            insert_cursor_kv(&connection, key, value);
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
        assert_eq!(chunks[0].source_id.as_deref(), Some(CURSOR_IDE_SOURCE_ID));
        assert_eq!(
            chunks[0].source_path.as_deref(),
            Some(path.display().to_string().as_str())
        );
        assert_eq!(
            chunks[0].source_record_key.as_deref(),
            Some("bubbleId:composer-1:user-1")
        );
        assert_eq!(chunks[0].source_part_id.as_deref(), Some("user-1"));
        assert_eq!(
            chunks[2].source_record_key.as_deref(),
            Some("bubbleId:composer-1:tool-1")
        );
        assert_eq!(chunks[2].source_message_id.as_deref(), Some("tool-1"));
        assert_eq!(chunks[2].source_part_id.as_deref(), Some("tool-1"));
    }
}
