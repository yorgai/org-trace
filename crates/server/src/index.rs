//! Rebuildable server-side query projection for HTTP routes.
//!
//! The server event JSONL log remains authoritative. This module rebuilds a thin
//! in-memory index from stored events for MVP status and session queries, so it
//! can be replaced by a persisted projection later without changing route
//! boundaries.

use anyhow::Result;
use brick_core::{query_indexed_sessions, IndexedSession, SessionQuery, TraceIndex};
use brick_protocol::TraceEvent;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Summary of a rebuilt server query index.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerIndexStatus {
    pub repo_id: Option<String>,
    pub event_count: usize,
    pub mission_count: usize,
    pub session_count: usize,
    pub artifact_count: usize,
    pub file_count: usize,
    pub session_log_count: usize,
    pub diff_count: usize,
    pub rebuilt_at: DateTime<Utc>,
}

/// Query parameters accepted by server session routes.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct ServerSessionQuery {
    pub app_id: Option<String>,
    pub app_session_id: Option<String>,
    pub app_session_name: Option<String>,
    pub runtime_id: Option<String>,
    pub actor_id: Option<String>,
    pub limit: Option<usize>,
}

/// Response returned by server session query routes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ServerSessionsResponse {
    pub repo_id: Option<String>,
    pub sessions: Vec<IndexedSession>,
}

/// Rebuilds the derived server index from the provided event stream.
pub fn rebuild_server_index(repo_id: Option<&str>, events: &[TraceEvent]) -> Result<TraceIndex> {
    let scoped_events = events
        .iter()
        .filter(|event| repo_matches(event.repo_id.as_deref(), repo_id))
        .cloned()
        .collect::<Vec<_>>();
    TraceIndex::build(&scoped_events)
}

/// Summarizes a rebuilt server index.
pub fn server_index_status(repo_id: Option<&str>, index: &TraceIndex) -> ServerIndexStatus {
    ServerIndexStatus {
        repo_id: repo_id.map(ToString::to_string),
        event_count: index.event_count,
        mission_count: index.missions.len(),
        session_count: index.sessions.len(),
        artifact_count: index.artifacts.len(),
        file_count: index.files.len(),
        session_log_count: index.session_logs.len(),
        diff_count: index.diffs.len(),
        rebuilt_at: index.rebuilt_at,
    }
}

/// Runs a typed session query against a rebuilt server index.
pub fn query_server_sessions(
    repo_id: Option<&str>,
    index: &TraceIndex,
    query: &ServerSessionQuery,
) -> ServerSessionsResponse {
    let session_query = SessionQuery {
        app_id: query.app_id.clone(),
        app_session_id: query.app_session_id.clone(),
        app_session_name: query.app_session_name.clone(),
        runtime_id: query.runtime_id.clone(),
        actor_id: query.actor_id.clone(),
    };
    let limit = query.limit.unwrap_or(20).clamp(1, 1000);
    let sessions = query_indexed_sessions(index, &session_query)
        .into_iter()
        .take(limit)
        .cloned()
        .collect();

    ServerSessionsResponse {
        repo_id: repo_id.map(ToString::to_string),
        sessions,
    }
}

fn repo_matches(event_repo_id: Option<&str>, filter_repo_id: Option<&str>) -> bool {
    match filter_repo_id {
        Some(repo_id) => event_repo_id == Some(repo_id),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use brick_protocol::{ActorRef, ActorType, SessionId, SessionSource, SessionStartedPayload};

    use super::*;

    fn session_event(repo_id: &str, app_id: &str) -> TraceEvent {
        let mut event = TraceEvent::session_started(
            ActorRef {
                actor_type: ActorType::Agent,
                actor_id: "agent-1".to_string(),
                display_name: None,
            },
            SessionId::new(),
            None,
            SessionStartedPayload {
                session_name: Some("server session".to_string()),
                source: SessionSource {
                    app_id: Some(app_id.to_string()),
                    app_session_id: None,
                    app_session_name: None,
                    runtime_id: None,
                },
                repo_context_id: None,
            },
        )
        .expect("session event");
        event.repo_id = Some(repo_id.to_string());
        event
    }

    #[test]
    fn server_index_scopes_events_by_repo() {
        let events = vec![
            session_event("repo-a", "cursor"),
            session_event("repo-b", "codex"),
        ];
        let index = rebuild_server_index(Some("repo-a"), &events).expect("build index");
        let status = server_index_status(Some("repo-a"), &index);
        let sessions = query_server_sessions(
            Some("repo-a"),
            &index,
            &ServerSessionQuery {
                app_id: Some("cursor".to_string()),
                ..ServerSessionQuery::default()
            },
        );

        assert_eq!(status.event_count, 1);
        assert_eq!(status.session_count, 1);
        assert_eq!(sessions.sessions.len(), 1);
    }
}
