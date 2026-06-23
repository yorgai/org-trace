use anyhow::Result;

use std::path::{Path, PathBuf};

use crate::{
    list_native_source_sessions, ActivityChunk, Liveness, NativeSourceSession,
    SourcePlanWithEdgesUpsert, SourceProfile,
};

use super::liveness::probe_liveness;
use super::{
    claude_code, codex_app, cursor_agent, cursor_ide, gemini, opencode, orgii, windsurf,
};

const SOURCE_CLAUDE_CODE: &str = "claude_code";
const SOURCE_CODEX_APP: &str = "codex_app";
const SOURCE_CURSOR_IDE: &str = "cursor_ide";
const SOURCE_CURSOR_AGENT: &str = "cursor_agent";
const SOURCE_OPENCODE: &str = "opencode";
const SOURCE_WINDSURF: &str = "windsurf";
const SOURCE_ORGII: &str = "orgii";
const SOURCE_GEMINI: &str = "gemini";

/// Lists native sessions through the app-specific provider for a source profile.
///
/// This is the single funnel through which every provider's results pass, so it
/// is also where transient [`Liveness`] is computed (never inside providers,
/// never persisted) — see [`super::liveness`].
pub fn list_source_sessions(
    profile: &SourceProfile,
    limit: Option<usize>,
) -> Result<Vec<NativeSourceSession>> {
    list_source_sessions_since(profile, limit, None)
}

/// Like [`list_source_sessions`] but with an optional `since` watermark (RFC3339)
/// for incremental refresh. Only ORGII (the multi-GB SQLite source where a full
/// re-scan is expensive) honors `since` today; every other provider ignores it
/// and returns its newest `limit` sessions, relying on the refresh layer's
/// per-session fingerprint skip to avoid redundant upserts.
pub fn list_source_sessions_since(
    profile: &SourceProfile,
    limit: Option<usize>,
    since: Option<&str>,
) -> Result<Vec<NativeSourceSession>> {
    let mut sessions = match profile.name.as_str() {
        SOURCE_CLAUDE_CODE => claude_code::list_sessions(profile, limit, since),
        SOURCE_CODEX_APP => codex_app::list_sessions(profile, limit, since),
        SOURCE_CURSOR_IDE => cursor_ide::list_sessions(profile, limit, since),
        SOURCE_CURSOR_AGENT => cursor_agent::list_sessions(profile, limit, since),
        SOURCE_OPENCODE => opencode::list_sessions(profile, limit, since),
        SOURCE_WINDSURF => windsurf::list_sessions(profile, limit, since),
        SOURCE_ORGII => orgii::list_sessions(profile, limit, since),
        SOURCE_GEMINI => gemini::list_sessions(profile, limit, since),
        _ => list_native_source_sessions(profile, limit),
    }?;
    fill_liveness(&mut sessions);
    Ok(sessions)
}

/// Fills the transient `liveness` and `last_activity` of each session in place.
fn fill_liveness(sessions: &mut [NativeSourceSession]) {
    for session in sessions.iter_mut() {
        // Gate on the session's own activity time, not the (possibly shared)
        // file mtime — SQLite sources keep many sessions in one `.db`.
        let activity = session.session_updated_at.or(session.modified_at);
        session.liveness = probe_liveness(&session.path, &session.source_app_id, activity);
        session.last_activity = activity;
    }
}

/// Resolves a session's "work scope" — the directory other sessions are judged
/// to overlap with. Priority: git repo root → recorded cwd → none. A scope that
/// is the user's home directory or a very shallow path is rejected (returns
/// `None`) so "everything under `~`" never counts as one shared scope; such
/// sessions can still match at file granularity.
pub fn work_scope(session: &NativeSourceSession) -> Option<PathBuf> {
    let candidate = session.repo_path.clone().or_else(|| session.cwd.clone())?;
    if is_too_shallow(&candidate) {
        return None;
    }
    Some(candidate)
}

