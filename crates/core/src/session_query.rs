//! Read-only query helpers for indexed Brick sessions.
//!
//! These helpers operate only on the rebuildable JSON index. The durable JSONL
//! queue remains the source of truth and callers decide when to load or rebuild
//! the index before querying.

use crate::{IndexedSession, TraceIndex};

/// Optional filters for discovering sessions from the local JSON index.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionQuery {
    pub app_id: Option<String>,
    pub app_session_id: Option<String>,
    pub app_session_name: Option<String>,
    pub runtime_id: Option<String>,
    pub actor_id: Option<String>,
}

impl SessionQuery {
    /// Returns true when every configured filter matches the indexed session.
    pub fn matches(&self, session: &IndexedSession) -> bool {
        matches_optional(&self.app_id, session.source.app_id.as_deref())
            && matches_optional(
                &self.app_session_id,
                session.source.app_session_id.as_deref(),
            )
            && matches_optional(
                &self.app_session_name,
                session.source.app_session_name.as_deref(),
            )
            && matches_optional(&self.runtime_id, session.source.runtime_id.as_deref())
            && matches_optional(&self.actor_id, session.actor_id.as_deref())
    }
}

/// Returns sessions matching the query in newest-first event order.
pub fn query_indexed_sessions<'a>(
    index: &'a TraceIndex,
    query: &SessionQuery,
) -> Vec<&'a IndexedSession> {
    let mut sessions = index
        .sessions
        .values()
        .filter(|session| query.matches(session))
        .collect::<Vec<_>>();
    sessions.sort_by(|left, right| {
        right
            .last_event_at
            .cmp(&left.last_event_at)
            .then_with(|| left.session_id.cmp(&right.session_id))
    });
    sessions
}

fn matches_optional(expected: &Option<String>, actual: Option<&str>) -> bool {
    match expected {
        Some(expected) => actual == Some(expected.as_str()),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use brick_protocol::{ActorRef, ActorType, SessionSource};
    use chrono::Utc;

    use crate::{IndexedSession, SessionQuery};

    #[test]
    fn session_query_filters_source_and_actor_fields() {
        let mut session = IndexedSession::blank(
            "session_1".to_string(),
            Utc::now(),
            &ActorRef {
                actor_type: ActorType::Agent,
                actor_id: "agent-1".to_string(),
                display_name: None,
            },
        );
        session.source = SessionSource {
            app_id: Some("cursor".to_string()),
            app_session_id: Some("native-1".to_string()),
            app_session_name: Some("Phase 5".to_string()),
            runtime_id: Some("runtime-1".to_string()),
        };

        assert!(SessionQuery {
            app_id: Some("cursor".to_string()),
            app_session_id: Some("native-1".to_string()),
            app_session_name: Some("Phase 5".to_string()),
            runtime_id: Some("runtime-1".to_string()),
            actor_id: Some("agent-1".to_string()),
        }
        .matches(&session));

        assert!(!SessionQuery {
            actor_id: Some("other-agent".to_string()),
            ..SessionQuery::default()
        }
        .matches(&session));
    }
}
