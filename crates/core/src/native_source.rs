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

pub(crate) const GENERIC_NATIVE_FILE_PARSER_VERSION: &str = "native-file-v1";

/// Whether a source session appears to be running right now.
///
/// This is a *transient*, scan-time value — never persisted to SQLite, because a
/// stored liveness would be stale the moment it lands. It is recomputed on every
/// `list_source_sessions` call from the source's own turn signals plus file
/// recency (see [`crate::sources::liveness`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Liveness {
    /// A turn is in progress, or the transcript changed within the active window.
    Active,
    /// No recent activity; the session is idle or finished.
    Idle,
    /// Could not determine (unsupported source, unreadable file).
    #[default]
    Unknown,
}

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
    /// Working directory the session ran in, used as a work-scope fallback when
    /// `repo_path` is absent (e.g. Codex sessions in non-git folders).
    pub cwd: Option<PathBuf>,
    /// Transient liveness, recomputed per scan (never persisted).
    pub liveness: Liveness,
    /// Most recent activity instant (turn signal or file mtime), for ranking.
    pub last_activity: Option<SystemTime>,
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
    pub cwd: Option<PathBuf>,
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
            cwd: None,
        }
    }
}

/// Returns recent generic native source sessions discovered from a source profile.
pub fn list_native_source_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    list_file_source_sessions(profile, limit, None, |_| {
        Ok(NativeSessionMetadata::default())
    })
}

pub(crate) fn list_file_source_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<SystemTime>,
    extract: impl Fn(&Path) -> Result<NativeSessionMetadata>,
) -> Result<Vec<NativeSourceSession>> {
    list_file_source_sessions_with_filter(profile, limit, since, extract, is_supported_session_file)
}

pub(crate) fn list_file_source_sessions_with_filter(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<SystemTime>,
    extract: impl Fn(&Path) -> Result<NativeSessionMetadata>,
    include: impl Fn(&Path) -> bool,
) -> Result<Vec<NativeSourceSession>> {
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
        collect_session_files(
            &root,
            &app_id,
            since,
            &extract,
            &include,
            limit,
            &mut sessions,
        )?;
        if limit.is_some_and(|limit| sessions.len() >= limit) {
            break;
        }
    }

    sessions.sort_by(|left, right| right.modified_at.cmp(&left.modified_at));
    sessions.dedup_by(|left, right| left.path == right.path);
    if let Some(limit) = limit {
        sessions.truncate(limit);
    }
    Ok(sessions)
}

fn collect_session_files(
    root: &Path,
    app_id: &str,
    since: Option<SystemTime>,
    extract: &impl Fn(&Path) -> Result<NativeSessionMetadata>,
    include: &impl Fn(&Path) -> bool,
    limit: Option<usize>,
    sessions: &mut Vec<NativeSourceSession>,
) -> Result<()> {
    if !root.exists() {
        return Ok(());
    }
    if root.is_file() {
        if include(root) && !skip_by_mtime(root, since) {
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
            // Incremental skip: a file whose mtime is at/under the watermark cannot
            // have changed since the last index, so skip the (expensive) parse
            // entirely. Conservative — mtime newer than the in-file timestamp only
            // causes an occasional re-parse the fingerprint layer then dedupes.
            if skip_by_mtime(&path, since) {
                continue;
            }
            sessions.push(session_from_path(&path, app_id, extract)?);
            if limit.is_some_and(|limit| sessions.len() >= limit) {
                return Ok(());
            }
        }
    }
    Ok(())
}

/// True when `since` is set and the file's mtime is at or before it (unchanged
/// since the last index). Unreadable mtime → never skip (fail open).
fn skip_by_mtime(path: &Path, since: Option<SystemTime>) -> bool {
    let Some(since) = since else {
        return false;
    };
    fs::metadata(path)
        .and_then(|metadata| metadata.modified())
        .map(|mtime| mtime <= since)
        .unwrap_or(false)
}

/// Parses an RFC3339 watermark string into a `SystemTime` for mtime comparison.
/// Returns `None` for `None` or an unparseable string (→ full scan, fail open).
pub(crate) fn since_to_system_time(since: Option<&str>) -> Option<SystemTime> {
    let since = since?;
    chrono::DateTime::parse_from_rfc3339(since)
        .ok()
        .map(|dt| SystemTime::from(dt.with_timezone(&chrono::Utc)))
}

/// Drops sessions at/under the `since` watermark, for SQLite-blob sources (cursor
/// family) whose updated time lives inside a JSON blob and so cannot be filtered
/// before parse. The blob read is unavoidable; this still shrinks the downstream
/// upsert set and lets the watermark advance. Sessions with no known
/// `session_updated_at` are kept (fail open).
pub(crate) fn filter_sessions_since(
    sessions: Vec<NativeSourceSession>,
    since: Option<&str>,
) -> Vec<NativeSourceSession> {
    let Some(since) = since_to_system_time(since) else {
        return sessions;
    };
    sessions
        .into_iter()
        .filter(|session| match session.session_updated_at {
            Some(updated) => updated > since,
            None => true,
        })
        .collect()
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
        cwd: extracted.cwd,
        liveness: Liveness::Unknown,
        last_activity: None,
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
