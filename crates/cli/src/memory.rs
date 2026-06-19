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
use brick_core::{
    LocalStore, MetadataDb, SourceProfileStore, SourceSessionRecord, SourceSessionTextQuery,
};
use serde::Serialize;

use crate::args::MemoryCommand;
use crate::history::{
    build_file_session_blame_response, ensure_json, print_json, read_profile,
    refresh_profiles_to_metadata,
};

/// Refresh ceiling shared with `brick history`: index all sessions before query.
const QUERY_REFRESH_LIMIT: usize = 100_000;

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
        MemoryCommand::Query {
            query,
            source,
            limit,
            format,
        } => {
            ensure_json(format);
            print_json(&build_query_response(profiles, &query, &source, limit)?)
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

/// The free-text query payload: a summary plus matching sessions, newest first.
#[derive(Debug, Serialize, PartialEq)]
pub struct MemoryQueryResponse {
    pub schema: String,
    pub query: String,
    pub source: String,
    pub status: String,
    pub summary: String,
    pub match_count: usize,
    pub errors: Vec<String>,
    pub matches: Vec<QueryMatch>,
}

/// One session matching a free-text query.
#[derive(Debug, Serialize, PartialEq)]
pub struct QueryMatch {
    pub source_id: String,
    pub external_session_id: String,
    /// Session title / first user message — the "why".
    pub intent: Option<String>,
    pub repo_path: Option<String>,
    pub branch: Option<String>,
    pub last_seen_at: String,
    pub files_changed: Option<u64>,
    pub touched_files: Vec<String>,
    /// How to retrieve the full transcript for this session.
    pub recall_chunks_hint: String,
}

fn build_query_response(
    profiles: &SourceProfileStore,
    query: &str,
    source: &str,
    limit: usize,
) -> Result<MemoryQueryResponse> {
    let mut errors = Vec::new();
    let selected_profiles = if source == "all" {
        profiles.list_profiles()?
    } else {
        vec![read_profile(profiles, source)?]
    };

    let mut matches = Vec::new();
    let status = match MetadataDb::open_global() {
        Ok(mut metadata_db) => {
            if let Err(error) = refresh_profiles_to_metadata(
                &mut metadata_db,
                &selected_profiles,
                QUERY_REFRESH_LIMIT,
            ) {
                errors.push(format!("source_metadata_refresh: {error}"));
            }
            let query_source = (source != "all").then(|| source.to_string());
            match metadata_db.query_source_sessions_text(&SourceSessionTextQuery {
                query: query.to_string(),
                source_id: query_source,
                limit,
            }) {
                Ok(records) => {
                    matches = records.into_iter().map(query_match_from_record).collect();
                    if matches.is_empty() {
                        "empty"
                    } else {
                        "ok"
                    }
                }
                Err(error) => {
                    errors.push(format!("source_metadata_query: {error}"));
                    "error"
                }
            }
        }
        Err(error) => {
            errors.push(format!("source_metadata_open: {error}"));
            "error"
        }
    };

    let summary = summarize_query(query, &matches);
    Ok(MemoryQueryResponse {
        schema: "memory-query-v1".to_string(),
        query: query.to_string(),
        source: source.to_string(),
        status: status.to_string(),
        summary,
        match_count: matches.len(),
        errors,
        matches,
    })
}

fn query_match_from_record(record: SourceSessionRecord) -> QueryMatch {
    let touched_files = record
        .touched_files_json
        .as_ref()
        .and_then(|value| value.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.as_str().map(ToOwned::to_owned))
                .collect()
        })
        .unwrap_or_default();
    let recall_chunks_hint = format!(
        "brick history chunks --source {} --session-id {} --format json",
        record.source_id, record.external_session_id
    );
    QueryMatch {
        recall_chunks_hint,
        intent: record
            .title
            .filter(|title| !title.is_empty())
            .or_else(|| record.name.filter(|name| !name.is_empty())),
        repo_path: record.repo_path.map(|path| path.display().to_string()),
        branch: record.branch,
        last_seen_at: record.last_seen_at.to_rfc3339(),
        files_changed: record.files_changed,
        touched_files,
        source_id: record.source_id,
        external_session_id: record.external_session_id,
    }
}

/// Builds a one-line natural-language summary of the query result.
fn summarize_query(query: &str, matches: &[QueryMatch]) -> String {
    if matches.is_empty() {
        return format!("No indexed sessions match \"{}\".", truncate(query, 80));
    }
    let mut tools: Vec<&str> = matches
        .iter()
        .map(|entry| entry.source_id.as_str())
        .collect();
    tools.sort_unstable();
    tools.dedup();
    let count = matches.len();
    let (session_word, verb) = if count == 1 {
        ("session", "matches")
    } else {
        ("sessions", "match")
    };
    let latest_intent = matches
        .iter()
        .find_map(|entry| entry.intent.as_deref())
        .map(|intent| format!(" Most recent: \"{}\".", truncate(intent, 120)))
        .unwrap_or_default();
    format!(
        "{count} {session_word} {verb} \"{}\" (via {}).{latest_intent}",
        truncate(query, 80),
        tools.join(", ")
    )
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

    fn query_match(source: &str, intent: Option<&str>) -> QueryMatch {
        QueryMatch {
            source_id: source.to_string(),
            external_session_id: "sid".to_string(),
            intent: intent.map(ToOwned::to_owned),
            repo_path: Some("/repo".to_string()),
            branch: Some("main".to_string()),
            last_seen_at: "2026-06-19T00:00:00+00:00".to_string(),
            files_changed: Some(2),
            touched_files: vec!["src/lib.rs".to_string()],
            recall_chunks_hint: "brick history chunks …".to_string(),
        }
    }

    #[test]
    fn query_summary_for_no_matches() {
        let summary = summarize_query("auth refactor", &[]);
        assert_eq!(summary, "No indexed sessions match \"auth refactor\".");
    }

    #[test]
    fn query_summary_counts_and_lists_tools() {
        let matches = vec![
            query_match("codex_app", Some("Refactor auth layer")),
            query_match("orgii", None),
        ];
        let summary = summarize_query("auth", &matches);
        assert!(summary.starts_with("2 sessions match \"auth\""));
        assert!(summary.contains("codex_app, orgii"));
        assert!(summary.contains("Most recent: \"Refactor auth layer\"."));
    }

    #[test]
    fn query_summary_singular_for_one_match() {
        let matches = vec![query_match("gemini", Some("Fix bug"))];
        let summary = summarize_query("bug", &matches);
        assert!(summary.starts_with("1 session matches \"bug\" (via gemini)."));
    }
}
