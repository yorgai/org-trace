//! Git context capture for provenance events.
//!
//! Captured repo context is best-effort: missing upstreams or remotes produce
//! `None`, but local event recording should still proceed.

use std::path::Path;
use std::process::Command;

use brick_protocol::{ContextMode, RepoContextCapturedPayload};

/// Captures best-effort Git state for an event written from `work_dir`.
pub fn capture_repo_context(repo_root: &Path, work_dir: &Path) -> RepoContextCapturedPayload {
    RepoContextCapturedPayload {
        repo_root: repo_root.display().to_string(),
        work_dir: work_dir.display().to_string(),
        remote_url: git_output(repo_root, ["config", "--get", "remote.origin.url"]),
        branch: git_output(repo_root, ["branch", "--show-current"]),
        upstream_branch: git_output(
            repo_root,
            ["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"],
        ),
        head_commit: git_output(repo_root, ["rev-parse", "HEAD"]),
        merge_base_commit: git_output(repo_root, ["merge-base", "HEAD", "@{u}"]),
        dirty: is_dirty(repo_root),
        context_mode: ContextMode::AttachedCurrentBranch,
    }
}

fn git_output<const N: usize>(repo_root: &Path, args: [&str; N]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?.trim().to_string();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn is_dirty(repo_root: &Path) -> bool {
    git_output(
        repo_root,
        ["status", "--porcelain", "--", ".", ":!.brick/provenance"],
    )
    .map(|output| !output.trim().is_empty())
    .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::process::Command;

    use chrono::Utc;

    use super::*;
    use crate::PROVENANCE_DIR;

    #[test]
    fn repo_context_dirty_ignores_provenance_storage() {
        let repo_root = std::env::temp_dir().join(format!(
            "brick-test-real-git-{}",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&repo_root).expect("create temp repo");
        Command::new("git")
            .arg("init")
            .current_dir(&repo_root)
            .output()
            .expect("init git repo");
        fs::write(repo_root.join("tracked.txt"), "tracked").expect("write tracked file");
        Command::new("git")
            .args(["add", "tracked.txt"])
            .current_dir(&repo_root)
            .output()
            .expect("git add");
        Command::new("git")
            .args([
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.com",
                "commit",
                "-m",
                "init",
            ])
            .current_dir(&repo_root)
            .output()
            .expect("git commit");

        fs::create_dir_all(repo_root.join(PROVENANCE_DIR)).expect("create provenance dir");
        fs::write(repo_root.join(PROVENANCE_DIR).join("local.json"), "{}")
            .expect("write provenance file");

        let context = capture_repo_context(&repo_root, &repo_root);
        assert!(!context.dirty);
    }
}
