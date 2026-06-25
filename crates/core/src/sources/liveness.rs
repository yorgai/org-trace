//! Transient liveness probing for native source sessions.
//!
//! Brick never persists liveness — a stored "active" flag is wrong the instant
//! it lands. Instead we recompute it per scan from two cheap signals:
//!
//! 1. **Turn boundaries in the source's own transcript** — the authoritative
//!    signal. Codex writes `task_started` / `task_complete`; Claude writes an
//!    `assistant` record whose `message.stop_reason` is set when a turn ends.
//!    Reading just the **tail** of the file tells us whether the last turn is
//!    still open, without re-parsing the whole transcript.
//! 2. **File mtime recency** — the fallback for SQLite-backed tools (Cursor,
//!    Windsurf) and any source without explicit turn markers.
//!
//! Only sessions whose file changed within [`ACTIVE_WINDOW`] are probed for turn
//! state; everything older is `Idle` without opening the file, so a large
//! `~/.claude/projects` tree costs O(recently-touched) reads, not O(all files).

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::Value;

use crate::Liveness;

use super::traits::KnownSource;

/// How recently a transcript must have changed to be considered possibly active.
/// Generous on purpose: an agent that is thinking, calling a slow tool, or
/// waiting for user approval does not write to its transcript, and we must not
/// flip it to `Idle` mid-turn. Tunable from one place.
pub const ACTIVE_WINDOW: Duration = Duration::from_secs(120);

/// Max bytes to read from the end of a transcript when probing turn state.
/// A handful of trailing records is plenty; this bounds work on huge files.
const TAIL_BYTES: u64 = 16 * 1024;

/// Returns true when `modified_at` falls within [`ACTIVE_WINDOW`] of now.
pub fn within_active_window(modified_at: Option<SystemTime>) -> bool {
    let Some(modified_at) = modified_at else {
        return false;
    };
    match modified_at.elapsed() {
        Ok(elapsed) => elapsed <= ACTIVE_WINDOW,
        // `elapsed()` errors when the mtime is in the future (clock skew); treat
        // a future mtime as "just changed".
        Err(_) => true,
    }
}

/// Probes liveness for a single session given its source app id and the
/// session's own activity instant.
///
/// `activity_at` must be the *per-session* last-activity time, not a shared file
/// mtime: SQLite-backed sources (orgii, cursor, …) store every session in one
/// `.db`, so its mtime moves whenever *any* session writes. Passing each
/// session's `session_updated_at` keeps one busy session from marking all its
/// siblings active. For per-file JSONL sources the two coincide.
///
/// `source_app_id` selects the turn-signal parser; unknown sources fall back to
/// pure recency. Sessions outside the active window short-circuit to `Idle`
/// without any read.
pub fn probe_liveness(
    path: &Path,
    source_app_id: &str,
    activity_at: Option<SystemTime>,
) -> Liveness {
    if !within_active_window(activity_at) {
        return Liveness::Idle;
    }

    // Within the window: consult turn signals for JSONL tools, else trust the
    // per-session activity time we already gated on.
    match KnownSource::from_name(source_app_id) {
        Some(KnownSource::CodexApp) => probe_codex(path).unwrap_or(Liveness::Active),
        Some(KnownSource::ClaudeCode) => probe_claude(path).unwrap_or(Liveness::Active),
        // SQLite-backed and unknown sources have no per-turn markers; a recent
        // per-session activity time is the only evidence, and we already know it
        // is in-window. Listed explicitly so a newly-added JSONL tool that needs
        // tail-probing is a compile error here rather than a silent "Active".
        Some(
            KnownSource::CursorIde
            | KnownSource::CursorAgent
            | KnownSource::OpenCode
            | KnownSource::Windsurf
            | KnownSource::Orgii
            | KnownSource::Gemini,
        )
        | None => Liveness::Active,
    }
}

/// Reads up to the last [`TAIL_BYTES`] of a file and returns its complete lines,
/// oldest-first. The first (possibly partial) line is dropped unless we read
/// from the very start, and a trailing partial line (a record mid-write) is also
/// dropped — both are the price of reading a live, still-growing transcript.
pub fn tail_lines(path: &Path) -> std::io::Result<Vec<String>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start))?;
    let mut buf = Vec::with_capacity((len - start) as usize);
    file.read_to_end(&mut buf)?;

    let text = String::from_utf8_lossy(&buf);
    let mut lines: Vec<String> = text.lines().map(ToOwned::to_owned).collect();

    // If we started mid-file, the first line is almost certainly a fragment.
    if start > 0 && !lines.is_empty() {
        lines.remove(0);
    }
    // A transcript being actively appended may end mid-record. If the raw buffer
    // does not end in a newline, the last line is incomplete — drop it.
    if !buf.ends_with(b"\n") && !lines.is_empty() {
        lines.pop();
    }
    Ok(lines)
}

/// Parses tail lines as JSON, skipping anything that does not parse (e.g. a
/// half-written record that slipped through).
fn tail_json(path: &Path) -> Option<Vec<Value>> {
    let lines = tail_lines(path).ok()?;
    Some(
        lines
            .iter()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .collect(),
    )
}

