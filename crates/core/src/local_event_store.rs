//! Local event/chunk SQLite store.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use brick_protocol::{EventType, SourceSessionObservedPayload, TraceEvent};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct EventChunkRow {
    pub event_id: String,
    pub repo_id: String,
    pub org_id: Option<String>,
    pub source_id: String,
    pub external_session_id: String,
    pub chunk_index: i64,
    pub chunk_kind: Option<String>,
    pub role: Option<String>,
    pub actor_id: Option<String>,
    pub occurred_at: Option<String>,
    pub text: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct LocalEventStore {
    path: PathBuf,
}

impl LocalEventStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn init(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create local event DB directory {}",
                    parent.display()
                )
            })?;
        }
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open local event DB at {}", self.path.display()))?;
        create_schema(&connection)
    }

    pub fn append_event(&self, repo_id: &str, event: &TraceEvent) -> Result<()> {
        self.init()?;
        let mut event = event.clone();
        if event.repo_id.is_none() {
            event.repo_id = Some(repo_id.to_string());
        }
        let (compact_event, chunks) = compact_event_and_chunks(&event)?;
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open local event DB at {}", self.path.display()))?;
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT OR IGNORE INTO brick_events (
                event_id, repo_id, org_id, occurred_at, event_json, inserted_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                event.event_id.to_string(),
                event.repo_id.as_deref().unwrap_or(repo_id),
                event.org_id.as_ref().map(ToString::to_string),
                event.occurred_at.to_rfc3339(),
                serde_json::to_string(&compact_event)?,
                Utc::now().to_rfc3339(),
            ],
        )?;
        transaction.execute(
            "DELETE FROM brick_event_chunks WHERE event_id = ?1",
            [event.event_id.to_string()],
        )?;
        for chunk in chunks {
            transaction.execute(
                "INSERT INTO brick_event_chunks (
                    event_id, repo_id, org_id, source_id, external_session_id,
                    chunk_index, chunk_kind, role, actor_id, occurred_at, text, raw_json, inserted_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)",
                params![
                    chunk.event_id,
                    chunk.repo_id,
                    chunk.org_id,
                    chunk.source_id,
                    chunk.external_session_id,
                    chunk.chunk_index,
                    chunk.chunk_kind,
                    chunk.role,
                    chunk.actor_id,
                    chunk.occurred_at,
                    chunk.text,
                    serde_json::to_string(&chunk.raw)?,
                    Utc::now().to_rfc3339(),
                ],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    pub fn append_events(&self, repo_id: &str, events: &[TraceEvent]) -> Result<()> {
        for event in events {
            self.append_event(repo_id, event)?;
        }
        Ok(())
    }

    pub fn read_events(&self) -> Result<Vec<TraceEvent>> {
        self.read_events_for_repo(None)
    }

    pub fn read_events_for_repo(&self, repo_id: Option<&str>) -> Result<Vec<TraceEvent>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open local event DB at {}", self.path.display()))?;
        create_schema(&connection)?;
        let mut statement = connection.prepare(
            "SELECT event_json FROM brick_events
             WHERE (?1 IS NULL OR repo_id = ?1)
             ORDER BY occurred_at ASC, event_id ASC",
        )?;
        let rows = statement.query_map([repo_id], |row| row.get::<_, String>(0))?;
        let mut events = Vec::new();
        for row in rows {
            let event_json = row.context("failed to read local event row")?;
            events.push(serde_json::from_str(&event_json).context("failed to decode local event")?);
        }
        Ok(events)
    }

    pub fn read_events_for_repo_with_chunks(
        &self,
        repo_id: Option<&str>,
    ) -> Result<Vec<TraceEvent>> {
        let mut events = self.read_events_for_repo(repo_id)?;
        if events
            .iter()
            .all(|event| event.event_type != EventType::SourceSessionObserved)
        {
            return Ok(events);
        }
        if !self.path.exists() {
            return Ok(events);
        }
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open local event DB at {}", self.path.display()))?;
        create_schema(&connection)?;
        let mut statement = connection.prepare(
            "SELECT raw_json FROM brick_event_chunks
             WHERE event_id = ?1
             ORDER BY chunk_index ASC",
        )?;
        for event in &mut events {
            if event.event_type != EventType::SourceSessionObserved {
                continue;
            }
            let event_id = event.event_id.to_string();
            let rows = statement.query_map([event_id], |row| row.get::<_, String>(0))?;
            let mut chunks = Vec::new();
            for row in rows {
                let raw_json = row.context("failed to read local event chunk")?;
                chunks.push(
                    serde_json::from_str(&raw_json)
                        .context("failed to decode local event chunk")?,
                );
            }
            if let Some(payload) = event.payload.as_object_mut() {
                payload.insert("normalized_chunks".to_string(), Value::Array(chunks));
            }
        }
        Ok(events)
    }

    pub fn event_count(&self) -> Result<usize> {
        self.event_count_for_repo(None)
    }

    pub fn event_count_for_repo(&self, repo_id: Option<&str>) -> Result<usize> {
        if !self.path.exists() {
            return Ok(0);
        }
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open local event DB at {}", self.path.display()))?;
        create_schema(&connection)?;
        connection
            .query_row(
                "SELECT COUNT(*) FROM brick_events WHERE (?1 IS NULL OR repo_id = ?1)",
                [repo_id],
                |row| row.get::<_, i64>(0),
            )
            .map(|count| count as usize)
            .context("failed to count local events")
    }

    pub fn read_session_chunks(
        &self,
        source_id: &str,
        external_session_id: &str,
    ) -> Result<Vec<EventChunkRow>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let connection = Connection::open(&self.path)
            .with_context(|| format!("failed to open local event DB at {}", self.path.display()))?;
        create_schema(&connection)?;
        let mut statement = connection.prepare(
            "SELECT event_id, repo_id, org_id, source_id, external_session_id, chunk_index,
                    chunk_kind, role, actor_id, occurred_at, text, raw_json
             FROM brick_event_chunks
             WHERE source_id = ?1 AND external_session_id = ?2
             ORDER BY chunk_index ASC, occurred_at ASC, event_id ASC",
        )?;
        let rows = statement.query_map(params![source_id, external_session_id], |row| {
            let raw_json: String = row.get(11)?;
            Ok(EventChunkRow {
                event_id: row.get(0)?,
                repo_id: row.get(1)?,
                org_id: row.get(2)?,
                source_id: row.get(3)?,
                external_session_id: row.get(4)?,
                chunk_index: row.get(5)?,
                chunk_kind: row.get(6)?,
                role: row.get(7)?,
                actor_id: row.get(8)?,
                occurred_at: row.get(9)?,
                text: row.get(10)?,
                raw: serde_json::from_str(&raw_json).unwrap_or_else(|_| json!({})),
            })
        })?;
        let mut chunks = Vec::new();
        for row in rows {
            chunks.push(row.context("failed to read local event chunk")?);
        }
        Ok(chunks)
    }
}

