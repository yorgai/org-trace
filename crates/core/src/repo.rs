//! Repository discovery helpers for commands launched from nested workdirs.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// Walks upward from `start` until a Git repository root is found.
pub fn discover_repo_root(start: impl AsRef<Path>) -> Result<PathBuf> {
    let mut current = start
        .as_ref()
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", start.as_ref().display()))?;

    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }

        if !current.pop() {
            return Err(anyhow!("no git repository found"));
        }
    }
}
