use anyhow::{Context, Result};

use std::path::{Path, PathBuf};

use crate::{
    list_native_source_sessions, ActivityChunk, Liveness, NativeSourceSession,
    SourcePlanWithEdgesUpsert, SourceProfile,
};

use super::liveness::probe_liveness;
use super::{claude_code, codex_app, cursor_agent, cursor_ide, gemini, opencode, orgii, windsurf};

const SOURCE_CLAUDE_CODE: &str = "claude_code";
const SOURCE_CODEX_APP: &str = "codex_app";
const SOURCE_CURSOR_IDE: &str = "cursor_ide";
const SOURCE_CURSOR_AGENT: &str = "cursor_agent";
const SOURCE_OPENCODE: &str = "opencode";
const SOURCE_WINDSURF: &str = "windsurf";
const SOURCE_ORGII: &str = "orgii";
const SOURCE_GEMINI: &str = "gemini";

/// The set of first-class AI-tool sources Brick has a dedicated parser for.
///
/// This enum is the single registry of known sources. Every per-source dispatch
/// (`list_source_sessions_since`, `format_source_session_chunks`,
/// `list_source_plans`, liveness) matches on it **exhaustively** — so adding a
/// new tool is a compile error at every site that must handle it, instead of a
/// silent fall-through to the generic parser / empty chunks / wrong liveness.
///
/// A source string that doesn't map to a variant is an *unknown* source and is
/// handled by the explicit `None` arm of `KnownSource::from_name` (the generic
/// file provider), which is a supported feature — custom user sources.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KnownSource {
    ClaudeCode,
    CodexApp,
    CursorIde,
    CursorAgent,
    OpenCode,
    Windsurf,
    Orgii,
    Gemini,
}