fn create_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        "PRAGMA foreign_keys = ON;
         CREATE TABLE IF NOT EXISTS brick_events (
            event_id TEXT PRIMARY KEY,
            repo_id TEXT NOT NULL,
            org_id TEXT,
            occurred_at TEXT NOT NULL,
            event_json TEXT NOT NULL,
            inserted_at TEXT NOT NULL
         );
         CREATE TABLE IF NOT EXISTS brick_event_chunks (
            event_id TEXT NOT NULL,
            repo_id TEXT NOT NULL,
            org_id TEXT,
            source_id TEXT NOT NULL,
            external_session_id TEXT NOT NULL,
            chunk_index INTEGER NOT NULL,
            chunk_kind TEXT,
            role TEXT,
            actor_id TEXT,
            occurred_at TEXT,
            text TEXT,
            raw_json TEXT NOT NULL,
            inserted_at TEXT NOT NULL,
            PRIMARY KEY (event_id, chunk_index),
            FOREIGN KEY(event_id) REFERENCES brick_events(event_id) ON DELETE CASCADE
         );
         CREATE INDEX IF NOT EXISTS brick_events_repo_occurred_idx
            ON brick_events(repo_id, occurred_at);
         CREATE INDEX IF NOT EXISTS brick_event_chunks_repo_session_idx
            ON brick_event_chunks(repo_id, source_id, external_session_id, chunk_index);
         CREATE INDEX IF NOT EXISTS brick_event_chunks_repo_occurred_idx
            ON brick_event_chunks(repo_id, occurred_at);",
    )?;
    Ok(())
}

