//! Global Brick home resolution.
//!
//! This module owns process-wide Brick state paths. The unified local event DB
//! lives directly under the Brick home; repo-specific derived caches and views
//! live under per-repository provenance directories.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use sha2::{Digest, Sha256};

/// Environment variable that overrides the default global Brick home.
pub const BRICK_HOME_ENV: &str = "BRICK_HOME";

/// Directory name used below the operating-system user home by default.
pub const DEFAULT_BRICK_HOME_DIR: &str = ".brick";

/// Filename of the unified local Brick event/chunk database.
pub const LOCAL_EVENT_DB_FILE: &str = "brick.sqlite";

/// Filename of the legacy source metadata database, used as a migration/input store only.
pub const METADATA_DB_FILE: &str = "metadata.sqlite";

/// Directory under the Brick home holding per-repository provenance stores.
pub const REPOS_DIR: &str = "repos";

const HOME_ENV: &str = "HOME";
const USERPROFILE_ENV: &str = "USERPROFILE";

/// Resolves the global Brick home using `BRICK_HOME` or `~/.brick`.
pub fn resolve_brick_home() -> Result<PathBuf> {
    resolve_brick_home_with_env(std::env::var_os(BRICK_HOME_ENV).map(PathBuf::from))
}

/// Returns the default global Brick home path without consulting `BRICK_HOME`.
pub fn default_brick_home() -> Result<PathBuf> {
    user_home_dir()
        .map(|home| home.join(DEFAULT_BRICK_HOME_DIR))
        .ok_or_else(|| anyhow!("failed to resolve default Brick home; set {BRICK_HOME_ENV}"))
}

/// Returns the unified local event/chunk database path for the resolved global Brick home.
pub fn local_event_db_path() -> Result<PathBuf> {
    Ok(resolve_brick_home()?.join(LOCAL_EVENT_DB_FILE))
}

/// Returns the unified local event/chunk database path for an explicit Brick home.
pub fn local_event_db_path_in_home(brick_home: impl AsRef<Path>) -> PathBuf {
    brick_home.as_ref().join(LOCAL_EVENT_DB_FILE)
}

/// Returns the legacy metadata database path for the resolved global Brick home.
pub fn metadata_db_path() -> Result<PathBuf> {
    Ok(resolve_brick_home()?.join(METADATA_DB_FILE))
}

/// Returns the legacy metadata database path for an explicit Brick home.
pub fn metadata_db_path_in_home(brick_home: impl AsRef<Path>) -> PathBuf {
    brick_home.as_ref().join(METADATA_DB_FILE)
}

/// Derives a stable identifier for a repository from its canonical root path.
///
/// Used as the directory name under `~/.brick/repos/<id>/`. Path-based so a fresh
/// checkout with no commits still gets a stable home; the canonical path keeps a
/// symlinked root (macOS `/var`→`/private/var`) mapping to one id. Moving or
/// renaming the repo directory yields a new id (acceptable for the dev phase).
pub fn repo_id_for_root(repo_root: impl AsRef<Path>) -> String {
    let root = repo_root.as_ref();
    let canonical = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    let digest = hasher.finalize();
    // 16 hex chars (64 bits) is plenty to avoid collisions across one user's repos.
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

/// Returns the global provenance store root for a repository:
/// `<BRICK_HOME>/repos/<repo_id>/provenance`. This replaces the legacy
/// repo-local `<repo>/.brick/provenance` so a user has exactly one `~/.brick`
/// and nothing is written under their working tree.
pub fn repo_provenance_root(repo_root: impl AsRef<Path>) -> Result<PathBuf> {
    let id = repo_id_for_root(&repo_root);
    Ok(resolve_brick_home()?
        .join(REPOS_DIR)
        .join(id)
        .join(crate::PROVENANCE_DIR))
}

fn resolve_brick_home_with_env(value: Option<PathBuf>) -> Result<PathBuf> {
    match value {
        Some(path) if path.as_os_str().is_empty() => default_brick_home(),
        Some(path) => Ok(path),
        None => default_brick_home(),
    }
}

fn user_home_dir() -> Option<PathBuf> {
    std::env::var_os(HOME_ENV)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os(USERPROFILE_ENV).map(PathBuf::from))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn brick_home_uses_override_when_present() {
        let override_home = PathBuf::from("/tmp/brick-home-override");
        let resolved = resolve_brick_home_with_env(Some(override_home.clone()))
            .expect("resolve Brick home override");
        assert_eq!(resolved, override_home);
    }

    #[test]
    fn metadata_db_path_uses_explicit_home() {
        let home = PathBuf::from("/tmp/brick-home-explicit");
        assert_eq!(metadata_db_path_in_home(&home), home.join(METADATA_DB_FILE));
    }
}
