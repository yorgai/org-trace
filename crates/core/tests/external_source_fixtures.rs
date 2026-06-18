use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use brick_core::{format_source_session_chunks, list_source_sessions, SourceProfile};
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

#[derive(Debug, Deserialize)]
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
        assert_eq!(
            manifest.format,
            FixtureFormat::Jsonl,
            "SQLite fixture formats should be generated from text specs before enabling this generic runner: {}",
            manifest_path.display()
        );

        let profile = source_profile(&manifest, &scenario_path);
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

            if let Some(expected_chunks) =
                manifest.expected.chunks.get(&expected.external_session_id)
            {
                let expected_chunks = expected_chunks.as_array().unwrap_or_else(|| {
                    panic!(
                        "expected chunks must be an array for session {} in {}",
                        expected.external_session_id,
                        manifest_path.display()
                    )
                });
                let chunks = format_source_session_chunks(
                    &manifest.source,
                    &expected.external_session_id,
                    Some(&session.path),
                )
                .unwrap_or_else(|error| {
                    panic!(
                        "format chunks for fixture {} session {} failed: {error:#}",
                        manifest_path.display(),
                        expected.external_session_id
                    )
                });
                assert_eq!(
                    chunks.len(),
                    expected_chunks.len(),
                    "chunk count mismatch for {} session {}",
                    manifest_path.display(),
                    expected.external_session_id
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
                    if let Some(output) = optional_expected_string(expected_chunk, "output") {
                        assert_eq!(
                            chunk.result.get("output").and_then(Value::as_str),
                            Some(output)
                        );
                    }
                }
            }
        }
    }
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

fn source_profile(manifest: &FixtureManifest, scenario_path: &Path) -> SourceProfile {
    SourceProfile {
        name: manifest.source.clone(),
        app_id: Some(manifest.source.clone()),
        actor_id: None,
        actor_type: None,
        store_root: None,
        session_db_path: manifest
            .profile
            .session_db_path
            .as_ref()
            .map(|path| scenario_path.join(path)),
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
        cursor_state_db_path: manifest
            .profile
            .cursor_state_db_path
            .as_ref()
            .map(|path| scenario_path.join(path)),
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