/// Codex: scan the tail for the last `task_started` vs `task_complete`. If a
/// `task_started` appears after the last `task_complete` (or there is no
/// completion at all), a turn is still open → `Active`.
fn probe_codex(path: &Path) -> Option<Liveness> {
    let values = tail_json(path)?;
    let mut last_started: Option<usize> = None;
    let mut last_complete: Option<usize> = None;
    for (index, value) in values.iter().enumerate() {
        let payload_type = value
            .get("payload")
            .and_then(|payload| payload.get("type"))
            .and_then(Value::as_str)
            .or_else(|| value.get("type").and_then(Value::as_str));
        match payload_type {
            Some("task_started") => last_started = Some(index),
            Some("task_complete") => last_complete = Some(index),
            _ => {}
        }
    }
    let live = match (last_started, last_complete) {
        (Some(started), Some(complete)) => started > complete,
        (Some(_), None) => true,
        _ => false,
    };
    Some(if live {
        Liveness::Active
    } else {
        Liveness::Idle
    })
}

/// Claude: the last `assistant` record carries `message.stop_reason` when its
/// turn finished. A trailing assistant without a stop reason, or a pending
/// `queue-operation`, means a turn is in flight → `Active`.
fn probe_claude(path: &Path) -> Option<Liveness> {
    let values = tail_json(path)?;
    for value in values.iter().rev() {
        match value.get("type").and_then(Value::as_str) {
            Some("queue-operation") => return Some(Liveness::Active),
            Some("assistant") => {
                let stopped = value
                    .get("message")
                    .and_then(|message| message.get("stop_reason"))
                    .map(|reason| !reason.is_null())
                    .unwrap_or(false);
                return Some(if stopped {
                    Liveness::Idle
                } else {
                    Liveness::Active
                });
            }
            // A trailing `user` record means we are waiting on the assistant.
            Some("user") => return Some(Liveness::Active),
            _ => continue,
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_file(name: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-liveness-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut file = File::create(&path).expect("create temp");
        file.write_all(body.as_bytes()).expect("write");
        path
    }

    #[test]
    fn codex_open_turn_is_active() {
        let body = "{\"type\":\"session_meta\",\"payload\":{\"type\":\"session_meta\"}}\n\
                    {\"payload\":{\"type\":\"task_started\"}}\n";
        let path = temp_file("codex-open", body);
        assert_eq!(probe_codex(&path), Some(Liveness::Active));
    }

    #[test]
    fn codex_completed_turn_is_idle() {
        let body = "{\"payload\":{\"type\":\"task_started\"}}\n\
                    {\"payload\":{\"type\":\"task_complete\"}}\n";
        let path = temp_file("codex-done", body);
        assert_eq!(probe_codex(&path), Some(Liveness::Idle));
    }

    #[test]
    fn codex_restarted_after_complete_is_active() {
        let body = "{\"payload\":{\"type\":\"task_started\"}}\n\
                    {\"payload\":{\"type\":\"task_complete\"}}\n\
                    {\"payload\":{\"type\":\"task_started\"}}\n";
        let path = temp_file("codex-restart", body);
        assert_eq!(probe_codex(&path), Some(Liveness::Active));
    }

    #[test]
    fn claude_assistant_with_stop_reason_is_idle() {
        let body = "{\"type\":\"user\",\"message\":{}}\n\
                    {\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n";
        let path = temp_file("claude-done", body);
        assert_eq!(probe_claude(&path), Some(Liveness::Idle));
    }

    #[test]
    fn claude_assistant_without_stop_reason_is_active() {
        let body = "{\"type\":\"assistant\",\"message\":{\"stop_reason\":null}}\n";
        let path = temp_file("claude-open", body);
        assert_eq!(probe_claude(&path), Some(Liveness::Active));
    }

    #[test]
    fn claude_trailing_user_is_active() {
        let body = "{\"type\":\"assistant\",\"message\":{\"stop_reason\":\"end_turn\"}}\n\
                    {\"type\":\"user\",\"message\":{}}\n";
        let path = temp_file("claude-userwait", body);
        assert_eq!(probe_claude(&path), Some(Liveness::Active));
    }

    #[test]
    fn tail_drops_incomplete_trailing_line() {
        let body = "{\"a\":1}\n{\"b\":2}\n{\"c\":3"; // last line no newline
        let path = temp_file("tail-partial", body);
        let lines = tail_lines(&path).expect("tail");
        assert_eq!(lines, vec!["{\"a\":1}", "{\"b\":2}"]);
    }

    #[test]
    fn tail_empty_file() {
        let path = temp_file("tail-empty", "");
        assert!(tail_lines(&path).expect("tail").is_empty());
    }

    #[test]
    fn within_window_recent_true_old_false() {
        assert!(within_active_window(Some(SystemTime::now())));
        assert!(!within_active_window(
            SystemTime::now().checked_sub(Duration::from_secs(600))
        ));
        assert!(!within_active_window(None));
    }

    #[test]
    fn probe_old_file_is_idle_without_parse() {
        // Even a Codex file with an open turn is Idle if its mtime is stale.
        let body = "{\"payload\":{\"type\":\"task_started\"}}\n";
        let path = temp_file("codex-stale", body);
        let old = SystemTime::now()
            .checked_sub(Duration::from_secs(3600))
            .unwrap();
        assert_eq!(
            probe_liveness(&path, "codex_app", Some(old)),
            Liveness::Idle
        );
    }
}
