//! Git diff metadata capture for patch artifact provenance.
//!
//! This module records file-level diff statistics and stable identifiers without
//! storing full patches or claiming line-level authorship. JSONL events remain
//! the authoritative provenance record.

use std::path::Path;
use std::process::Command;

use anyhow::{anyhow, Context, Result};
use brick_protocol::{
    DiffCapturedPayload, DiffFileChange, DiffFileChangeKind, DiffHunk, DiffTarget, RepoContextId,
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
    apply_hunks(repo_root, &request, &mut file_changes)?;
    apply_file_patch_ids(repo_root, &request, &mut file_changes);
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
                hunks: Vec::new(),
                patch_id: None,
            });
        } else {
            changes.push(DiffFileChange {
                path: first_path,
                old_path: None,
                change_kind: DiffFileChangeKind::Modified,
                additions,
                deletions,
                hunks: Vec::new(),
                patch_id: None,
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

/// Runs `git diff --unified=0 --no-color` (no `-z`) so the unified body keeps
/// its readable `+++`/`@@` headers for hunk parsing.
fn git_diff_unified(repo_root: &Path, request: &DiffCaptureRequest) -> Result<Vec<u8>> {
    let mut command = Command::new("git");
    command.arg("diff").arg("--no-color").arg("--unified=0");
    add_diff_target_args(&mut command, request)?;
    let output = command
        .current_dir(repo_root)
        .output()
        .context("failed to run git diff --unified=0")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git diff --unified=0 failed: {}",
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

/// Runs `git diff -U0` and attaches the parsed per-hunk line ranges to each file
/// change by new-path. `-U0` (zero context) makes each contiguous edit its own
/// minimal hunk, so the `@@` headers carry precise line numbers for blame.
fn apply_hunks(
    repo_root: &Path,
    request: &DiffCaptureRequest,
    changes: &mut [DiffFileChange],
) -> Result<()> {
    let unified = git_diff_unified(repo_root, request)?;
    let text = String::from_utf8_lossy(&unified);
    let by_path = parse_unified_hunks(&text);
    for change in changes.iter_mut() {
        if let Some(hunks) = by_path.get(change.path.as_str()) {
            change.hunks = hunks.clone();
        }
    }
    Ok(())
}

/// Computes and attaches a per-file `git patch-id` to each change. Each file's
/// patch-id is the stable id of *just that file's* diff slice, which is what a
/// later multi-file commit reproduces via `git show <commit> -- <path>`. This
/// is the bridge key line-level blame uses to map a committed line back to the
/// session that captured the change. Best-effort: a file whose patch-id cannot
/// be computed (binary, git failure) is simply left as `None`.
fn apply_file_patch_ids(
    repo_root: &Path,
    request: &DiffCaptureRequest,
    changes: &mut [DiffFileChange],
) {
    for change in changes.iter_mut() {
        change.patch_id = file_patch_id(repo_root, request, &change.path);
    }
}

/// Runs `git diff <target> -- <path>` piped into `git patch-id --stable`,
/// returning the leading hash. Uses default context (matching how a commit's
/// patch-id is later computed) so the working-capture and committed ids agree.
fn file_patch_id(repo_root: &Path, request: &DiffCaptureRequest, path: &str) -> Option<String> {
    let mut diff = Command::new("git");
    diff.arg("diff").arg("--no-color");
    add_diff_target_args(&mut diff, request).ok()?;
    diff.arg("--").arg(path);
    let output = diff.current_dir(repo_root).output().ok()?;
    if !output.status.success() || output.stdout.is_empty() {
        return None;
    }
    let mut child = Command::new("git")
        .arg("patch-id")
        .arg("--stable")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .current_dir(repo_root)
        .spawn()
        .ok()?;
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(&output.stdout).ok()?;
    }
    let out = child.wait_with_output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .split_whitespace()
        .next()
        .map(str::to_string)
}

/// Parses a unified diff body into a map of new-path → hunk line ranges. Reads
/// the `+++ b/<path>` header to key each file and each `@@ -a,b +c,d @@` header
/// for the hunk ranges. Only line numbers and the section header trailer are
/// kept; changed content lines are ignored so no code is copied into events.
pub fn parse_unified_hunks(text: &str) -> std::collections::HashMap<String, Vec<DiffHunk>> {
    let mut by_path: std::collections::HashMap<String, Vec<DiffHunk>> =
        std::collections::HashMap::new();
    let mut current_path: Option<String> = None;
    for raw_line in text.lines() {
        let line = raw_line.trim_start();
        if let Some(rest) = line.strip_prefix("+++ ") {
            current_path = strip_diff_path_prefix(rest.trim());
        } else if line.starts_with("@@ ") {
            if let (Some(path), Some(hunk)) = (current_path.as_ref(), parse_hunk_header(line)) {
                by_path.entry(path.clone()).or_default().push(hunk);
            }
        }
    }
    by_path
}

/// Strips git's `a/` or `b/` prefix from a diff header path, returning `None`
/// for `/dev/null` (pure add/delete sentinel).
fn strip_diff_path_prefix(raw: &str) -> Option<String> {
    if raw == "/dev/null" {
        return None;
    }
    let trimmed = raw
        .strip_prefix("a/")
        .or_else(|| raw.strip_prefix("b/"))
        .unwrap_or(raw);
    Some(trimmed.to_string())
}

/// Parses a single `@@ -old_start,old_lines +new_start,new_lines @@ header` line.
/// Counts default to 1 when omitted (git's convention for single-line ranges).
fn parse_hunk_header(line: &str) -> Option<DiffHunk> {
    let after = line.strip_prefix("@@ ")?;
    let close = after.find(" @@")?;
    let ranges = &after[..close];
    let header = after[close + 3..].trim();
    let header = (!header.is_empty()).then(|| header.to_string());
    let mut parts = ranges.split_whitespace();
    let old = parts.next()?.strip_prefix('-')?;
    let new = parts.next()?.strip_prefix('+')?;
    let (old_start, old_lines) = parse_range_pair(old)?;
    let (new_start, new_lines) = parse_range_pair(new)?;
    Some(DiffHunk {
        old_start,
        old_lines,
        new_start,
        new_lines,
        header,
    })
}

/// Parses a `start,count` or bare `start` range into `(start, count)`.
fn parse_range_pair(raw: &str) -> Option<(u64, u64)> {
    let mut parts = raw.splitn(2, ',');
    let start = parts.next()?.parse::<u64>().ok()?;
    let count = match parts.next() {
        Some(value) => value.parse::<u64>().ok()?,
        None => 1,
    };
    Some((start, count))
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
        for hunk in &change.hunks {
            normalized.push_str(&format!(
                "\thunk\t{}\t{}\t{}\t{}\n",
                hunk.old_start, hunk.old_lines, hunk.new_start, hunk.new_lines
            ));
        }
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
                hunks: Vec::new(),
                patch_id: None,
            },
            DiffFileChange {
                path: "a.rs".to_string(),
                old_path: None,
                change_kind: DiffFileChangeKind::Deleted,
                additions: Some(0),
                deletions: Some(2),
                hunks: Vec::new(),
                patch_id: None,
            },
        ];
        let mut right = left.clone();
        right.reverse();

        assert_eq!(
            compute_summary_hash(DiffTarget::Working, None, None, &left),
            compute_summary_hash(DiffTarget::Working, None, None, &right)
        );
    }

    #[test]
    fn parses_unified_hunks_with_ranges_and_header() {
        let diff = "diff --git a/src/auth.rs b/src/auth.rs\n\
                    index 1111111..2222222 100644\n\
                    --- a/src/auth.rs\n\
                    +++ b/src/auth.rs\n\
                    @@ -10,2 +10,3 @@ fn refresh()\n\
                    -old\n\
                    +new1\n\
                    +new2\n\
                    @@ -40 +41,0 @@\n\
                    -gone\n";
        let by_path = parse_unified_hunks(diff);
        let hunks = by_path.get("src/auth.rs").expect("path present");
        assert_eq!(hunks.len(), 2);
        assert_eq!(hunks[0].old_start, 10);
        assert_eq!(hunks[0].old_lines, 2);
        assert_eq!(hunks[0].new_start, 10);
        assert_eq!(hunks[0].new_lines, 3);
        assert_eq!(hunks[0].header.as_deref(), Some("fn refresh()"));
        // Bare `-40` means count defaults to 1; `+41,0` is a pure deletion.
        assert_eq!(hunks[1].old_start, 40);
        assert_eq!(hunks[1].old_lines, 1);
        assert_eq!(hunks[1].new_start, 41);
        assert_eq!(hunks[1].new_lines, 0);
    }

    #[test]
    fn unified_parser_ignores_dev_null_added_file_old_side() {
        let diff = "diff --git a/new.rs b/new.rs\n\
                    new file mode 100644\n\
                    --- /dev/null\n\
                    +++ b/new.rs\n\
                    @@ -0,0 +1,2 @@\n\
                    +line1\n\
                    +line2\n";
        let by_path = parse_unified_hunks(diff);
        let hunks = by_path.get("new.rs").expect("new path present");
        assert_eq!(hunks.len(), 1);
        assert_eq!(hunks[0].new_start, 1);
        assert_eq!(hunks[0].new_lines, 2);
    }
}
