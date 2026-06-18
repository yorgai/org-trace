use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde_json::Value;

/// Input for creating or updating a source plan row together with its session edges.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcePlanWithEdgesUpsert {
    pub plan: SourcePlanUpsert,
    pub edges: Vec<SourcePlanSessionEdgeUpsert>,
}

/// Input for creating or updating a source-plan row.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcePlanUpsert {
    pub source_id: String,
    pub external_plan_id: String,
    pub title: Option<String>,
    pub source_path: Option<PathBuf>,
    pub source_uri: Option<String>,
    pub source_mtime: Option<DateTime<Utc>>,
    pub parser_version: Option<String>,
    pub discovered_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub metadata_json: Option<Value>,
}

/// Typed source-plan row returned by metadata DB queries.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcePlanRecord {
    pub source_id: String,
    pub external_plan_id: String,
    pub title: Option<String>,
    pub source_path: Option<PathBuf>,
    pub source_uri: Option<String>,
    pub source_mtime: Option<DateTime<Utc>>,
    pub parser_version: Option<String>,
    pub discovered_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata_json: Option<Value>,
}

/// Optional filters for listing source-plan rows.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SourcePlanListQuery {
    pub source_id: Option<String>,
    pub limit: usize,
    pub offset: usize,
}

/// Input for creating or updating a source plan-to-session edge row.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcePlanSessionEdgeUpsert {
    pub source_id: String,
    pub external_plan_id: String,
    pub external_session_id: String,
    pub role: SourcePlanSessionEdgeRole,
    pub todo_ids_json: Option<Value>,
    pub discovered_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub metadata_json: Option<Value>,
}

/// Typed source plan-to-session edge row returned by metadata DB queries.
#[derive(Debug, Clone, PartialEq)]
pub struct SourcePlanSessionEdgeRecord {
    pub source_id: String,
    pub external_plan_id: String,
    pub external_session_id: String,
    pub role: SourcePlanSessionEdgeRole,
    pub todo_ids_json: Option<Value>,
    pub discovered_at: DateTime<Utc>,
    pub last_seen_at: DateTime<Utc>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
    pub metadata_json: Option<Value>,
}

/// Role for a recovered source plan-to-session edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum SourcePlanSessionEdgeRole {
    CreatedBy,
    EditedBy,
    ReferencedBy,
    BuiltBy,
}

impl SourcePlanSessionEdgeRole {
    pub const CREATED_BY: &'static str = "created_by";
    pub const EDITED_BY: &'static str = "edited_by";
    pub const REFERENCED_BY: &'static str = "referenced_by";
    pub const BUILT_BY: &'static str = "built_by";

    pub fn as_str(self) -> &'static str {
        match self {
            Self::CreatedBy => Self::CREATED_BY,
            Self::EditedBy => Self::EDITED_BY,
            Self::ReferencedBy => Self::REFERENCED_BY,
            Self::BuiltBy => Self::BUILT_BY,
        }
    }
}

impl fmt::Display for SourcePlanSessionEdgeRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SourcePlanSessionEdgeRole {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            Self::CREATED_BY => Ok(Self::CreatedBy),
            Self::EDITED_BY => Ok(Self::EditedBy),
            Self::REFERENCED_BY => Ok(Self::ReferencedBy),
            Self::BUILT_BY => Ok(Self::BuiltBy),
            other => Err(format!("unknown source plan-session edge role: {other}")),
        }
    }
}
