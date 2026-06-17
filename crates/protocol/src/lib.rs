use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

pub const EVENT_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    Human,
    Agent,
    System,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventType {
    MissionCreated,
    MissionUpdated,
    SessionStarted,
    SessionLinkedToMission,
    ArtifactCreated,
    ArtifactLinkedToMission,
    ArtifactReviewed,
    ArtifactAccepted,
    RepoContextCaptured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfidenceLevel {
    Explicit,
    Observed,
    Imported,
    Inferred,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    pub actor_type: ActorType,
    pub actor_id: String,
    pub display_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TraceEvent {
    pub event_id: Uuid,
    pub event_type: EventType,
    pub schema_version: u16,
    pub payload_schema_version: u16,
    pub occurred_at: DateTime<Utc>,
    pub recorded_at: DateTime<Utc>,
    pub actor: ActorRef,
    pub repo_id: Option<String>,
    pub mission_id: Option<String>,
    pub session_id: Option<String>,
    pub artifact_id: Option<String>,
    pub confidence: ConfidenceLevel,
    pub payload: Value,
}

impl TraceEvent {
    pub fn new(event_type: EventType, actor: ActorRef, payload: Value) -> Self {
        let now = Utc::now();

        Self {
            event_id: Uuid::new_v4(),
            event_type,
            schema_version: EVENT_SCHEMA_VERSION,
            payload_schema_version: EVENT_SCHEMA_VERSION,
            occurred_at: now,
            recorded_at: now,
            actor,
            repo_id: None,
            mission_id: None,
            session_id: None,
            artifact_id: None,
            confidence: ConfidenceLevel::Explicit,
            payload,
        }
    }
}
