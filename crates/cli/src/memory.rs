//! `brick memory recall` — agent-facing recall over indexed history.
//!
//! Where `brick history file-session-blame` returns raw attribution rows, this
//! command aggregates them into a compact, human/agent-readable summary of *who*
//! changed a file across past sessions and *why* — one entry per prior session,
//! enriched with the session title/intent and change size, newest first. It is
//! the single call the agent-awareness block points at so an agent can recall
//! prior decisions before editing a file, instead of stitching together three
//! `brick history` calls.

use anyhow::Result;
use brick_core::{LocalStore, MetadataDb, SourceProfileStore};
use serde::Serialize;

use crate::args::MemoryCommand;
use crate::history::{build_file_session_blame_response, ensure_json, print_json};

/// Entry point for `brick memory <subcommand>`.
pub fn handle_memory(
    command: MemoryCommand,
    profiles: &SourceProfileStore,
    store: &LocalStore,
) -> Result<()> {
    match command {
        MemoryCommand::Recall {
            path,
            source,
            limit,
            format,
        } => {
            ensure_json(format);
            print_json(&build_recall_response(
                store, profiles, &path, &source, limit,
            )?)
        }
    }
}

/// The aggregated recall payload: a short summary plus per-session entries.
#[derive(Debug, Serialize, PartialEq)]
pub struct MemoryRecallResponse {
    pub schema: String,
    pub file_path: String,
    pub source: String,
    pub status: String,
    /// One-line natural-language summary suitable for direct agent consumption.
    pub summary: String,
    pub session_count: usize,
    pub errors: Vec<String>,
    pub sessions: Vec<RecallSession>,
}

/// One prior session that touched the file, with intent and change size.
#[derive(Debug, Serialize, PartialEq)]
pub struct RecallSession {
    pub source_id: Option<String>,
    pub external_session_id: Option<String>,
    /// Session title / first user message — the "why".
    pub intent: Option<String>,
    pub last_seen_at: String,
    pub files_changed: Option<u64>,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub confidence: Option<String>,
    /// How to retrieve the full transcript for this session.
    pub recall_chunks_hint: Option<String>,
}

fn build_recall_response(
    store: &LocalStore,
    profiles: &SourceProfileStore,
    file_path: &str,
    source: &str,
    limit: usize,
) -> Result<MemoryRecallResponse> {
    let blame = build_file_session_blame_response(store, profiles, file_path, source, limit)?;

    // Enrich each blame row with the session title from the metadata DB so the
    // agent sees the *intent* behind each prior change, not just an ID.
    let metadata_db = MetadataDb::open_global().ok();
    let mut sessions = Vec::new();
    for row in &blame.rows {
        let intent = lookup_intent(
            metadata_db.as_ref(),
            row.source_id.as_deref(),
            row.external_session_id.as_deref(),
        );
        let recall_chunks_hint =
            match (row.source_id.as_deref(), row.external_session_id.as_deref()) {
                (Some(source_id), Some(session_id)) => Some(format!(
                "brick history chunks --source {source_id} --session-id {session_id} --format json"
            )),
                _ => None,
            };
        sessions.push(RecallSession {
            source_id: row.source_id.clone(),
            external_session_id: row.external_session_id.clone(),
            intent,
            last_seen_at: row.last_seen_at.clone(),
            files_changed: row.files_changed,
            lines_added: row.lines_added,
            lines_removed: row.lines_removed,
            confidence: row.confidence.clone(),
            recall_chunks_hint,
        });
    }

    let summary = summarize(file_path, &sessions);
    Ok(MemoryRecallResponse {
        schema: "memory-recall-v1".to_string(),
        file_path: file_path.to_string(),
        source: source.to_string(),
        status: blame.status,
        summary,
        session_count: sessions.len(),
        errors: blame.errors,
        sessions,
    })
}

/// Looks up a session's title/name as its recall "intent".
fn lookup_intent(
    metadata_db: Option<&MetadataDb>,
    source_id: Option<&str>,
    external_session_id: Option<&str>,
) -> Option<String> {
    let metadata_db = metadata_db?;
    let source_id = source_id?;
    let external_session_id = external_session_id?;
    let record = metadata_db
        .get_source_session(source_id, external_session_id)
        .ok()??;
    record
        .title
        .filter(|title| !title.is_empty())
        .or_else(|| record.name.filter(|name| !name.is_empty()))
}

/// Builds a one-line natural-language summary of the recall result.
fn summarize(file_path: &str, sessions: &[RecallSession]) -> String {
    let name = file_path.rsplit('/').next().unwrap_or(file_path);
    if sessions.is_empty() {
        return format!("No prior indexed sessions touched {name}.");
    }
    let mut tools: Vec<&str> = sessions
        .iter()
        .filter_map(|session| session.source_id.as_deref())
        .collect();
    tools.sort_unstable();
    tools.dedup();
    let tools_label = if tools.is_empty() {
        "unknown tools".to_string()
    } else {
        tools.join(", ")
    };
    let count = sessions.len();
    let session_word = if count == 1 { "session" } else { "sessions" };
    let latest_intent = sessions
        .iter()
        .find_map(|session| session.intent.as_deref())
        .map(|intent| format!(" Most recent: \"{}\".", truncate(intent, 120)))
        .unwrap_or_default();
    format!("{count} prior {session_word} touched {name} (via {tools_label}).{latest_intent}")
}

fn truncate(value: &str, max: usize) -> String {
    if value.chars().count() <= max {
        return value.to_string();
    }
    let mut out: String = value.chars().take(max).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(source: &str, intent: Option<&str>) -> RecallSession {
        RecallSession {
            source_id: Some(source.to_string()),
            external_session_id: Some("sid".to_string()),
            intent: intent.map(ToOwned::to_owned),
            last_seen_at: "2026-06-19T00:00:00Z".to_string(),
            files_changed: Some(2),
            lines_added: Some(10),
            lines_removed: Some(1),
            confidence: Some("metadata_only".to_string()),
            recall_chunks_hint: None,
        }
    }

    #[test]
    fn summary_for_no_sessions() {
        let summary = summarize("/repo/src/lib.rs", &[]);
        assert_eq!(summary, "No prior indexed sessions touched lib.rs.");
    }

    #[test]
    fn summary_counts_sessions_and_lists_tools() {
        let sessions = vec![
            session("codex_app", Some("Add CSV export")),
            session("claude_code", None),
        ];
        let summary = summarize("/repo/data.csv", &sessions);
        assert!(summary.starts_with("2 prior sessions touched data.csv"));
        assert!(summary.contains("claude_code, codex_app"));
        assert!(summary.contains("Most recent: \"Add CSV export\"."));
    }

    #[test]
    fn summary_singular_for_one_session() {
        let sessions = vec![session("gemini", Some("Fix bug"))];
        let summary = summarize("/x/y.py", &sessions);
        assert!(summary.starts_with("1 prior session touched y.py (via gemini)."));
    }

    #[test]
    fn truncate_caps_long_intent() {
        let long = "x".repeat(200);
        let out = truncate(&long, 10);
        assert_eq!(out.chars().count(), 11); // 10 + ellipsis
        assert!(out.ends_with('…'));
    }
}