impl KnownSource {
    /// Maps a source/profile name to its variant, or `None` for an unknown
    /// (custom) source that should use the generic file provider.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            SOURCE_CLAUDE_CODE => Some(Self::ClaudeCode),
            SOURCE_CODEX_APP => Some(Self::CodexApp),
            SOURCE_CURSOR_IDE => Some(Self::CursorIde),
            SOURCE_CURSOR_AGENT => Some(Self::CursorAgent),
            SOURCE_OPENCODE => Some(Self::OpenCode),
            SOURCE_WINDSURF => Some(Self::Windsurf),
            SOURCE_ORGII => Some(Self::Orgii),
            SOURCE_GEMINI => Some(Self::Gemini),
            _ => None,
        }
    }
}

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
    let mut sessions = match KnownSource::from_name(profile.name.as_str()) {
        Some(KnownSource::ClaudeCode) => claude_code::list_sessions(profile, limit, since),
        Some(KnownSource::CodexApp) => codex_app::list_sessions(profile, limit, since),
        Some(KnownSource::CursorIde) => cursor_ide::list_sessions(profile, limit, since),
        Some(KnownSource::CursorAgent) => cursor_agent::list_sessions(profile, limit, since),
        Some(KnownSource::OpenCode) => opencode::list_sessions(profile, limit, since),
        Some(KnownSource::Windsurf) => windsurf::list_sessions(profile, limit, since),
        Some(KnownSource::Orgii) => orgii::list_sessions(profile, limit, since),
        Some(KnownSource::Gemini) => gemini::list_sessions(profile, limit, since),
        None => list_native_source_sessions(profile, limit),
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
///
/// Only Cursor IDE exposes a plan store today; every other known source has no
/// plan concept and returns empty. The match is exhaustive so a newly-added
/// source must explicitly declare whether it has plans.
pub fn list_source_plans(profile: &SourceProfile) -> Result<Vec<SourcePlanWithEdgesUpsert>> {
    match KnownSource::from_name(profile.name.as_str()) {
        Some(KnownSource::CursorIde) => cursor_ide::list_plans(profile),
        Some(
            KnownSource::ClaudeCode
            | KnownSource::CodexApp
            | KnownSource::CursorAgent
            | KnownSource::OpenCode
            | KnownSource::Windsurf
            | KnownSource::Orgii
            | KnownSource::Gemini,
        )
        | None => Ok(Vec::new()),
    }
}

pub fn fused_source_session_chunks(
    source_id: &str,
    external_session_id: &str,
) -> Result<Vec<ActivityChunk>> {
    let db = crate::MetadataDb::open_global()?;
    let rows = db.list_source_session_chunks(source_id, external_session_id)?;
    rows.into_iter()
        .map(|row| {
            serde_json::from_value(row.raw_json)
                .map_err(anyhow::Error::from)
                .with_context(|| {
                    format!(
                        "failed to decode fused source-session chunk {}:{}:{}",
                        row.source_id, row.external_session_id, row.chunk_id
                    )
                })
        })
        .collect()
}

/// Formats source records as activity chunk JSON for one source session when supported.
pub fn format_source_session_chunks(
    source_id: &str,
    external_session_id: &str,
    source_path: Option<&Path>,
) -> Result<Vec<ActivityChunk>> {
    match KnownSource::from_name(source_id) {
        Some(KnownSource::ClaudeCode) => {
            claude_code::format_chunks(external_session_id, source_path)
        }
        Some(KnownSource::CodexApp) => codex_app::format_chunks(external_session_id, source_path),
        Some(KnownSource::CursorIde) => cursor_ide::format_chunks(external_session_id, source_path),
        Some(KnownSource::CursorAgent) => {
            cursor_agent::format_chunks(external_session_id, source_path)
        }
        Some(KnownSource::OpenCode) => opencode::format_chunks(external_session_id, source_path),
        Some(KnownSource::Windsurf) => windsurf::format_chunks(external_session_id, source_path),
        Some(KnownSource::Orgii) => orgii::format_chunks(external_session_id, source_path),
        Some(KnownSource::Gemini) => gemini::format_chunks(external_session_id, source_path),
        None => Ok(Vec::new()),
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
    let chunks = match fused_source_session_chunks(source_id, external_session_id) {
        Ok(chunks) if !chunks.is_empty() => chunks,
        _ => format_source_session_chunks(source_id, external_session_id, source_path)?,
    };
    Ok(select_turn_final_message(&chunks, occurred_at))
}

/// I/O wrapper over [`infer_turn_rationale`]: loads the session transcript and
/// infers the turn-final note plus any cause references. Mirrors
/// [`turn_final_assistant_message`] but returns the richer [`InferredRationale`].
pub fn infer_session_rationale(
    source_id: &str,
    external_session_id: &str,
    source_path: Option<&Path>,
    occurred_at: &str,
) -> Result<InferredRationale> {
    let chunks = match fused_source_session_chunks(source_id, external_session_id) {
        Ok(chunks) if !chunks.is_empty() => chunks,
        _ => format_source_session_chunks(source_id, external_session_id, source_path)?,
    };
    Ok(infer_turn_rationale(&chunks, occurred_at))
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

/// A weak (read-side, `observed`-confidence) rationale inferred from a session
/// transcript: the turn-final note plus any cause references the assistant
/// mentioned. Unlike an explicit `link` edge, every field here is best-effort
/// and may be empty.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InferredRationale {
    /// The turn-final assistant message — the WHY the diff alone can't express.
    pub note: Option<String>,
    /// Entity/cause references parsed out of that turn's assistant text:
    /// `mission_…` / `artifact_…` ids, and the kind of relation the phrasing
    /// implies (e.g. "supersedes" / "because"). Best-effort, never fabricated.
    pub cause_refs: Vec<CauseRef>,
}

/// One cause reference scraped from transcript text. `target` is the raw id
/// (`mission_…`, `artifact_…`) the assistant named; `relation` is the causal
/// relation the surrounding phrasing implies. The caller is responsible for
/// resolving `target` to a real event (and dropping it if it can't) — this
/// function only surfaces candidates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CauseRef {
    pub target: String,
    pub relation: InferredRelation,
}

/// The causal relation a transcript phrase implies, inferred from prose. The
/// `cli` layer maps these onto its wire strings (`derived_from` / `supersedes` /
/// `triggered_by`) when enriching an `explain` step's observed rationale.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InferredRelation {
    DerivedFrom,
    Supersedes,
    TriggeredBy,
}

/// Infers a weak rationale for the turn containing `occurred_at`: the turn-final
/// assistant message (same selection as [`select_turn_final_message`]) plus any
/// `mission_…` / `artifact_…` references found in that turn's assistant text,
/// each tagged with the relation its phrasing implies.
///
/// Pure (no I/O) so it unit-tests without a filesystem. Honesty rule: this only
/// ever produces `observed`-confidence signals — it surfaces id candidates and
/// implied relations, but never invents a target that isn't named verbatim.
pub fn infer_turn_rationale(chunks: &[ActivityChunk], occurred_at: &str) -> InferredRationale {
    let note = select_turn_final_message(chunks, occurred_at);
    let cause_refs = note.as_deref().map(extract_cause_refs).unwrap_or_default();
    InferredRationale { note, cause_refs }
}

