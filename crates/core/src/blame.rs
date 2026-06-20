//! Line-level AI blame: maps each current line of a file to the Brick session,
//! actor, and mission that produced it.
//!
//! Attribution is reconstructed from the append-only event log (the source of
//! truth), never from a mutable cache. The challenge is that a `diff.captured`
//! event records hunk line ranges at *capture time* (working-tree coordinates),
//! but later commits and edits drift those line numbers. Two ideas combine to
//! survive that drift:
//!
//! 1. **`git blame` solves line→commit drift.** For every current line, git
//!    already maps it to the commit that last touched it, regardless of how many
//!    later edits shifted it.
//! 2. **`git patch-id` solves capture→commit identity.** A patch-id is a stable
//!    hash of a diff that is invariant across the working-tree → commit
//!    transition (git normalizes line numbers and context). Brick records the
//!    patch-id of every captured diff, so we can map a line's *commit* to the
//!    session/actor that *captured the same change* — even though the capture
//!    happened against the working tree before the commit existed.
//!
//! So the committed path is: line → (git blame) commit → (git patch-id) patch
//! identity → (event log) session/actor/mission. The only time we fall back to
//! the raw captured hunk ranges is for a diff that is still uncommitted (its
//! patch-id matches no commit), where the recorded line numbers are by
//! definition still in current-file coordinates.

use std::collections::HashMap;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, Context, Result};
use brick_protocol::{DiffCapturedPayload, DiffTarget, EventType, TraceEvent};
use serde::{Deserialize, Serialize};

use crate::store::LocalStore;

/// Confidence in a single line's attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlameConfidence {
    /// Attributed via `git blame` commit → captured patch-id → Brick session.
    /// Survives line drift and the working-tree → commit transition.
    Commit,
    /// Attributed via an *uncommitted* working diff's hunk in current line
    /// coordinates (the change has not entered git history yet).
    Working,
    /// No Brick diff event covers this line.
    Unattributed,
}

/// Attribution for a single current line of a file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlameLine {
    pub line_no: u64,
    pub session_id: Option<String>,
    pub actor_type: Option<String>,
    pub actor_id: Option<String>,
    pub mission_id: Option<String>,
    pub commit: Option<String>,
    pub occurred_at: Option<String>,
    pub source_event_id: Option<String>,
    pub confidence: BlameConfidence,
}

/// The Brick-side attribution carried by a `diff.captured` event.
#[derive(Debug, Clone)]
struct Attribution {
    session_id: Option<String>,
    actor_type: String,
    actor_id: String,
    mission_id: Option<String>,
    occurred_at: String,
    event_id: String,
}

