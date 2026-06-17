use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use chrono::Utc;
use org_trace_protocol::TraceEvent;

pub const PROVENANCE_DIR: &str = ".orgii/provenance";
pub const QUEUE_DIR: &str = "queue";
pub const EVENTS_DIR: &str = "events";

#[derive(Debug, Clone)]
pub struct LocalStore {
    repo_root: PathBuf,
}

impl LocalStore {
    pub fn new(repo_root: impl Into<PathBuf>) -> Self {
        Self {
            repo_root: repo_root.into(),
        }
    }

    pub fn repo_root(&self) -> &Path {
        &self.repo_root
    }

    pub fn provenance_dir(&self) -> PathBuf {
        self.repo_root.join(PROVENANCE_DIR)
    }

    pub fn init(&self) -> Result<()> {
        fs::create_dir_all(self.provenance_dir().join(QUEUE_DIR))
            .context("failed to create provenance queue directory")?;
        fs::create_dir_all(self.provenance_dir().join(EVENTS_DIR))
            .context("failed to create provenance events directory")?;
        Ok(())
    }

    pub fn append_event(&self, event: &TraceEvent) -> Result<PathBuf> {
        self.init()?;

        let date = Utc::now().format("%Y-%m-%d");
        let path = self.provenance_dir().join(QUEUE_DIR).join(format!("{date}.jsonl"));
        let serialized = serde_json::to_string(event).context("failed to serialize trace event")?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("failed to open event queue at {}", path.display()))?;
        writeln!(file, "{serialized}").context("failed to append trace event")?;
        Ok(path)
    }
}

pub fn discover_repo_root(start: impl AsRef<Path>) -> Result<PathBuf> {
    let mut current = start
        .as_ref()
        .canonicalize()
        .with_context(|| format!("failed to canonicalize {}", start.as_ref().display()))?;

    loop {
        if current.join(".git").exists() {
            return Ok(current);
        }

        if !current.pop() {
            return Err(anyhow!("no git repository found"));
        }
    }
}