/// True for paths too broad to be a meaningful shared scope: the home dir, root,
/// or any path with fewer than two components below root (e.g. `/Users`).
fn is_too_shallow(path: &Path) -> bool {
    if let Some(home) = dirs_home() {
        if path == home {
            return true;
        }
    }
    // Count non-root components; `/`, `/Users`, `C:\` are too shallow.
    let depth = path
        .components()
        .filter(|component| {
            !matches!(
                component,
                std::path::Component::RootDir | std::path::Component::Prefix(_)
            )
        })
        .count();
    depth < 2
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Marks whether a session is currently active (convenience for callers).
pub fn is_active(session: &NativeSourceSession) -> bool {
    session.liveness == Liveness::Active
}

/// Lists source plans and recovered plan-session edges through supported providers.
pub fn list_source_plans(profile: &SourceProfile) -> Result<Vec<SourcePlanWithEdgesUpsert>> {
    match profile.name.as_str() {
        SOURCE_CURSOR_IDE => cursor_ide::list_plans(profile),
        _ => Ok(Vec::new()),
    }
}

/// Formats source records as activity chunk JSON for one source session when supported.
pub fn format_source_session_chunks(
    source_id: &str,
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    match source_id {
        SOURCE_CLAUDE_CODE => claude_code::format_chunks(external_session_id, source_path),
        SOURCE_CODEX_APP => codex_app::format_chunks(external_session_id, source_path),
        SOURCE_CURSOR_IDE => cursor_ide::format_chunks(external_session_id, source_path),
        SOURCE_CURSOR_AGENT => cursor_agent::format_chunks(external_session_id, source_path),
        SOURCE_OPENCODE => opencode::format_chunks(external_session_id, source_path),
        SOURCE_WINDSURF => windsurf::format_chunks(external_session_id, source_path),
        SOURCE_ORGII => orgii::format_chunks(external_session_id, source_path),
        SOURCE_GEMINI => gemini::format_chunks(external_session_id, source_path),
        _ => Ok(Vec::new()),
    }
}

/// The final assistant message of the conversational turn that a change at
/// `occurred_at` belongs to — an `observed` rationale recovered from the original
/// transcript when no agent ever asserted one via `link`.
///
/// A "turn" runs from one user message to the next; an agent narrates what it did
/// (and often *why*) in the last assistant message before the next user prompt.
/// Real-data analysis (Codex 99% / Claude 100% of edit-turns) showed this message
/// is almost always present and frequently carries the constraint / blocker /
/// decision that the diff alone cannot express.
///
/// Selection: take the assistant messages in the turn whose bounds contain
/// `occurred_at` (greatest user-message timestamp `<= occurred_at`, up to the next
/// user message), and return the last one. If timestamps don't line up with any
/// turn, fall back to the last assistant message at or before `occurred_at`, then
/// to the last assistant message overall — a best-effort guess, never fabricated.
pub fn turn_final_assistant_message(
    source_id: &str,
    external_session_id: &str,
    source_path: Option<&Path>,
    occurred_at: &str,
) -> Result<Option<String>> {
    let chunks = format_source_session_chunks(source_id, external_session_id, source_path)?;
    Ok(select_turn_final_message(&chunks, occurred_at))
}

/// Pure turn-selection over ordered chunks, split out for unit testing without
/// touching the filesystem. See [`turn_final_assistant_message`] for semantics.
pub fn select_turn_final_message(chunks: &[ActivityChunk], occurred_at: &str) -> Option<String> {
    fn assistant_text(chunk: &ActivityChunk) -> Option<String> {
        if chunk.action_type != crate::ACTION_TYPE_ASSISTANT {
            return None;
        }
        chunk
            .result
            .get("content")
            .and_then(|value| value.as_str())
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(ToOwned::to_owned)
    }
    fn is_user(chunk: &ActivityChunk) -> bool {
        chunk.function == crate::FUNCTION_USER_MESSAGE
    }

    // Turn bounds: the last user message at or before `occurred_at` opens the
    // turn; the next user message after it closes the turn.
    let turn_start = chunks
        .iter()
        .enumerate()
        .filter(|(_, chunk)| is_user(chunk) && chunk.created_at.as_str() <= occurred_at)
        .map(|(index, _)| index)
        .next_back();

    if let Some(start) = turn_start {
        let turn_end = chunks
            .iter()
            .enumerate()
            .skip(start + 1)
            .find(|(_, chunk)| is_user(chunk))
            .map(|(index, _)| index)
            .unwrap_or(chunks.len());
        let final_in_turn = chunks[start..turn_end]
            .iter()
            .filter_map(assistant_text)
            .next_back();
        if final_in_turn.is_some() {
            return final_in_turn;
        }
    }

    // Fallbacks: last assistant at/before the timestamp, then last overall.
    chunks
        .iter()
        .filter(|chunk| chunk.created_at.as_str() <= occurred_at)
        .filter_map(assistant_text)
        .next_back()
        .or_else(|| chunks.iter().filter_map(assistant_text).next_back())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use brick_protocol::ActorType;

    use super::*;
    use crate::{assistant_message_chunk, user_message_chunk, GENERIC_NATIVE_FILE_PARSER_VERSION};

    fn temp_source_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-source-dispatch-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create source dispatch root");
        path
    }

    fn profile(name: &str, session_log_path: PathBuf) -> SourceProfile {
        SourceProfile {
            name: name.to_string(),
            app_id: Some(name.to_string()),
            actor_id: None,
            actor_type: Some(ActorType::Agent),
            store_root: None,
            session_db_path: None,
            session_log_path: Some(session_log_path),
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        }
    }

    #[test]
    fn dispatches_claude_code_to_app_specific_provider() {
        let root = temp_source_root("claude");
        fs::write(
            root.join("session-1.jsonl"),
            "{\"type\":\"assistant\",\"timestamp\":\"2026-06-18T01:00:00Z\",\"message\":{\"model\":\"claude-sonnet\",\"usage\":{\"input_tokens\":11,\"output_tokens\":7}}}\n",
        )
        .expect("write claude fixture");
        let source_profile = profile(SOURCE_CLAUDE_CODE, root);

        let sessions =
            list_source_sessions(&source_profile, Some(10)).expect("list claude sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].parser_version, "claude-code-jsonl-v4");
        assert_eq!(sessions[0].model.as_deref(), Some("claude-sonnet"));
        assert_eq!(sessions[0].input_tokens, Some(11));
        assert_eq!(sessions[0].output_tokens, Some(7));
    }

    #[test]
    fn unknown_sources_use_generic_file_provider() {
        let root = temp_source_root("generic");
        fs::write(root.join("session-1.jsonl"), "{\"message\":\"hello\"}\n")
            .expect("write generic fixture");
        let source_profile = profile("custom_source", root);

        let sessions =
            list_source_sessions(&source_profile, Some(10)).expect("list generic sessions");

        assert_eq!(sessions.len(), 1);
        assert_eq!(
            sessions[0].parser_version,
            GENERIC_NATIVE_FILE_PARSER_VERSION
        );
        assert_eq!(sessions[0].source_app_id, "custom_source");
    }

    #[test]
    fn turn_final_message_picks_last_assistant_in_the_owning_turn() {
        // Two turns. A change at 10:05 belongs to turn 1; its closing narration
        // (10:04) is the rationale, NOT turn 2's message.
        let chunks = vec![
            user_message_chunk("s", "p", 0, "2026-06-20T10:00:00Z", "do the cache fix"),
            assistant_message_chunk("s", "p", 1, "2026-06-20T10:02:00Z", "looking at it"),
            assistant_message_chunk(
                "s",
                "p",
                2,
                "2026-06-20T10:04:00Z",
                "Added TTL because reads were stale; chose Instant over SystemTime.",
            ),
            user_message_chunk("s", "p", 3, "2026-06-20T10:10:00Z", "now do auth"),
            assistant_message_chunk("s", "p", 4, "2026-06-20T10:12:00Z", "auth done"),
        ];
        let got = select_turn_final_message(&chunks, "2026-06-20T10:05:00Z");
        assert_eq!(
            got.as_deref(),
            Some("Added TTL because reads were stale; chose Instant over SystemTime.")
        );
    }

    #[test]
    fn turn_final_message_falls_back_to_last_before_timestamp_without_user_bounds() {
        // No user messages at all (degenerate transcript) → last assistant at or
        // before the timestamp.
        let chunks = vec![
            assistant_message_chunk("s", "p", 0, "2026-06-20T09:00:00Z", "early"),
            assistant_message_chunk("s", "p", 1, "2026-06-20T09:30:00Z", "the one"),
            assistant_message_chunk("s", "p", 2, "2026-06-20T11:00:00Z", "too late"),
        ];
        let got = select_turn_final_message(&chunks, "2026-06-20T10:00:00Z");
        assert_eq!(got.as_deref(), Some("the one"));
    }

    #[test]
    fn turn_final_message_none_when_turn_has_no_assistant_text() {
        // A turn with only a user message and tool noise yields nothing rather
        // than fabricating a rationale.
        let chunks = vec![user_message_chunk("s", "p", 0, "2026-06-20T10:00:00Z", "go")];
        assert_eq!(select_turn_final_message(&chunks, "2026-06-20T10:05:00Z"), None);
    }
}
