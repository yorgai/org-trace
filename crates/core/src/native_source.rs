//! Generic read-only listing for native session files.
//!
//! App-specific metadata extraction lives under `sources/*`. This module is the
//! fallback file enumerator and shared session DTO/builder for providers.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::SourceProfile;

const DEFAULT_NATIVE_SESSION_LIMIT: usize = 50;
const MAX_NATIVE_SCAN_ENTRIES: usize = 10_000;
pub(crate) const GENERIC_NATIVE_FILE_PARSER_VERSION: &str = "native-file-v1";

/// Metadata for an external source session that can be imported into Brick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSourceSession {
    pub external_session_id: String,
    pub source_app_id: String,
    pub title: Option<String>,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_at: Option<SystemTime>,
    pub parser_version: String,
    pub session_created_at: Option<SystemTime>,
    pub session_updated_at: Option<SystemTime>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub repo_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub touched_files: Vec<String>,
    pub listable: bool,
    pub metadata_json: Option<Value>,
}

#[derive(Debug)]
pub(crate) struct NativeSessionMetadata {
    pub title: Option<String>,
    pub parser_version: Option<String>,
    pub session_created_at: Option<SystemTime>,
    pub session_updated_at: Option<SystemTime>,
    pub model: Option<String>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub repo_path: Option<PathBuf>,
    pub branch: Option<String>,
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub touched_files: Vec<String>,
    pub listable: bool,
    pub metadata_json: Option<Value>,
}

impl Default for NativeSessionMetadata {
    fn default() -> Self {
        Self {
            title: None,
            parser_version: None,
            session_created_at: None,
            session_updated_at: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            repo_path: None,
            branch: None,
            files_changed: None,
            lines_added: None,
            lines_removed: None,
            touched_files: Vec::new(),
            listable: true,
            metadata_json: None,
        }
    }
}

/// Returns recent generic native source sessions discovered from a source profile.
pub fn list_native_source_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    list_file_source_sessions(profile, limit, |_| Ok(NativeSessionMetadata::default()))
}

pub(crate) fn list_file_source_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
    extract: impl Fn(&Path) -> Result<NativeSessionMetadata>,
) -> Result<Vec<NativeSourceSession>> {
    list_file_source_sessions_with_filter(profile, limit, extract, is_supported_session_file)
}

pub(crate) fn list_file_source_sessions_with_filter(
    profile: &SourceProfile,
    limit: Option<usize>,
    extract: impl Fn(&Path) -> Result<NativeSessionMetadata>,
    include: impl Fn(&Path) -> bool,
) -> Result<Vec<NativeSourceSession>> {
    let scan_limit = limit.unwrap_or(DEFAULT_NATIVE_SESSION_LIMIT);
    let mut roots = Vec::new();
    if let Some(path) = &profile.session_log_path {
        roots.push(path.clone());
    }
    if let Some(path) = &profile.evidence_root {
        roots.push(path.clone());
    }

    let app_id = profile
        .app_id
        .clone()
        .unwrap_or_else(|| profile.name.clone());
    let mut sessions = Vec::new();
    for root in roots {
        collect_session_files(&root, &app_id, &extract, &include, &mut sessions)?;
        if sessions.len() >= MAX_NATIVE_SCAN_ENTRIES {
            break;
        }
    }

    sessions.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
    sessions.dedup_by(|left, right| left.path == right.path);
    sessions.truncate(scan_limit);
    Ok(sessions)
}

fn collect_session_files(
    root: &Path,
    app_id: &str,
    extract: &impl Fn(&Path) -> Result<NativeSessionMetadata>,
    include: &impl Fn(&Path) -> bool,
    sessions: &mut Vec<NativeSourceSession>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    if root.is_file() {
        if include(root) {
            sessions.push(session_from_path(root, app_id, extract)?);
        }
        return Ok(());
    }

    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let entries = fs::read_dir(&directory).with_context(|| {
            format!(
                "failed to read native source directory {}",
                directory.display()
            )
        })?;
        for entry in entries {
            let entry = entry.with_context(|| {
                format!(
                    "failed to read native source entry in {}",
                    directory.display()
                )
            })?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            if !include(&path) {
                continue;
            }
            sessions.push(session_from_path(&path, app_id, extract)?);
            if sessions.len() >= MAX_NATIVE_SCAN_ENTRIES {
                return Ok(());
            }
        }
    }
    Ok(())
}

fn is_supported_session_file(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("jsonl" | "json" | "txt" | "log" | "md" | "markdown")
    )
}

fn session_from_path(
    path: &Path,
    app_id: &str,
    extract: &impl Fn(&Path) -> Result<NativeSessionMetadata>,
) -> Result<NativeSourceSession> {
    let file_metadata = fs::metadata(path).with_context(|| {
        format!(
            "failed to read native source metadata for {}",
            path.display()
        )
    })?;
    let external_session_id = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or("session")
        .to_string();
    let extracted = extract(path)?;
    let title = extracted
        .title
        .unwrap_or_else(|| external_session_id.clone());
    Ok(NativeSourceSession {
        title: Some(title),
        external_session_id,
        source_app_id: app_id.to_string(),
        path: path.to_path_buf(),
        size_bytes: file_metadata.len(),
        modified_at: file_metadata.modified().ok(),
        parser_version: extracted
            .parser_version
            .unwrap_or_else(|| GENERIC_NATIVE_FILE_PARSER_VERSION.to_string()),
        session_created_at: extracted.session_created_at,
        session_updated_at: extracted.session_updated_at,
        model: extracted.model,
        input_tokens: extracted.input_tokens,
        output_tokens: extracted.output_tokens,
        repo_path: extracted.repo_path,
        branch: extracted.branch,
        files_changed: extracted.files_changed,
        lines_added: extracted.lines_added,
        lines_removed: extracted.lines_removed,
        touched_files: extracted.touched_files,
        listable: extracted.listable,
        metadata_json: extracted.metadata_json,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;
    use brick_protocol::ActorType;

    fn temp_source_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-native-source-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create native source root");
        path
    }

    #[test]
    fn lists_supported_session_files_from_profile_paths() {
        let root = temp_source_root("list-files");
        let nested = root.join("projects").join("repo");
        fs::create_dir_all(&nested).expect("create nested source dir");
        let transcript_path = nested.join("abc.jsonl");
        let mut file = fs::File::create(&transcript_path).expect("create transcript");
        writeln!(file, "{{\"message\":\"hello\"}}").expect("write transcript");
        fs::write(nested.join("ignore.bin"), "ignored").expect("write ignored file");

        let profile = SourceProfile {
            name: "generic".to_string(),
            app_id: Some("generic".to_string()),
            actor_id: None,
            actor_type: Some(ActorType::Agent),
            store_root: None,
            session_db_path: None,
            session_log_path: Some(root.join("projects")),
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        };

        let sessions = list_native_source_sessions(&profile, Some(10)).expect("list sessions");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].external_session_id, "abc");
        assert_eq!(sessions[0].path, transcript_path);
        assert_eq!(
            sessions[0].parser_version,
            GENERIC_NATIVE_FILE_PARSER_VERSION
        );
    }
}
