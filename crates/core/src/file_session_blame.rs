//! File-level session attribution rows for runtime and source metadata provenance.
//!
//! This is intentionally file/session-level attribution. It does not claim Git
//! line-level authorship.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Evidence source for a file/session attribution row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileSessionBlameEvidenceKind {
    RuntimeEvent,
    SourceMetadata,
    ChunkPointer,
}

impl FileSessionBlameEvidenceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::RuntimeEvent => "runtime_event",
            Self::SourceMetadata => "source_metadata",
            Self::ChunkPointer => "chunk_pointer",
        }
    }
}

/// Filters for querying file/session attribution rows from runtime provenance.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SqliteFileSessionBlameQuery {
    pub file_path: String,
    pub limit: usize,
}

/// Filters for querying file/session attribution rows from source metadata.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceFileSessionBlameQuery {
    pub file_path: String,
    pub source_id: Option<String>,
    pub repo_path: Option<PathBuf>,
    pub limit: usize,
}

/// A raw, auditable file-level attribution row.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FileSessionBlameRow {
    pub file_path: String,
    pub session_id: Option<String>,
    pub external_session_id: Option<String>,
    pub source_id: Option<String>,
    pub app_id: Option<String>,
    pub actor_id: Option<String>,
    pub actor_type: Option<String>,
    pub evidence_kind: FileSessionBlameEvidenceKind,
    pub last_seen_at: String,
    pub lines_added: Option<u64>,
    pub lines_removed: Option<u64>,
    pub files_changed: Option<u64>,
    pub confidence: Option<String>,
    pub source_pointer: Option<Value>,
}
