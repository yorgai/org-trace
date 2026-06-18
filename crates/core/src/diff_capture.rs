//! Git diff metadata capture for patch artifact provenance.
//!
//! This module records file-level diff statistics and stable identifiers without
//! storing full patches or claiming line-level authorship. JSONL events remain
//! the authoritative provenance record.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use brick_protocol::{
    DiffCapturedPayload, DiffFileChange, DiffFileChangeKind, DiffTarget, RepoContextId,
};
use sha2::{Digest, Sha256};

/// Request describing which Git diff should be summarized.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffCaptureRequest {
    pub target: DiffTarget,
    pub base_commit: Option<String>,
    pub head_commit: Option<String>,
    pub repo_context_id: Option<RepoContextId>,
}

/// Captures Git diff metadata for artifact/session provenance.
pub fn capture_diff(repo_root: &Path, request: DiffCaptureRequest) -> Result<DiffCapturedPayload> {
    let base_commit = request.base_commit.clone();
    let head_commit = request.head_commit.clone();
    let numstat_output = git_diff_output(repo_root, &request, "--numstat")?;
    let summary_output = git_diff_output(repo_root, &request, "--summary")?;
    let mut file_changes = parse_numstat(&numstat_output)?;
    apply_summary(&mut file_changes, &summary_output);
    let patch_id = compute_patch_id(repo_root, &request)?;
    let summary_hash = compute_summary_hash(
        request.target,
        base_commit.as_deref(),
        head_commit.as_deref(),
        &file_changes,
    );

    Ok(DiffCapturedPayload {
        diff_target: request.target,
        base_commit,
        head_commit,
        patch_id,
        summary_hash,
        file_changes,
        repo_context_id: request.repo_context_id,
    })
}

/// Parses `git diff --numstat -z` output into stable file summaries.
pub fn parse_numstat(output: &[u8]) -> Result<Vec<DiffFileChange>> {
    if output.is_empty() {
        return Ok(Vec::new());
    }

    let fields = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8(field.to_vec()).context("git numstat output was not UTF-8"))
        .collect::<Result<Vec<_>>>()?;

    let mut changes = Vec::new();
    let mut index = 0;
    while index < fields.len() {
        let header = &fields[index];
        index += 1;
        let (additions, deletions, first_path) = parse_numstat_header(header)?;
        if first_path.is_empty() {
            if index + 1 >= fields.len() {
                return Err(anyhow!("git numstat rename entry was missing paths"));
            }
            let old_path = fields[index].clone();
            let path = fields[index + 1].clone();
            index += 2;
            changes.push(DiffFileChange {
                path,
                old_path: Some(old_path),
                change_kind: DiffFileChangeKind::Renamed,
                additions,
                deletions,
            });
        } else {
            changes.push(DiffFileChange {
                path: first_path,
                old_path: None,
                change_kind: DiffFileChangeKind::Modified,
                additions,
                deletions,
            });
        }
    }

    Ok(changes)
}

fn parse_numstat_header(header: &str) -> Result<(Option<u64>, Option<u64>, String)> {
    let mut parts = header.splitn(3, '\t');
    let additions = parse_numstat_count(parts.next(), "additions")?;
    let deletions = parse_numstat_count(parts.next(), "deletions")?;
    let path = parts.next().unwrap_or_default().to_string();
    Ok((additions, deletions, path))
}

fn parse_numstat_count(value: Option<&str>, label: &str) -> Result<Option<u64>> {
    match value {
        Some("-") => Ok(None),
        Some(raw) => raw
            .trim()
            .parse::<u64>()
            .map(Some)
            .with_context(|| format!("invalid numstat {label}: {raw}")),
        None => Err(anyhow!("missing numstat {label}")),
    }
}

