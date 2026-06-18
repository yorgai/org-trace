//! Built-in discovery of local agent history and evidence stores.
//!
//! Discovery is intentionally read-only. It only reports candidate paths that
//! already exist so `brick init` can help users decide what to include without
//! copying transcripts or recordings into Brick.

use std::env;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Known local source application that can feed Brick evidence pointers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveredSourceKind {
    Orgii,
    Cursor,
    ClaudeCode,
    Codex,
    OpenCode,
}

impl DiscoveredSourceKind {
    /// Stable source profile name used when writing discovered defaults.
    pub fn profile_name(self) -> &'static str {
        match self {
            DiscoveredSourceKind::Orgii => "orgii",
            DiscoveredSourceKind::Cursor => "cursor",
            DiscoveredSourceKind::ClaudeCode => "claude_code",
            DiscoveredSourceKind::Codex => "codex",
            DiscoveredSourceKind::OpenCode => "opencode",
        }
    }

    /// App ID recorded in Brick session source metadata.
    pub fn app_id(self) -> &'static str {
        self.profile_name()
    }

    /// Human-readable label for CLI prompts and scan output.
    pub fn label(self) -> &'static str {
        match self {
            DiscoveredSourceKind::Orgii => "ORGII",
            DiscoveredSourceKind::Cursor => "Cursor",
            DiscoveredSourceKind::ClaudeCode => "Claude Code",
            DiscoveredSourceKind::Codex => "Codex",
            DiscoveredSourceKind::OpenCode => "OpenCode",
        }
    }
}

/// Type of local evidence path discovered for a source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiscoveredPathKind {
    EvidenceRoot,
    SessionDatabase,
    CursorStateDatabase,
    SessionLogRoot,
    HistoryDatabase,
}

impl DiscoveredPathKind {
    /// Stable lower-case name for CLI output.
    pub fn label(self) -> &'static str {
        match self {
            DiscoveredPathKind::EvidenceRoot => "evidence_root",
            DiscoveredPathKind::SessionDatabase => "session_db_path",
            DiscoveredPathKind::CursorStateDatabase => "cursor_state_db_path",
            DiscoveredPathKind::SessionLogRoot => "session_log_path",
            DiscoveredPathKind::HistoryDatabase => "history_db_path",
        }
    }
}

/// One discovered local path that can be referenced by a source profile.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredSourcePath {
    pub kind: DiscoveredPathKind,
    pub path: PathBuf,
}

/// A discovered source with one or more existing local paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredSource {
    pub source: DiscoveredSourceKind,
    pub paths: Vec<DiscoveredSourcePath>,
}

/// Scans well-known local paths for source evidence stores.
pub fn discover_sources() -> Vec<DiscoveredSource> {
    let Some(home) = home_dir() else {
        return Vec::new();
    };
    let mut sources = Vec::new();
    push_source(
        &mut sources,
        DiscoveredSourceKind::Orgii,
        orgii_paths(&home),
    );
    push_source(
        &mut sources,
        DiscoveredSourceKind::Cursor,
        cursor_paths(&home),
    );
    push_source(
        &mut sources,
        DiscoveredSourceKind::ClaudeCode,
        claude_code_paths(&home),
    );
    push_source(
        &mut sources,
        DiscoveredSourceKind::Codex,
        codex_paths(&home),
    );
    push_source(
        &mut sources,
        DiscoveredSourceKind::OpenCode,
        opencode_paths(&home),
    );
    sources
}

fn push_source(
    sources: &mut Vec<DiscoveredSource>,
    source: DiscoveredSourceKind,
    candidates: Vec<DiscoveredSourcePath>,
) {
    let existing = candidates
        .into_iter()
        .filter(|candidate| candidate.path.exists())
        .collect::<Vec<_>>();
    if !existing.is_empty() {
        sources.push(DiscoveredSource {
            source,
            paths: existing,
        });
    }
}

