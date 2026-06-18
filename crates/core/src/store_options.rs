//! Storage root resolution for local Brick stores.
//!
//! Resolution keeps Git repository discovery separate from event storage. The
//! repository root is still used for Git context and repo-local config, while the
//! effective storage root may come from CLI, environment, or a selected source.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::{SourceProfile, PROVENANCE_DIR};

/// Environment variable used when no explicit store root flag is provided.
pub const STORE_ROOT_ENV: &str = "BRICK_STORE_ROOT";

/// Inputs used to resolve the effective Brick storage root.
#[derive(Debug, Clone, Default)]
pub struct StorageOptions {
    pub explicit_store_root: Option<PathBuf>,
    pub source_profile: Option<SourceProfile>,
}

impl StorageOptions {
    /// Creates empty storage options that preserve repo-local defaults.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the CLI-provided storage root with highest precedence.
    pub fn with_explicit_store_root(mut self, store_root: Option<PathBuf>) -> Self {
        self.explicit_store_root = store_root;
        self
    }

    /// Sets the selected source profile used as a lower-priority fallback.
    pub fn with_source_profile(mut self, source_profile: Option<SourceProfile>) -> Self {
        self.source_profile = source_profile;
        self
    }
}

/// Explains which source selected the effective storage root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageRootSource {
    ExplicitFlag,
    Environment,
    SourceProfile,
    RepoDefault,
}

/// Fully resolved storage root information for a `LocalStore`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedStorageRoot {
    pub path: PathBuf,
    pub source: StorageRootSource,
}

/// Resolves storage root precedence for a repository.
pub fn resolve_storage_root(
    repo_root: &Path,
    options: &StorageOptions,
) -> Result<ResolvedStorageRoot> {
    if let Some(path) = &options.explicit_store_root {
        return Ok(ResolvedStorageRoot {
            path: absolutize_path(repo_root, path)?,
            source: StorageRootSource::ExplicitFlag,
        });
    }

    if let Some(path) = std::env::var_os(STORE_ROOT_ENV).map(PathBuf::from) {
        return Ok(ResolvedStorageRoot {
            path: absolutize_path(repo_root, &path)?,
            source: StorageRootSource::Environment,
        });
    }

    if let Some(path) = options
        .source_profile
        .as_ref()
        .and_then(|profile| profile.store_root.as_ref())
    {
        return Ok(ResolvedStorageRoot {
            path: absolutize_path(repo_root, path)?,
            source: StorageRootSource::SourceProfile,
        });
    }

    Ok(ResolvedStorageRoot {
        path: repo_root.join(PROVENANCE_DIR),
        source: StorageRootSource::RepoDefault,
    })
}

fn absolutize_path(repo_root: &Path, path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        repo_root.join(path)
    };

    if absolute.exists() {
        absolute
            .canonicalize()
            .with_context(|| format!("failed to canonicalize store root {}", absolute.display()))
    } else {
        Ok(absolute)
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::fs;

    use chrono::Utc;

    use super::*;

    fn temp_repo_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-test-store-options-{name}-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(path.join(".git")).expect("create fake git dir");
        path
    }

    fn with_store_root_env<T>(value: Option<&Path>, body: impl FnOnce() -> T) -> T {
        let previous = std::env::var_os(STORE_ROOT_ENV);
        match value {
            Some(path) => std::env::set_var(STORE_ROOT_ENV, path),
            None => std::env::remove_var(STORE_ROOT_ENV),
        }
        let result = body();
        restore_env(previous);
        result
    }

    fn restore_env(previous: Option<OsString>) {
        match previous {
            Some(value) => std::env::set_var(STORE_ROOT_ENV, value),
            None => std::env::remove_var(STORE_ROOT_ENV),
        }
    }

    #[test]
    fn storage_root_resolution_uses_expected_precedence() {
        let repo_root = temp_repo_root("precedence");
        let profile = SourceProfile {
            name: "agent".to_string(),
            app_id: None,
            actor_id: None,
            actor_type: None,
            store_root: Some(PathBuf::from("profile-store")),
            session_db_path: None,
            session_log_path: None,
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        };

        with_store_root_env(None, || {
            let resolved = resolve_storage_root(&repo_root, &StorageOptions::new())
                .expect("resolve default store root");
            assert_eq!(resolved.path, repo_root.join(PROVENANCE_DIR));
            assert_eq!(resolved.source, StorageRootSource::RepoDefault);

            let resolved = resolve_storage_root(
                &repo_root,
                &StorageOptions::new().with_source_profile(Some(profile.clone())),
            )
            .expect("resolve profile store root");
            assert_eq!(resolved.path, repo_root.join("profile-store"));
            assert_eq!(resolved.source, StorageRootSource::SourceProfile);
        });

        with_store_root_env(Some(Path::new("env-store")), || {
            let resolved = resolve_storage_root(
                &repo_root,
                &StorageOptions::new().with_source_profile(Some(profile.clone())),
            )
            .expect("resolve env store root");
            assert_eq!(resolved.path, repo_root.join("env-store"));
            assert_eq!(resolved.source, StorageRootSource::Environment);

            let resolved = resolve_storage_root(
                &repo_root,
                &StorageOptions::new()
                    .with_explicit_store_root(Some(PathBuf::from("flag-store")))
                    .with_source_profile(Some(profile)),
            )
            .expect("resolve explicit store root");
            assert_eq!(resolved.path, repo_root.join("flag-store"));
            assert_eq!(resolved.source, StorageRootSource::ExplicitFlag);
        });
    }
}
