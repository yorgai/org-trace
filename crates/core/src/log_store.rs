//! Content-addressed storage wrapper for session log bytes.
//!
//! Session logs intentionally share the same underlying blob store as artifact
//! attachments while exposing a separate domain API so log uploads cannot be
//! confused with artifact attachment uploads in callers or events.

use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::{AttachmentStore, StoredAttachment};

/// Metadata returned after copying a session log into the shared blob store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSessionLog {
    pub original_path: PathBuf,
    pub storage_path: PathBuf,
    pub storage_uri: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Domain-specific wrapper for storing session log content-addressed bytes.
#[derive(Debug, Clone)]
pub struct LogStore {
    blob_store: AttachmentStore,
}

impl LogStore {
    /// Creates a log store backed by the existing content-addressed blob root.
    pub fn new(storage_root: impl Into<PathBuf>) -> Self {
        Self {
            blob_store: AttachmentStore::new(storage_root),
        }
    }

    /// Copies `source_path` into shared blob storage and returns log metadata.
    pub fn store_file(&self, source_path: impl AsRef<Path>) -> Result<StoredSessionLog> {
        let stored = self.blob_store.store_file(source_path)?;
        Ok(stored.into())
    }
}

impl From<StoredAttachment> for StoredSessionLog {
    fn from(stored: StoredAttachment) -> Self {
        Self {
            original_path: stored.original_path,
            storage_path: stored.storage_path,
            storage_uri: stored.storage_uri,
            sha256: stored.sha256,
            size_bytes: stored.size_bytes,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-log-store-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn stores_log_in_shared_content_addressed_blobs() {
        let root = temp_dir("store");
        let source = root.join("session.jsonl");
        fs::write(&source, "hello").expect("write log");
        let store = LogStore::new(root.join("store"));

        let stored = store.store_file(&source).expect("store log");

        assert_eq!(
            stored.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(stored.size_bytes, 5);
        assert_eq!(
            fs::read_to_string(stored.storage_path).expect("read blob"),
            "hello"
        );
        assert_eq!(
            stored.storage_uri,
            format!("brick-blob://sha256/{}", stored.sha256)
        );
    }
}
