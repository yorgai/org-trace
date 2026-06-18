//! Content-addressed storage for artifact attachment bytes.
//!
//! Attachment uploads copy source files into the effective local storage root and
//! record only metadata in JSONL events. The blob path is deterministic by
//! SHA-256 digest so repeated uploads of identical content are idempotent.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use sha2::{Digest, Sha256};

use crate::BLOBS_DIR;

/// Metadata returned after copying a local file into the blob store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredAttachment {
    pub original_path: PathBuf,
    pub storage_path: PathBuf,
    pub storage_uri: String,
    pub sha256: String,
    pub size_bytes: u64,
}

/// Content-addressed local store rooted at the effective Brick storage root.
#[derive(Debug, Clone)]
pub struct AttachmentStore {
    storage_root: PathBuf,
}

impl AttachmentStore {
    /// Creates a content store under an already resolved Brick storage root.
    pub fn new(storage_root: impl Into<PathBuf>) -> Self {
        Self {
            storage_root: storage_root.into(),
        }
    }

    /// Copies `source_path` to `blobs/sha256/<digest>` and returns its metadata.
    pub fn store_file(&self, source_path: impl AsRef<Path>) -> Result<StoredAttachment> {
        let source_path = source_path.as_ref();
        let metadata = source_path.metadata().with_context(|| {
            format!(
                "failed to read attachment metadata at {}",
                source_path.display()
            )
        })?;
        if !metadata.is_file() {
            return Err(anyhow!(
                "attachment path is not a regular file: {}",
                source_path.display()
            ));
        }

        let (sha256, size_bytes) = digest_file(source_path)?;
        let storage_path = self.blob_path(&sha256);
        if let Some(parent) = storage_path.parent() {
            fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create attachment blob directory {}",
                    parent.display()
                )
            })?;
        }

        if storage_path.exists() {
            verify_existing_blob(&storage_path, &sha256, size_bytes)?;
        } else {
            copy_new_blob(source_path, &storage_path)?;
            verify_existing_blob(&storage_path, &sha256, size_bytes)?;
        }

        Ok(StoredAttachment {
            original_path: source_path.to_path_buf(),
            storage_path,
            storage_uri: format!("brick-blob://sha256/{sha256}"),
            sha256,
            size_bytes,
        })
    }

    fn blob_path(&self, sha256: &str) -> PathBuf {
        self.storage_root
            .join(BLOBS_DIR)
            .join("sha256")
            .join(sha256)
    }
}

fn digest_file(path: &Path) -> Result<(String, u64)> {
    let mut file = File::open(path)
        .with_context(|| format!("failed to open attachment file {}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut size_bytes = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];

    loop {
        let read = file
            .read(&mut buffer)
            .with_context(|| format!("failed to read attachment file {}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size_bytes += u64::try_from(read).context("attachment read size cannot fit in u64")?;
    }

    Ok((format!("{:x}", hasher.finalize()), size_bytes))
}

fn verify_existing_blob(path: &Path, expected_sha256: &str, expected_size: u64) -> Result<()> {
    let (actual_sha256, actual_size) = digest_file(path)?;
    if actual_sha256 != expected_sha256 || actual_size != expected_size {
        return Err(anyhow!(
            "existing blob {} does not match expected digest/size",
            path.display()
        ));
    }
    Ok(())
}

fn copy_new_blob(source_path: &Path, storage_path: &Path) -> Result<()> {
    let mut source = File::open(source_path)
        .with_context(|| format!("failed to open attachment file {}", source_path.display()))?;
    let mut destination = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(storage_path)
        .with_context(|| {
            format!(
                "failed to create attachment blob {}",
                storage_path.display()
            )
        })?;
    std::io::copy(&mut source, &mut destination).with_context(|| {
        format!(
            "failed to copy attachment {} to {}",
            source_path.display(),
            storage_path.display()
        )
    })?;
    destination
        .flush()
        .with_context(|| format!("failed to flush attachment blob {}", storage_path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "brick-attachment-store-{name}-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    #[test]
    fn stores_file_by_sha256_and_reuses_existing_blob() {
        let root = temp_dir("store");
        let source = root.join("source.txt");
        fs::write(&source, "hello").expect("write source");
        let store = AttachmentStore::new(root.join("store"));

        let first = store.store_file(&source).expect("store first blob");
        let second = store.store_file(&source).expect("reuse blob");

        assert_eq!(
            first.sha256,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
        assert_eq!(first.size_bytes, 5);
        assert_eq!(first.storage_path, second.storage_path);
        assert_eq!(
            fs::read_to_string(first.storage_path).expect("read blob"),
            "hello"
        );
        assert_eq!(
            first.storage_uri,
            format!("brick-blob://sha256/{}", first.sha256)
        );
    }

    #[test]
    fn rejects_incompatible_existing_blob() {
        let root = temp_dir("conflict");
        let source = root.join("source.txt");
        fs::write(&source, "hello").expect("write source");
        let store_root = root.join("store");
        let blob_path = store_root
            .join(BLOBS_DIR)
            .join("sha256")
            .join("2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824");
        fs::create_dir_all(blob_path.parent().expect("blob parent")).expect("create blob parent");
        fs::write(&blob_path, "corrupt").expect("write corrupt blob");

        let err = AttachmentStore::new(store_root)
            .store_file(&source)
            .expect_err("reject corrupt existing blob");
        assert!(err
            .to_string()
            .contains("does not match expected digest/size"));
    }
}