/// Computes line-level blame for `rel_path` (repo-relative) using the event log
/// and the working tree at `repo_root`.
pub fn blame_file(store: &LocalStore, repo_root: &Path, rel_path: &str) -> Result<Vec<BlameLine>> {
    let events = store.read_all_events()?;
    let diff_events = collect_diff_events(&events, rel_path);

    // Index captured changes by this file's per-file patch-id (the
    // cross-commit-stable identity) and remember the most recent working diff
    // for the uncommitted overlay. We use the per-file id, not the whole-diff
    // id, because the commit that later lands the change usually touches other
    // files too — only the per-file slice (`git show <commit> -- <path>`)
    // reproduces the captured id.
    let mut patch_to_attr: HashMap<String, Attribution> = HashMap::new();
    let mut latest_working: Option<(Attribution, DiffCapturedPayload)> = None;
    for (event, payload) in &diff_events {
        let attr = attribution_of(event, payload);
        for change in payload
            .file_changes
            .iter()
            .filter(|change| change.path == rel_path)
        {
            if let Some(patch_id) = change.patch_id.clone() {
                patch_to_attr.insert(patch_id, attr.clone());
            }
        }
        if payload.diff_target == DiffTarget::Working {
            // Events are time-ordered; keep the last working diff.
            latest_working = Some((attr, payload.clone()));
        }
    }

    let line_count = current_line_count(repo_root, rel_path)?;
    // Blame the WORKING TREE (not HEAD): every line is reported in current-file
    // coordinates, committed lines carry their commit SHA, and uncommitted lines
    // carry an all-zero SHA. This single coordinate space is what lets the
    // committed (patch-id) path and the uncommitted (hunk overlay) path compose
    // without drift.
    let blame_commits = git_blame_line_commits(repo_root, rel_path).unwrap_or_default();

    // Resolve each blamed commit to this file's per-file patch-id once.
    let mut commit_patch_ids: HashMap<String, Option<String>> = HashMap::new();

    let mut lines = Vec::with_capacity(line_count as usize);
    for line_no in 1..=line_count {
        let mut line = BlameLine {
            line_no,
            session_id: None,
            actor_type: None,
            actor_id: None,
            mission_id: None,
            commit: None,
            occurred_at: None,
            source_event_id: None,
            confidence: BlameConfidence::Unattributed,
        };

        if let Some(commit) = blame_commits.get(&line_no) {
            if !is_zero_sha(commit) {
                line.commit = Some(commit.clone());
                let patch_id = commit_patch_ids
                    .entry(commit.clone())
                    .or_insert_with(|| commit_file_patch_id(repo_root, commit, rel_path));
                if let Some(patch_id) = patch_id {
                    if let Some(attr) = patch_to_attr.get(patch_id) {
                        apply_attr(&mut line, attr, BlameConfidence::Commit);
                    }
                }
            }
        }

        lines.push(line);
    }

    // Overlay the latest working diff onto the lines git reports as uncommitted
    // (all-zero SHA). Those lines are, by construction, in current-file
    // coordinates, so the captured hunk ranges line up exactly. Committed lines
    // are never touched here — the patch-id path above already attributed them,
    // which is what survives later line drift.
    if let Some((attr, payload)) = &latest_working {
        for change in payload
            .file_changes
            .iter()
            .filter(|change| change.path == rel_path)
        {
            for hunk in &change.hunks {
                if hunk.new_lines == 0 {
                    continue;
                }
                let start = hunk.new_start;
                let end = hunk.new_start + hunk.new_lines - 1;
                for line in lines.iter_mut() {
                    let uncommitted = blame_commits
                        .get(&line.line_no)
                        .map(|sha| is_zero_sha(sha))
                        .unwrap_or(true);
                    if uncommitted && line.line_no >= start && line.line_no <= end {
                        apply_attr(line, attr, BlameConfidence::Working);
                    }
                }
            }
        }
    }

    Ok(lines)
}

/// True for git's all-zero "not yet committed" boundary SHA.
fn is_zero_sha(sha: &str) -> bool {
    sha.bytes().all(|b| b == b'0')
}

fn apply_attr(line: &mut BlameLine, attr: &Attribution, confidence: BlameConfidence) {
    line.session_id = attr.session_id.clone();
    line.actor_type = Some(attr.actor_type.clone());
    line.actor_id = Some(attr.actor_id.clone());
    line.mission_id = attr.mission_id.clone();
    line.occurred_at = Some(attr.occurred_at.clone());
    line.source_event_id = Some(attr.event_id.clone());
    line.confidence = confidence;
}

/// Extracts the `diff.captured` events touching `rel_path`, decoded and paired
/// with their payloads, preserving event-log order.
fn collect_diff_events<'a>(
    events: &'a [TraceEvent],
    rel_path: &str,
) -> Vec<(&'a TraceEvent, DiffCapturedPayload)> {
    let mut out = Vec::new();
    for event in events {
        if event.event_type != EventType::DiffCaptured {
            continue;
        }
        let Ok(payload) = serde_json::from_value::<DiffCapturedPayload>(event.payload.clone())
        else {
            continue;
        };
        if payload
            .file_changes
            .iter()
            .any(|change| change.path == rel_path)
        {
            out.push((event, payload));
        }
    }
    out
}

fn attribution_of(event: &TraceEvent, _payload: &DiffCapturedPayload) -> Attribution {
    Attribution {
        session_id: event.session_id.as_ref().map(|id| id.to_string()),
        actor_type: format!("{:?}", event.actor.actor_type).to_lowercase(),
        actor_id: event.actor.actor_id.clone(),
        mission_id: event.mission_id.as_ref().map(|id| id.to_string()),
        occurred_at: event.occurred_at.to_rfc3339(),
        event_id: event.event_id.to_string(),
    }
}

/// Returns the number of lines in the current working-tree version of the file.
fn current_line_count(repo_root: &Path, rel_path: &str) -> Result<u64> {
    let full = repo_root.join(rel_path);
    let content = std::fs::read_to_string(&full)
        .with_context(|| format!("failed to read {} for blame", full.display()))?;
    Ok(content.lines().count() as u64)
}