fn orgii_paths(home: &Path) -> Vec<DiscoveredSourcePath> {
    let root = home.join(".orgii");
    vec![
        path(DiscoveredPathKind::EvidenceRoot, root.clone()),
        path(
            DiscoveredPathKind::SessionDatabase,
            root.join("sessions.db"),
        ),
        path(DiscoveredPathKind::SessionLogRoot, root.join("logs")),
        path(
            DiscoveredPathKind::EvidenceRoot,
            root.join("cursor-cli-profiles"),
        ),
        path(
            DiscoveredPathKind::EvidenceRoot,
            root.join("claude-code-cli-profiles"),
        ),
        path(
            DiscoveredPathKind::EvidenceRoot,
            root.join("codex-cli-profiles"),
        ),
        path(
            DiscoveredPathKind::EvidenceRoot,
            root.join("opencode-cli-profiles"),
        ),
    ]
}

fn cursor_paths(home: &Path) -> Vec<DiscoveredSourcePath> {
    vec![path(
        DiscoveredPathKind::CursorStateDatabase,
        cursor_state_db_path(home),
    )]
}

fn claude_code_paths(home: &Path) -> Vec<DiscoveredSourcePath> {
    vec![
        path(DiscoveredPathKind::EvidenceRoot, home.join(".claude")),
        path(
            DiscoveredPathKind::SessionLogRoot,
            home.join(".claude").join("projects"),
        ),
    ]
}

fn codex_paths(home: &Path) -> Vec<DiscoveredSourcePath> {
    codex_session_dir_candidates(home)
        .into_iter()
        .map(|candidate| path(DiscoveredPathKind::SessionLogRoot, candidate))
        .collect()
}

fn opencode_paths(home: &Path) -> Vec<DiscoveredSourcePath> {
    opencode_db_candidates(home)
        .into_iter()
        .map(|candidate| path(DiscoveredPathKind::HistoryDatabase, candidate))
        .collect()
}

fn path(kind: DiscoveredPathKind, path: PathBuf) -> DiscoveredSourcePath {
    DiscoveredSourcePath { kind, path }
}

fn cursor_state_db_path(home: &Path) -> PathBuf {
    if cfg!(target_os = "macos") {
        home.join("Library/Application Support/Cursor/User/globalStorage/state.vscdb")
    } else if cfg!(target_os = "windows") {
        home.join("AppData/Roaming/Cursor/User/globalStorage/state.vscdb")
    } else {
        home.join(".config/Cursor/User/globalStorage/state.vscdb")
    }
}

fn codex_session_dir_candidates(home: &Path) -> Vec<PathBuf> {
    let mut roots = vec![home.join(".codex")];
    if cfg!(target_os = "macos") {
        roots.push(home.join("Library/Application Support/codex"));
    } else if cfg!(target_os = "windows") {
        roots.push(home.join("AppData/Roaming/codex"));
        roots.push(home.join("AppData/Local/codex"));
    } else {
        roots.push(home.join(".config/codex"));
        roots.push(home.join(".local/share/codex"));
    }
    roots
        .into_iter()
        .map(|root| root.join("sessions"))
        .collect()
}

fn opencode_db_candidates(home: &Path) -> Vec<PathBuf> {
    let mut paths = vec![home.join(".local/share/opencode/opencode.db")];
    if cfg!(target_os = "macos") {
        paths.push(home.join("Library/Application Support/opencode/opencode.db"));
        paths.push(home.join("Library/Application Support/ai.opencode.desktop/opencode.db"));
    } else if cfg!(target_os = "windows") {
        paths.push(home.join("AppData/Roaming/opencode/opencode.db"));
        paths.push(home.join("AppData/Local/opencode/opencode.db"));
    } else {
        paths.push(home.join(".config/opencode/opencode.db"));
    }
    paths
}

fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("USERPROFILE").map(PathBuf::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codex_candidates_include_common_roots() {
        let home = Path::new("/Users/me");
        let rendered = codex_session_dir_candidates(home)
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();

        assert!(rendered.iter().any(|path| path.contains(".codex/sessions")));
        assert!(rendered.iter().all(|path| path.ends_with("sessions")));
    }

    #[test]
    fn opencode_candidates_include_db_files() {
        let home = Path::new("/Users/me");
        let rendered = opencode_db_candidates(home)
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();

        assert!(rendered
            .iter()
            .any(|path| path.contains("opencode/opencode.db")));
        assert!(rendered.iter().all(|path| path.ends_with("opencode.db")));
    }
}
