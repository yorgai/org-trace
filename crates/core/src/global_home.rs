//! Global Brick home resolution.
//!
//! This module owns process-wide Brick state paths. It is intentionally separate
//! from repo-local provenance storage so existing JSONL queue behavior remains
//! unchanged while new shared metadata can live under one user home.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

/// Environment variable that overrides the default global Brick home.
pub const BRICK_HOME_ENV: &str = "BRICK_HOME";

/// Directory name used below the operating-system user home by default.
pub const DEFAULT_BRICK_HOME_DIR: &str = ".brick";

/// Filename of the unified Brick metadata database.
pub const METADATA_DB_FILE: &str = "metadata.sqlite";

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

/// Returns the metadata database path for the resolved global Brick home.
pub fn metadata_db_path() -> Result<PathBuf> {
    Ok(resolve_brick_home()?.join(METADATA_DB_FILE))
}

/// Returns the metadata database path for an explicit Brick home.
pub fn metadata_db_path_in_home(brick_home: impl AsRef<Path>) -> PathBuf {
    brick_home.as_ref().join(METADATA_DB_FILE)
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