fn git_diff_output(
    repo_root: &Path,
    request: &DiffCaptureRequest,
    option: &str,
) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.arg("diff").arg("-z").arg(option);
    add_diff_target_args(&mut command, request)?;
    let output = command
        .current_dir(repo_root)
        .output()
        .with_context(|| format!("failed to run git diff {option}"))?;
    if !output.status.success() {
        return Err(anyhow!(
            "git diff {option} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output.stdout)
}

fn compute_patch_id(repo_root: &Path, request: &DiffCaptureRequest) -> Result<Option<String>> {
    let mut diff = Command::new("git");
    diff.arg("diff");
    add_diff_target_args(&mut diff, request)?;
    let output = diff
        .current_dir(repo_root)
        .output()
        .context("failed to run git diff for patch-id")?;
    if !output.status.success() || output.stdout.is_empty() {
        return Ok(None);
    }

    let mut patch_id = Command::new("git")
        .arg("patch-id")
        .arg("--stable")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .current_dir(repo_root)
        .spawn()
        .context("failed to spawn git patch-id")?;
    if let Some(stdin) = patch_id.stdin.as_mut() {
        use std::io::Write;
        stdin
            .write_all(&output.stdout)
            .context("failed to write diff to git patch-id")?;
    }
    let patch_output = patch_id
        .wait_with_output()
        .context("failed to read git patch-id output")?;
    if !patch_output.status.success() {
        return Ok(None);
    }
    let value = String::from_utf8_lossy(&patch_output.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string);
    Ok(value)
}

fn add_diff_target_args(command: &mut Command, request: &DiffCaptureRequest) -> Result<()> {
    match request.target {
        DiffTarget::Working => Ok(()),
        DiffTarget::Staged => {
            command.arg("--cached");
            Ok(())
        }
        DiffTarget::Range => {
            let base = request
                .base_commit
                .as_deref()
                .ok_or_else(|| anyhow!("range diff capture requires --base"))?;
            let head = request
                .head_commit
                .as_deref()
                .ok_or_else(|| anyhow!("range diff capture requires --head"))?;
            command.arg(format!("{base}..{head}"));
            Ok(())
        }
    }
}

fn apply_summary(changes: &mut [DiffFileChange], output: &[u8]) {
    if output.is_empty() {
        return;
    }
    let Ok(fields) = output
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| String::from_utf8(field.to_vec()))
        .collect::<Result<Vec<_>, _>>()
    else {
        return;
    };

    for field in fields {
        let trimmed = field.trim();
        if let Some(path) = trimmed.strip_prefix("create mode ").and_then(last_word) {
            set_kind(changes, path, DiffFileChangeKind::Added);
        } else if let Some(path) = trimmed.strip_prefix("delete mode ").and_then(last_word) {
            set_kind(changes, path, DiffFileChangeKind::Deleted);
        } else if trimmed.starts_with("rename ") {
            for change in changes
                .iter_mut()
                .filter(|change| change.old_path.is_some())
            {
                change.change_kind = DiffFileChangeKind::Renamed;
            }
        } else if trimmed.starts_with("copy ") {
            for change in changes
                .iter_mut()
                .filter(|change| change.old_path.is_some())
            {
                change.change_kind = DiffFileChangeKind::Copied;
            }
        } else if let Some(path) = trimmed.strip_prefix("mode change ").and_then(last_word) {
            set_kind(changes, path, DiffFileChangeKind::TypeChanged);
        }
    }
}

fn last_word(value: &str) -> Option<&str> {
    value.split_whitespace().last()
}

fn set_kind(changes: &mut [DiffFileChange], path: &str, kind: DiffFileChangeKind) {
    if let Some(change) = changes.iter_mut().find(|change| change.path == path) {
        change.change_kind = kind;
    }
}

fn compute_summary_hash(
    target: DiffTarget,
    base_commit: Option<&str>,
    head_commit: Option<&str>,
    changes: &[DiffFileChange],
) -> String {
    let mut normalized = String::new();
    normalized.push_str(&format!("target={target:?}\n"));
    normalized.push_str(&format!("base={}\n", base_commit.unwrap_or_default()));
    normalized.push_str(&format!("head={}\n", head_commit.unwrap_or_default()));
    let mut sorted = changes.to_vec();
    sorted.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.old_path.cmp(&right.old_path))
    });
    for change in sorted {
        normalized.push_str(&format!(
            "{}\t{}\t{:?}\t{}\t{}\n",
            change.old_path.as_deref().unwrap_or(""),
            change.path,
            change.change_kind,
            change
                .additions
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string()),
            change
                .deletions
                .map(|value| value.to_string())
                .unwrap_or_else(|| "-".to_string())
        ));
    }
    format!("{:x}", Sha256::digest(normalized.as_bytes()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_text_binary_and_rename_numstat() {
        let output =
            b"3\t1\tsrc/lib.rs\0-\t-\tassets/logo.png\0 2\t0\t\0old name.rs\0new name.rs\0";
        let changes = parse_numstat(output).expect("parse numstat");

        assert_eq!(changes.len(), 3);
        assert_eq!(changes[0].path, "src/lib.rs");
        assert_eq!(changes[0].additions, Some(3));
        assert_eq!(changes[0].deletions, Some(1));
        assert_eq!(changes[1].path, "assets/logo.png");
        assert_eq!(changes[1].additions, None);
        assert_eq!(changes[1].deletions, None);
        assert_eq!(changes[2].old_path.as_deref(), Some("old name.rs"));
        assert_eq!(changes[2].path, "new name.rs");
        assert_eq!(changes[2].change_kind, DiffFileChangeKind::Renamed);
    }

    #[test]
    fn stable_summary_hash_ignores_input_order() {
        let left = vec![
            DiffFileChange {
                path: "b.rs".to_string(),
                old_path: None,
                change_kind: DiffFileChangeKind::Modified,
                additions: Some(1),
                deletions: Some(0),
            },
            DiffFileChange {
                path: "a.rs".to_string(),
                old_path: None,
                change_kind: DiffFileChangeKind::Deleted,
                additions: Some(0),
                deletions: Some(2),
            },
        ];
        let mut right = left.clone();
        right.reverse();

        assert_eq!(
            compute_summary_hash(DiffTarget::Working, None, None, &left),
            compute_summary_hash(DiffTarget::Working, None, None, &right)
        );
    }
}
