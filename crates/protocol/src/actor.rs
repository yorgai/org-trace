//! Actor identity types recorded on every provenance event.
//!
//! Actors may be humans, agents, or system processes. The actor reference is
//! intentionally small so it can travel in each append-only event without
//! requiring a separate identity service during local-first operation.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

/// Broad category of actor responsible for recording or causing an event.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorType {
    Human,
    Agent,
    System,
}

impl FromStr for ActorType {
    type Err = ActorTypeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "human" => Ok(Self::Human),
            "agent" => Ok(Self::Agent),
            "system" => Ok(Self::System),
            _ => Err(ActorTypeParseError {
                value: value.to_string(),
            }),
        }
    }
}

/// Error returned when a CLI or environment value is not a supported actor type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActorTypeParseError {
    value: String,
}

impl fmt::Display for ActorTypeParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unsupported actor type: {}", self.value)
    }
}

impl std::error::Error for ActorTypeParseError {}

/// Lightweight actor reference embedded in each trace event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActorRef {
    pub actor_type: ActorType,
    pub actor_id: String,
    pub display_name: Option<String>,
}
