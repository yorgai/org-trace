//! Strongly typed identifiers for provenance graph entities.
//!
//! IDs keep their human-readable prefixes in serialized form so JSONL logs stay
//! easy to inspect while Rust callers cannot accidentally mix missions,
//! sessions, artifacts, or repo contexts.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

macro_rules! trace_id_type {
    ($name:ident, $prefix:literal) => {
        #[doc = concat!("Typed Brick identifier serialized with the `", $prefix, "` prefix.")]
        #[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl $name {
            /// Creates a new prefixed UUID identifier for local event recording.
            pub fn new() -> Self {
                Self(format!("{}{}", $prefix, Uuid::new_v4()))
            }

            /// Returns the serialized identifier string without allocating.
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl FromStr for $name {
            type Err = TraceIdParseError;

            fn from_str(value: &str) -> Result<Self, Self::Err> {
                if value.trim().is_empty() {
                    return Err(TraceIdParseError::Empty);
                }
                Ok(Self(value.to_string()))
            }
        }
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TraceIdParseError {
    Empty,
}

impl fmt::Display for TraceIdParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TraceIdParseError::Empty => formatter.write_str("trace id cannot be empty"),
        }
    }
}

impl std::error::Error for TraceIdParseError {}

trace_id_type!(OrgId, "org_");
trace_id_type!(ProjectId, "project_");
trace_id_type!(MissionId, "mission_");
trace_id_type!(SessionId, "session_");
trace_id_type!(ArtifactId, "artifact_");
trace_id_type!(AttachmentId, "attach_");
trace_id_type!(LogRefId, "logref_");
trace_id_type!(RepoContextId, "repoctx_");
trace_id_type!(ExternalRefId, "extref_");
trace_id_type!(FileRefId, "fileref_");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_ids_round_trip_as_strings() {
        let mission_id = MissionId::new();
        let serialized = serde_json::to_string(&mission_id).expect("serialize mission id");
        let deserialized: MissionId =
            serde_json::from_str(&serialized).expect("deserialize mission id");
        assert_eq!(mission_id, deserialized);
    }

    #[test]
    fn rejects_empty_trace_id() {
        let result = "".parse::<SessionId>();
        assert!(matches!(result, Err(TraceIdParseError::Empty)));
    }
}