fn compact_event_and_chunks(event: &TraceEvent) -> Result<(TraceEvent, Vec<EventChunkRow>)> {
    if event.event_type != EventType::SourceSessionObserved {
        return Ok((event.clone(), Vec::new()));
    }
    let mut payload: SourceSessionObservedPayload = serde_json::from_value(event.payload.clone())?;
    let raw_chunks = std::mem::take(&mut payload.normalized_chunks);
    let mut compact = event.clone();
    compact.payload = serde_json::to_value(&payload)?;

    let repo_id = event.repo_id.clone().unwrap_or_default();
    let org_id = event.org_id.as_ref().map(ToString::to_string);
    let chunks = raw_chunks
        .into_iter()
        .enumerate()
        .map(|(index, raw)| EventChunkRow {
            event_id: event.event_id.to_string(),
            repo_id: repo_id.clone(),
            org_id: org_id.clone(),
            source_id: payload.source_id.clone(),
            external_session_id: payload.external_session_id.clone(),
            chunk_index: index as i64,
            chunk_kind: chunk_kind(&raw),
            role: chunk_role(&raw),
            actor_id: chunk_actor_id(&raw),
            occurred_at: chunk_time(&raw),
            text: chunk_text(&raw),
            raw,
        })
        .collect();
    Ok((compact, chunks))
}

fn chunk_kind(raw: &Value) -> Option<String> {
    raw.get("action_type")
        .or_else(|| raw.get("kind"))
        .or_else(|| raw.get("type"))
        .or_else(|| raw.get("function"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn chunk_role(raw: &Value) -> Option<String> {
    raw.get("role")
        .or_else(|| raw.pointer("/result/role"))
        .or_else(|| raw.pointer("/result/message/role"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn chunk_actor_id(raw: &Value) -> Option<String> {
    raw.get("actor_id")
        .or_else(|| raw.pointer("/actor/actor_id"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn chunk_time(raw: &Value) -> Option<String> {
    raw.get("created_at")
        .or_else(|| raw.get("occurred_at"))
        .or_else(|| raw.get("timestamp"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

fn chunk_text(raw: &Value) -> Option<String> {
    raw.get("text")
        .or_else(|| raw.get("content"))
        .or_else(|| raw.pointer("/message/content"))
        .or_else(|| raw.pointer("/result/content"))
        .or_else(|| raw.pointer("/result/message/content"))
        .or_else(|| raw.pointer("/result/observation"))
        .or_else(|| raw.pointer("/result/output"))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
}

pub fn event_exists(path: &Path, event_id: &str) -> Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let connection = Connection::open(path)
        .with_context(|| format!("failed to open local event DB at {}", path.display()))?;
    create_schema(&connection)?;
    connection
        .query_row(
            "SELECT 1 FROM brick_events WHERE event_id = ?1",
            [event_id],
            |_| Ok(()),
        )
        .optional()
        .map(|row| row.is_some())
        .context("failed to check local event existence")
}

#[cfg(test)]
mod tests {
    use super::*;
    use brick_protocol::{ActorRef, ActorType, ConfidenceLevel, SourceSessionObservedPayload};

    #[test]
    fn stores_source_chunks_separately_from_compact_event() {
        let path = std::env::temp_dir().join(format!(
            "brick-local-event-store-{}.sqlite",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let store = LocalEventStore::new(&path);
        let payload = SourceSessionObservedPayload {
            source_id: "orgii".to_string(),
            external_session_id: "session-1".to_string(),
            title: None,
            name: None,
            source_path: None,
            source_uri: None,
            source_mtime: None,
            source_size: None,
            source_fingerprint: None,
            parser_version: None,
            session_created_at: None,
            session_updated_at: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            repo_path: None,
            branch: None,
            files_changed: None,
            lines_added: None,
            lines_removed: None,
            touched_files: Vec::new(),
            metadata_json: None,
            normalized_chunks: vec![json!({
                "action_type": "assistant",
                "result": { "content": "done", "role": "assistant" },
                "created_at": "2026-06-26T00:00:00Z"
            })],
        };
        let mut event = TraceEvent::source_session_observed(
            ActorRef {
                actor_type: ActorType::Agent,
                actor_id: "agent".to_string(),
                display_name: None,
            },
            payload,
        )
        .expect("event");
        event.repo_id = Some("repo-1".to_string());
        event.confidence = ConfidenceLevel::Observed;

        store.append_event("repo-1", &event).expect("append event");
        let events = store.read_events().expect("read events");
        assert_eq!(events.len(), 1);
        assert!(events[0].payload.get("normalized_chunks").is_none());
        let chunks = store
            .read_session_chunks("orgii", "session-1")
            .expect("read chunks");
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].text.as_deref(), Some("done"));
        let hydrated = store
            .read_events_for_repo_with_chunks(Some("repo-1"))
            .expect("read events with chunks");
        assert_eq!(
            hydrated[0].payload["normalized_chunks"][0]["result"]["content"],
            "done"
        );
        let _ = std::fs::remove_file(path);
    }
}