/// Scrapes `mission_…` / `artifact_…` ids out of free text and tags each with
/// the relation implied by nearby wording. Deduplicates by target id (first
/// relation wins). Conservative by design: an id with no relation cue defaults
/// to `DerivedFrom` (the weakest "this came from that"), and prose with a
/// relation cue but no id is ignored (nothing to bind to).
fn extract_cause_refs(text: &str) -> Vec<CauseRef> {
    let mut refs: Vec<CauseRef> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    // Split into whitespace/punctuation-delimited words; an entity id is a single
    // such token (`mission_<id>` / `artifact_<id>`). Track a char offset so we can
    // sample a relation cue from the surrounding text on a char boundary.
    let lower = text.to_lowercase();
    let mut offset = 0usize;
    for word in
        text.split(|c: char| c.is_whitespace() || matches!(c, ',' | ';' | '(' | ')' | '`' | '"'))
    {
        let word_start = match text[offset..].find(word) {
            Some(rel) if !word.is_empty() => offset + rel,
            _ => offset,
        };
        offset = word_start + word.len();
        let trimmed =
            word.trim_matches(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '-'));
        let Some(target) = parse_entity_id(trimmed) else {
            continue;
        };
        if !seen.insert(target.clone()) {
            continue;
        }
        // Sample a window of lowercased prose around the id for a relation cue,
        // snapping bounds to char boundaries.
        let win_lo = floor_char_boundary(&lower, word_start.saturating_sub(60));
        let win_hi = ceil_char_boundary(&lower, (offset + 60).min(lower.len()));
        let relation = relation_from_cue(&lower[win_lo..win_hi]);
        refs.push(CauseRef { target, relation });
    }
    refs
}

fn floor_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx > 0 && !s.is_char_boundary(idx) {
        idx -= 1;
    }
    idx
}

fn ceil_char_boundary(s: &str, mut idx: usize) -> usize {
    if idx >= s.len() {
        return s.len();
    }
    while idx < s.len() && !s.is_char_boundary(idx) {
        idx += 1;
    }
    idx
}

/// Parses a leading `mission_<id>` / `artifact_<id>` token from `text`. The id
/// body is `[A-Za-z0-9_-]+`. Returns the matched token (prefix included), or
/// `None` if `text` doesn't start with a recognized entity prefix.
fn parse_entity_id(text: &str) -> Option<String> {
    const PREFIXES: [&str; 2] = ["mission_", "artifact_"];
    let lower = text.to_lowercase();
    let prefix = PREFIXES.into_iter().find(|p| lower.starts_with(p))?;
    let body: String = text[prefix.len()..]
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect();
    if body.is_empty() {
        return None;
    }
    Some(format!("{prefix}{body}"))
}

/// Maps a window of lowercased prose to the relation it implies. Order matters:
/// the more specific / directional cues are checked first.
fn relation_from_cue(window: &str) -> InferredRelation {
    if window.contains("supersede")
        || window.contains("replaces")
        || window.contains("replaced")
        || window.contains("instead of")
    {
        InferredRelation::Supersedes
    } else if window.contains("triggered by")
        || window.contains("in response to")
        || window.contains("responds to")
    {
        InferredRelation::TriggeredBy
    } else {
        InferredRelation::DerivedFrom
    }
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
        let chunks = vec![user_message_chunk(
            "s",
            "p",
            0,
            "2026-06-20T10:00:00Z",
            "go",
        )];
        assert_eq!(
            select_turn_final_message(&chunks, "2026-06-20T10:05:00Z"),
            None
        );
    }

    #[test]
    fn infer_rationale_extracts_note_and_mission_cause() {
        let chunks = vec![
            user_message_chunk("s", "p", 0, "2026-06-20T10:00:00Z", "do it"),
            assistant_message_chunk(
                "s",
                "p",
                1,
                "2026-06-20T10:01:00Z",
                "Hardened the token refresh, derived from mission_abc123 which set the goal.",
            ),
        ];
        let got = infer_turn_rationale(&chunks, "2026-06-20T10:02:00Z");
        assert!(got.note.as_deref().unwrap().contains("Hardened"));
        assert_eq!(got.cause_refs.len(), 1);
        assert_eq!(got.cause_refs[0].target, "mission_abc123");
        assert_eq!(got.cause_refs[0].relation, InferredRelation::DerivedFrom);
    }

    #[test]
    fn infer_rationale_detects_supersedes_cue() {
        let chunks = vec![
            user_message_chunk("s", "p", 0, "2026-06-20T10:00:00Z", "go"),
            assistant_message_chunk(
                "s",
                "p",
                1,
                "2026-06-20T10:01:00Z",
                "This replaces artifact_old99, the earlier approach was wrong.",
            ),
        ];
        let got = infer_turn_rationale(&chunks, "2026-06-20T10:02:00Z");
        assert_eq!(got.cause_refs.len(), 1);
        assert_eq!(got.cause_refs[0].target, "artifact_old99");
        assert_eq!(got.cause_refs[0].relation, InferredRelation::Supersedes);
    }

    #[test]
    fn extract_cause_refs_ignores_prose_without_ids_and_dedupes() {
        // A relation cue with no entity id binds to nothing.
        assert!(extract_cause_refs("this supersedes the old behavior entirely").is_empty());
        // Repeated id is surfaced once.
        let refs = extract_cause_refs("mission_x drove this; see mission_x again");
        assert_eq!(refs.len(), 1);
        assert_eq!(refs[0].target, "mission_x");
    }
}
