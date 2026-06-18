use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use brick_core::{
    format_source_session_chunks, list_source_plans, list_source_sessions,
    SourcePlanSessionEdgeRole, SourceProfile,
};
use rusqlite::Connection;
use serde::Deserialize;
use serde_json::Value;

const SUPPORTED_SOURCES: &[&str] = &[
    "cursor_ide",
    "windsurf",
    "opencode",
    "claude_code",
    "codex_app",
];

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct FixtureManifest {
    source: String,
    description: String,
    format: FixtureFormat,
    #[serde(default)]
    db_spec_path: Option<PathBuf>,
    profile: FixtureProfile,
    expected: ExpectedFixture,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum FixtureFormat {
    Jsonl,
    CursorKvSqlite,
    OpencodeSqlite,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct FixtureProfile {
    session_log_path: Option<PathBuf>,
    session_db_path: Option<PathBuf>,
    cursor_state_db_path: Option<PathBuf>,
    evidence_root: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct ExpectedFixture {
    sessions: Vec<ExpectedSession>,
    #[serde(default)]
    chunks: serde_json::Map<String, Value>,
    #[serde(default)]
    plans: Vec<ExpectedPlan>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedSession {
    external_session_id: String,
    title: Option<String>,
    parser_version: String,
    model: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
    repo_path: Option<PathBuf>,
    branch: Option<String>,
    #[serde(default)]
    files_changed: Option<u64>,
    #[serde(default)]
    lines_added: Option<u64>,
    #[serde(default)]
    lines_removed: Option<u64>,
    #[serde(default)]
    touched_files: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedPlan {
    external_plan_id: String,
    title: Option<String>,
    source_path: Option<PathBuf>,
    source_uri: Option<String>,
    parser_version: String,
    #[serde(default)]
    edges: Vec<ExpectedPlanEdge>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ExpectedPlanEdge {
    external_session_id: String,
    role: String,
}

#[derive(Debug, Deserialize)]
struct CursorKvSpec {
    rows: Vec<CursorKvRow>,
}

#[derive(Debug, Deserialize)]
struct CursorKvRow {
    key: String,
    value: Value,
}

struct FixtureRuntime {
    temp_db_path: Option<PathBuf>,
}

#[test]
fn external_source_fixture_manifests_follow_convention() {
    let fixture_root = external_source_fixture_root();
    let source_dirs = std::fs::read_dir(&fixture_root).expect("read external source fixtures root");
    let mut discovered_sources = BTreeSet::new();
    let mut manifest_count = 0_usize;

    for entry in source_dirs {
        let entry = entry.expect("read source fixture directory entry");
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let source_id = entry.file_name().to_string_lossy().to_string();
        if source_id == "README.md" {
            continue;
        }
        assert!(
            SUPPORTED_SOURCES.contains(&source_id.as_str()),
            "unexpected external source fixture directory: {source_id}"
        );
        discovered_sources.insert(source_id.clone());

        for scenario in std::fs::read_dir(&path).expect("read source scenario directory") {
            let scenario = scenario.expect("read scenario entry");
            let scenario_path = scenario.path();
            if !scenario_path.is_dir() {
                continue;
            }
            let manifest_path = scenario_path.join("manifest.json");
            assert!(
                manifest_path.exists(),
                "fixture scenario missing manifest: {}",
                scenario_path.display()
            );
            let manifest = read_manifest(&manifest_path);
            assert_eq!(manifest.source, source_id);
            assert!(
                !manifest.description.trim().is_empty(),
                "fixture manifest description must not be empty: {}",
                manifest_path.display()
            );
            match manifest.format {
                FixtureFormat::Jsonl => assert!(
                    manifest.profile.session_log_path.is_some(),
                    "JSONL fixture must set profile.sessionLogPath: {}",
                    manifest_path.display()
                ),
                FixtureFormat::CursorKvSqlite | FixtureFormat::OpencodeSqlite => {
                    let spec_path = manifest.db_spec_path.as_ref().unwrap_or_else(|| {
                        panic!(
                            "SQLite fixture must set dbSpecPath for generated DB: {}",
                            manifest_path.display()
                        )
                    });
                    assert!(
                        scenario_path.join(spec_path).exists(),
                        "SQLite fixture dbSpecPath does not exist: {}",
                        scenario_path.join(spec_path).display()
                    );
                }
            }
            manifest_count += 1;
        }
    }

    for source in SUPPORTED_SOURCES {
        assert!(
            discovered_sources.contains(*source),
            "missing fixture convention directory for source: {source}"
        );
    }
    assert!(manifest_count > 0, "expected at least one fixture scenario");
}

#[test]
fn external_source_provider_fixtures_match_expected_metadata_and_chunks() {
    for scenario_path in fixture_scenario_paths() {
        let manifest_path = scenario_path.join("manifest.json");
        let manifest = read_manifest(&manifest_path);
        let runtime = materialize_fixture(&manifest, &scenario_path);
        let profile = source_profile(&manifest, &scenario_path, &runtime);
        let sessions = list_source_sessions(&profile, Some(50)).unwrap_or_else(|error| {
            panic!(
                "list sessions for fixture {} failed: {error:#}",
                manifest_path.display()
            )
        });

        assert_eq!(
            sessions.len(),
            manifest.expected.sessions.len(),
            "session count mismatch for {}",
            manifest_path.display()
        );

        for expected in &manifest.expected.sessions {
            let session = sessions
                .iter()
                .find(|session| session.external_session_id == expected.external_session_id)
                .unwrap_or_else(|| {
                    panic!(
                        "expected session {} missing for {}",
                        expected.external_session_id,
                        manifest_path.display()
                    )
                });
            assert_eq!(session.title, expected.title);
            assert_eq!(session.parser_version, expected.parser_version);
            assert_eq!(session.model, expected.model);
            assert_eq!(session.input_tokens, expected.input_tokens);
            assert_eq!(session.output_tokens, expected.output_tokens);
            assert_eq!(session.repo_path, expected.repo_path);
            assert_eq!(session.branch, expected.branch);
            if let Some(files_changed) = expected.files_changed {
                assert_eq!(session.files_changed, Some(files_changed));
            }
            if let Some(lines_added) = expected.lines_added {
                assert_eq!(session.lines_added, Some(lines_added));
            }
            if let Some(lines_removed) = expected.lines_removed {
                assert_eq!(session.lines_removed, Some(lines_removed));
            }
            if let Some(touched_files) = &expected.touched_files {
                assert_eq!(&session.touched_files, touched_files);
            }

            if let Some(expected_chunks) =
                manifest.expected.chunks.get(&expected.external_session_id)
            {
                assert_expected_chunks(
                    &manifest,
                    &manifest_path,
                    &expected.external_session_id,
                    Some(&session.path),
                    expected_chunks,
                );
            }
        }

        assert_expected_plans(&manifest, &manifest_path, &profile);
    }
}

fn assert_expected_chunks(
    manifest: &FixtureManifest,
    manifest_path: &Path,
    external_session_id: &str,
    source_path: Option<&Path>,
    expected_chunks: &Value,
) {
    let expected_chunks = expected_chunks.as_array().unwrap_or_else(|| {
        panic!(
            "expected chunks must be an array for session {} in {}",
            external_session_id,
            manifest_path.display()
        )
    });
    let chunks = format_source_session_chunks(&manifest.source, external_session_id, source_path)
        .unwrap_or_else(|error| {
            panic!(
                "format chunks for fixture {} session {} failed: {error:#}",
                manifest_path.display(),
                external_session_id
            )
        });
    assert_eq!(
        chunks.len(),
        expected_chunks.len(),
        "chunk count mismatch for {} session {}",
        manifest_path.display(),
        external_session_id
    );
    for (chunk, expected_chunk) in chunks.iter().zip(expected_chunks) {
        assert_eq!(
            chunk.action_type,
            expected_string(expected_chunk, "actionType")
        );
        assert_eq!(chunk.function, expected_string(expected_chunk, "function"));
        if let Some(text) = optional_expected_string(expected_chunk, "text") {
            assert_eq!(chunk_text(chunk), Some(text));
        }
        if let Some(command) = optional_expected_string(expected_chunk, "command") {
            assert_eq!(
                chunk.args.get("command").and_then(Value::as_str),
                Some(command)
            );
        }
        if let Some(target_file) = optional_expected_string(expected_chunk, "targetFile") {
            assert_eq!(
                chunk.args.get("target_file").and_then(Value::as_str),
                Some(target_file)
            );
        }
        if let Some(output) = optional_expected_string(expected_chunk, "output") {
            assert_eq!(
                chunk.result.get("output").and_then(Value::as_str),
                Some(output)
            );
        }
    }
}

fn assert_expected_plans(
    manifest: &FixtureManifest,
    manifest_path: &Path,
    profile: &SourceProfile,
) {
    if manifest.expected.plans.is_empty() {
        return;
    }
    let plans = list_source_plans(profile).unwrap_or_else(|error| {
        panic!(
            "list plans for fixture {} failed: {error:#}",
            manifest_path.display()
        )
    });
    assert_eq!(
        plans.len(),
        manifest.expected.plans.len(),
        "plan count mismatch for {}",
        manifest_path.display()
    );
    for expected in &manifest.expected.plans {
        let plan = plans
            .iter()
            .find(|plan| plan.plan.external_plan_id == expected.external_plan_id)
            .unwrap_or_else(|| {
                panic!(
                    "expected plan {} missing for {}",
                    expected.external_plan_id,
                    manifest_path.display()
                )
            });
        assert_eq!(plan.plan.title, expected.title);
        assert_eq!(plan.plan.source_path, expected.source_path);
        assert_eq!(plan.plan.source_uri, expected.source_uri);
        assert_eq!(
            plan.plan.parser_version.as_deref(),
            Some(expected.parser_version.as_str())
        );
        assert_eq!(plan.edges.len(), expected.edges.len());
        for expected_edge in &expected.edges {
            let role = expected_edge
                .role
                .parse::<SourcePlanSessionEdgeRole>()
                .expect("expected plan edge role should be valid");
            assert!(
                plan.edges.iter().any(|edge| {
                    edge.external_session_id == expected_edge.external_session_id
                        && edge.role == role
                }),
                "expected plan edge {}:{} missing for {}",
                expected_edge.external_session_id,
                expected_edge.role,
                manifest_path.display()
            );
        }
    }
}

fn materialize_fixture(manifest: &FixtureManifest, scenario_path: &Path) -> FixtureRuntime {
    match manifest.format {
        FixtureFormat::Jsonl => FixtureRuntime { temp_db_path: None },
        FixtureFormat::CursorKvSqlite => FixtureRuntime {
            temp_db_path: Some(build_cursor_kv_db(manifest, scenario_path)),
        },
        FixtureFormat::OpencodeSqlite => FixtureRuntime {
            temp_db_path: Some(build_opencode_db(manifest, scenario_path)),
        },
    }
}

fn build_cursor_kv_db(manifest: &FixtureManifest, scenario_path: &Path) -> PathBuf {
    let spec_path = fixture_spec_path(manifest, scenario_path);
    let spec: CursorKvSpec =
        serde_json::from_str(&fs::read_to_string(&spec_path).unwrap_or_else(|error| {
            panic!(
                "read Cursor-family fixture spec {} failed: {error}",
                spec_path.display()
            )
        }))
        .unwrap_or_else(|error| {
            panic!(
                "parse Cursor-family fixture spec {} failed: {error}",
                spec_path.display()
            )
        });
    let db_path = temp_fixture_db_path(&manifest.source, "state.vscdb");
    let connection = Connection::open(&db_path).expect("open generated Cursor-family fixture DB");
    connection
        .execute_batch("CREATE TABLE cursorDiskKV (key TEXT PRIMARY KEY, value TEXT NOT NULL);")
        .expect("create cursorDiskKV table");
    for row in spec.rows {
        connection
            .execute(
                "INSERT INTO cursorDiskKV (key, value) VALUES (?1, ?2)",
                (row.key, row.value.to_string()),
            )
            .expect("insert Cursor-family fixture row");
    }
    drop(connection);
    db_path
}

fn build_opencode_db(manifest: &FixtureManifest, scenario_path: &Path) -> PathBuf {
    let spec_path = fixture_spec_path(manifest, scenario_path);
    let sql = fs::read_to_string(&spec_path).unwrap_or_else(|error| {
        panic!(
            "read OpenCode fixture SQL spec {} failed: {error}",
            spec_path.display()
        )
    });
    let db_path = temp_fixture_db_path(&manifest.source, "opencode.db");
    let connection = Connection::open(&db_path).expect("open generated OpenCode fixture DB");
    connection
        .execute_batch(&sql)
        .expect("execute OpenCode fixture SQL spec");
    drop(connection);
    db_path
}

fn fixture_spec_path(manifest: &FixtureManifest, scenario_path: &Path) -> PathBuf {
    let spec_path = manifest
        .db_spec_path
        .as_ref()
        .expect("generated SQLite fixture should have dbSpecPath");
    scenario_path.join(spec_path)
}

fn temp_fixture_db_path(source: &str, file_name: &str) -> PathBuf {
    let root = std::env::temp_dir().join(format!(
        "brick-external-source-fixture-{source}-{}",
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    fs::create_dir_all(&root).expect("create external source fixture temp dir");
    root.join(file_name)
}

fn external_source_fixture_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("external_sources")
}

fn fixture_scenario_paths() -> Vec<PathBuf> {
    let fixture_root = external_source_fixture_root();
    let mut scenarios = Vec::new();
    for source in SUPPORTED_SOURCES {
        let source_dir = fixture_root.join(source);
        if !source_dir.exists() {
            continue;
        }
        for entry in std::fs::read_dir(&source_dir).expect("read source fixture directory") {
            let entry = entry.expect("read fixture scenario entry");
            let path = entry.path();
            if path.is_dir() && path.join("manifest.json").exists() {
                scenarios.push(path);
            }
        }
    }
    scenarios.sort();
    scenarios
}

fn read_manifest(path: &Path) -> FixtureManifest {
    let contents = std::fs::read_to_string(path)
        .unwrap_or_else(|error| panic!("read fixture manifest {} failed: {error}", path.display()));
    serde_json::from_str(&contents)
        .unwrap_or_else(|error| panic!("parse fixture manifest {} failed: {error}", path.display()))
}

fn source_profile(
    manifest: &FixtureManifest,
    scenario_path: &Path,
    runtime: &FixtureRuntime,
) -> SourceProfile {
    SourceProfile {
        name: manifest.source.clone(),
        app_id: Some(manifest.source.clone()),
        actor_id: None,
        actor_type: None,
        store_root: None,
        session_db_path: runtime.temp_db_path.clone().or_else(|| {
            manifest
                .profile
                .session_db_path
                .as_ref()
                .map(|path| scenario_path.join(path))
        }),
        session_log_path: manifest
            .profile
            .session_log_path
            .as_ref()
            .map(|path| scenario_path.join(path)),
        evidence_root: manifest
            .profile
            .evidence_root
            .as_ref()
            .map(|path| scenario_path.join(path)),
        cursor_state_db_path: runtime.temp_db_path.clone().or_else(|| {
            manifest
                .profile
                .cursor_state_db_path
                .as_ref()
                .map(|path| scenario_path.join(path))
        }),
        default_full_evidence_upload: None,
        notes: Some("sanitized external source fixture".to_string()),
    }
}

fn expected_string<'a>(value: &'a Value, field: &str) -> &'a str {
    optional_expected_string(value, field)
        .unwrap_or_else(|| panic!("expected fixture chunk field {field} to be a string"))
}

fn optional_expected_string<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn chunk_text(chunk: &brick_core::ActivityChunk) -> Option<&str> {
    chunk
        .result
        .get("message")
        .and_then(|message| message.get("content"))
        .and_then(Value::as_str)
        .or_else(|| chunk.result.get("content").and_then(Value::as_str))
        .or_else(|| chunk.result.get("observation").and_then(Value::as_str))
}
