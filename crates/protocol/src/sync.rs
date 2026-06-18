//! Sync protocol messages shared by CLI and self-hosted server.
//!
//! These types describe event transfer without committing to auth, queue
//! draining, or conflict resolution. The server accepts append-only events and
//! reports duplicates by event ID so clients can retry safely.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::TraceEvent;

/// Stable server-side event cursor used for paginated sync.
///
/// The MVP server encodes this as an append log sequence number. Clients should
/// treat it as an opaque string and pass `next_cursor` back as the next `after`
/// query value.
pub type EventCursor = String;

/// Request body for pushing locally queued events to a remote trace server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PushEventsRequest {
    pub events: Vec<TraceEvent>,
}

/// Response body describing which pushed events were newly accepted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushEventsResponse {
    pub accepted_event_ids: Vec<Uuid>,
    pub duplicate_event_ids: Vec<Uuid>,
}

impl PushEventsResponse {
    /// Number of events appended by the server during this request.
    pub fn accepted_count(&self) -> usize {
        self.accepted_event_ids.len()
    }

    /// Number of pushed events the server had already stored.
    pub fn duplicate_count(&self) -> usize {
        self.duplicate_event_ids.len()
    }
}

/// Response body for listing events currently stored by a trace server.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ListEventsResponse {
    pub events: Vec<TraceEvent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<EventCursor>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_cursor: Option<EventCursor>,
}

impl ListEventsResponse {
    /// Builds a non-paginated compatibility response.
    pub fn all(events: Vec<TraceEvent>) -> Self {
        Self {
            events,
            cursor: None,
            next_cursor: None,
        }
    }

    /// Builds a paginated response with the requested and following cursors.
    pub fn page(
        events: Vec<TraceEvent>,
        cursor: Option<EventCursor>,
        next_cursor: Option<EventCursor>,
    ) -> Self {
        Self {
            events,
            cursor,
            next_cursor,
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{ActorRef, ActorType, MissionCreatedPayload, MissionId};

    use super::*;

    fn event() -> TraceEvent {
        TraceEvent::mission_created(
            ActorRef {
                actor_type: ActorType::Human,
                actor_id: "tester".to_string(),
                display_name: None,
            },
            MissionId::new(),
            MissionCreatedPayload {
                title: "Sync payload".to_string(),
                description: None,
                repo_context_id: None,
            },
        )
        .expect("build event")
    }

    #[test]
    fn push_response_counts_accepted_and_duplicate_events() {
        let accepted_event_id = Uuid::new_v4();
        let duplicate_event_id = Uuid::new_v4();
        let response = PushEventsResponse {
            accepted_event_ids: vec![accepted_event_id],
            duplicate_event_ids: vec![duplicate_event_id],
        };

        assert_eq!(response.accepted_count(), 1);
        assert_eq!(response.duplicate_count(), 1);
    }

    #[test]
    fn sync_requests_round_trip_as_json() {
        let event = event();
        let request = PushEventsRequest {
            events: vec![event.clone()],
        };
        let listed = ListEventsResponse::all(vec![event]);

        let request_json = serde_json::to_string(&request).expect("serialize request");
        let listed_json = serde_json::to_string(&listed).expect("serialize list response");
        let decoded_request: PushEventsRequest =
            serde_json::from_str(&request_json).expect("decode request");
        let decoded_list: ListEventsResponse =
            serde_json::from_str(&listed_json).expect("decode list response");

        assert_eq!(decoded_request.events.len(), 1);
        assert_eq!(decoded_list.events.len(), 1);
        assert_eq!(decoded_list.next_cursor, None);
    }

    #[test]
    fn paginated_list_response_preserves_cursors() {
        let listed = ListEventsResponse::page(
            vec![event()],
            Some("10".to_string()),
            Some("11".to_string()),
        );

        let listed_json = serde_json::to_string(&listed).expect("serialize list response");
        let decoded: ListEventsResponse =
            serde_json::from_str(&listed_json).expect("decode list response");

        assert_eq!(decoded.cursor.as_deref(), Some("10"));
        assert_eq!(decoded.next_cursor.as_deref(), Some("11"));
    }

    #[test]
    fn legacy_list_response_without_cursors_decodes() {
        let decoded: ListEventsResponse =
            serde_json::from_str(r#"{"events":[]}"#).expect("decode legacy response");

        assert!(decoded.events.is_empty());
        assert_eq!(decoded.cursor, None);
        assert_eq!(decoded.next_cursor, None);
    }
}
