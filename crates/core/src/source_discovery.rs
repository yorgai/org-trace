//! Built-in discovery of local agent history and evidence stores.
//!
//! Discovery is intentionally read-only. It builds deterministic candidate sets
//! for known source storage locations and only reports paths that already exist
//! so `brick init` can help users decide what to include without copying
//! transcripts or recordings into Brick.

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
    Windsurf,
    OpenCode,
}

impl DiscoveredSourceKind {
    /// Stable source profile name used when writing discovered defaults.
    pub fn profile_name(self) -> &'static str {
        match self {
            DiscoveredSourceKind::Orgii => "orgii",
            DiscoveredSourceKind::Cursor => "cursor_ide",
            DiscoveredSourceKind::ClaudeCode => "claude_code",
            DiscoveredSourceKind::Codex => "codex_app",
            DiscoveredSourceKind::Windsurf => "windsurf",
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
            DiscoveredSourceKind::Windsurf => "Windsurf",
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
        DiscoveredSourceKind::Windsurf,
        windsurf_paths(&home),
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

fn windsurf_paths(home: &Path) -> Vec<DiscoveredSourcePath> {
    windsurf_state_db_candidates(home)
        .into_iter()
        .map(|candidate| path(DiscoveredPathKind::CursorStateDatabase, candidate))
        .collect()
}

fn opencode_paths(home: &Path) -> Vec<DiscoveredSourcePath> {
    opencode_db_candidates(home)
        .into_iter()
        .map(|candidate| path(DiscoveredPathKind::SessionDatabase, candidate))
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

fn windsurf_state_db_candidates(home: &Path) -> Vec<PathBuf> {
    app_state_db_candidates(home, "Windsurf", &[".windsurf"])
}

fn opencode_db_candidates(home: &Path) -> Vec<PathBuf> {
    opencode_roots(home)
        .into_iter()
        .map(|root| root.join("opencode.db"))
        .collect()
}

fn opencode_roots(home: &Path) -> Vec<PathBuf> {
    let fallback_roots = [home.join(".opencode"), home.join(".local/share/opencode")];
    let platform_roots = platform_data_roots(home, "opencode", &["ai.opencode.desktop"]);
    unique_paths(fallback_roots.into_iter().chain(platform_roots).collect())
}

fn app_state_db_candidates(home: &Path, app_name: &str, fallback_roots: &[&str]) -> Vec<PathBuf> {
    let roots = fallback_roots
        .iter()
        .map(|root| home.join(root))
        .chain(platform_data_roots(home, app_name, &[]))
        .collect();
    unique_paths(roots)
        .into_iter()
        .map(|root| root.join("User/globalStorage/state.vscdb"))
        .collect()
}

fn platform_data_roots(home: &Path, app_name: &str, alternate_app_names: &[&str]) -> Vec<PathBuf> {
    std::iter::once(app_name)
        .chain(alternate_app_names.iter().copied())
        .flat_map(|name| {
            [
                home.join("Library/Application Support").join(name),
                home.join("AppData/Roaming").join(name),
                home.join("AppData/Local").join(name),
                home.join(".config").join(name),
                home.join(".local/share").join(name),
            ]
        })
        .collect()
}

fn unique_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths.into_iter().fold(Vec::new(), |mut unique, path| {
        if !unique.contains(&path) {
            unique.push(path);
        }
        unique
    })
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
    fn windsurf_candidates_include_state_db_files() {
        let home = Path::new("/Users/me");
        let rendered = windsurf_state_db_candidates(home)
            .into_iter()
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>();

        assert!(rendered
            .iter()
            .any(|path| path.contains("Windsurf/User/globalStorage/state.vscdb")));
        assert!(rendered
            .iter()
            .any(|path| path.contains(".windsurf/User/globalStorage/state.vscdb")));
        assert!(rendered.iter().all(|path| path.ends_with("state.vscdb")));
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
        assert!(rendered
            .iter()
            .any(|path| path.contains(".opencode/opencode.db")));
        assert!(rendered.iter().all(|path| path.ends_with("opencode.db")));
    }

    #[test]
    fn opencode_discovery_paths_map_to_session_db_path() {
        let home = Path::new("/Users/me");
        let paths = opencode_paths(home);

        assert!(paths
            .iter()
            .all(|candidate| candidate.kind == DiscoveredPathKind::SessionDatabase));
    }
}
