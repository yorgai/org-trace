//! Read-only listing of native agent session files referenced by source profiles.
//!
//! Native import starts from pointers to external stores. This module only
//! enumerates files and metadata; copying transcript bytes remains an explicit
//! evidence action.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result};

use crate::SourceProfile;

const DEFAULT_NATIVE_SESSION_LIMIT: usize = 50;
const MAX_NATIVE_SCAN_ENTRIES: usize = 10_000;

/// Metadata for an external source session that can be imported into Brick.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeSourceSession {
    pub external_session_id: String,
    pub source_app_id: String,
    pub title: Option<String>,
    pub path: PathBuf,
    pub size_bytes: u64,
    pub modified_at: Option<SystemTime>,
}

/// Returns recent native source sessions discovered from a source profile.
pub fn list_native_source_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
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
        collect_session_files(&root, &app_id, &mut sessions)?;
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
    sessions: &mut Vec<NativeSourceSession>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    if root.is_file() {
        if is_supported_session_file(root) {
            sessions.push(session_from_path(root, app_id)?);
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
            if !is_supported_session_file(&path) {
                continue;
            }
            sessions.push(session_from_path(&path, app_id)?);
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

fn session_from_path(path: &Path, app_id: &str) -> Result<NativeSourceSession> {
    let metadata = fs::metadata(path).with_context(|| {
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
    Ok(NativeSourceSession {
        title: Some(external_session_id.clone()),
        external_session_id,
        source_app_id: app_id.to_string(),
        path: path.to_path_buf(),
        size_bytes: metadata.len(),
        modified_at: metadata.modified().ok(),
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::*;

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
            name: "claude_code".to_string(),
            app_id: Some("claude_code".to_string()),
            actor_id: None,
            actor_type: None,
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
    }
}