/// Runs `git blame --line-porcelain` against the WORKING TREE (no commit-ish)
/// and returns a map of current line number → commit SHA. Committed lines carry
/// their commit SHA; lines changed in the working tree carry git's all-zero
/// "not committed yet" SHA, which the caller treats as the uncommitted overlay
/// region. Reporting in current-file coordinates is what keeps both attribution
/// paths drift-free.
fn git_blame_line_commits(repo_root: &Path, rel_path: &str) -> Result<HashMap<u64, String>> {
    let output = Command::new("git")
        .arg("blame")
        .arg("--line-porcelain")
        .arg("--")
        .arg(rel_path)
        .current_dir(repo_root)
        .output()
        .context("failed to run git blame")?;
    if !output.status.success() {
        return Err(anyhow!(
            "git blame failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Ok(parse_porcelain(&text))
}

/// Parses `git blame --line-porcelain` output into final-line → commit SHA.
/// Header lines look like `<40-hex sha> <orig_line> <final_line> [<num_lines>]`;
/// content lines (tab-prefixed) and other porcelain fields are skipped.
fn parse_porcelain(text: &str) -> HashMap<u64, String> {
    let mut map = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(sha) = parts.next() else { continue };
        if sha.len() != 40 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        let (_, final_line) = (parts.next(), parts.next());
        if let Some(final_line) = final_line.and_then(|value| value.parse::<u64>().ok()) {
            map.insert(final_line, sha.to_string());
        }
    }
    map
}

/// Computes the stable per-file patch-id of a single commit by piping
/// `git show <commit> -- <path>` into `git patch-id --stable`. This isolates
/// the commit's slice for one file, which is exactly what the capture side
/// recorded for that file — so it matches even when the commit touched other
/// files too. Returns `None` for merge/empty slices or any git failure.
fn commit_file_patch_id(repo_root: &Path, commit: &str, rel_path: &str) -> Option<String> {
    let show = Command::new("git")
        .arg("show")
        .arg("--no-color")
        .arg(commit)
        .arg("--")
        .arg(rel_path)
        .current_dir(repo_root)
        .output()
        .ok()?;
    if !show.status.success() || show.stdout.is_empty() {
        return None;
    }
    patch_id_of(repo_root, &show.stdout)
}

/// Pipes a raw diff into `git patch-id --stable` and returns the leading hash.
fn patch_id_of(repo_root: &Path, diff: &[u8]) -> Option<String> {
    let mut child = Command::new("git")
        .arg("patch-id")
        .arg("--stable")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .current_dir(repo_root)
        .spawn()
        .ok()?;
    if let Some(stdin) = child.stdin.as_mut() {
        use std::io::Write;
        stdin.write_all(diff).ok()?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_header_maps_final_line_to_commit() {
        let porcelain = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa 1 1 1\n\
                         \tfn main() {\n\
                         bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb 2 2 1\n\
                         \t    let x = 1;\n";
        let map = parse_porcelain(porcelain);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map.get(&1).map(String::as_str),
            Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
        );
        assert_eq!(
            map.get(&2).map(String::as_str),
            Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
        );
    }

    #[test]
    fn porcelain_ignores_non_header_lines() {
        // No 40-hex header → empty map; author/content lines must not leak in.
        let porcelain = "author Jane\n\tactual code line\nfilename src/main.rs\n";
        assert!(parse_porcelain(porcelain).is_empty());
    }

    /// Regression guard for the events-authoritative blame invariant: the derived
    /// index struct must NOT carry `patch_id`. Owner blame reads `patch_id` only
    /// from the JSONL event payload (see `blame_file`), so mirroring it into the
    /// cache is forbidden — that would let stale-cache code silently mis-attribute.
    /// If someone adds the field, this test fails and forces a deliberate decision.
    #[test]
    fn indexed_diff_file_change_does_not_carry_patch_id() {
        use crate::index_types::IndexedDiffFileChange;
        use brick_protocol::DiffFileChangeKind;

        let change = IndexedDiffFileChange {
            path: "src/main.rs".to_string(),
            old_path: None,
            change_kind: DiffFileChangeKind::Modified,
            additions: Some(1),
            deletions: Some(0),
            hunks: Vec::new(),
        };
        let value = serde_json::to_value(&change).expect("serialize indexed diff change");
        assert!(
            value.get("patch_id").is_none(),
            "IndexedDiffFileChange must not carry patch_id — blame is events-authoritative"
        );
    }
}
