use anyhow::{bail, Context, Result};
use brick_protocol::{EventType, TraceEvent};
use serde::{Deserialize, Serialize};

use crate::identity;
use crate::wire::{ListEventsResponse, PushEventsRequest, PushEventsResponse};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupabaseRemote {
    url: String,
    anon_key: String,
}

impl SupabaseRemote {
    pub fn from_env() -> Result<Self> {
        let (url, anon_key) = identity::supabase_config()?;
        Ok(Self { url, anon_key })
    }

    #[cfg(test)]
    fn new(url: impl Into<String>, anon_key: impl Into<String>) -> Self {
        Self {
            url: url.into().trim_end_matches('/').to_string(),
            anon_key: anon_key.into(),
        }
    }

    pub fn push_events(
        &self,
        repo_id: Option<&str>,
        request: &PushEventsRequest,
        bearer: &str,
    ) -> Result<PushEventsResponse> {
        ensure_repo_id(repo_id)?;
        let mut accepted_event_ids = Vec::new();
        let mut duplicate_event_ids = Vec::new();
        for event in &request.events {
            let rows = self.insert_event(event, bearer)?;
            self.insert_event_chunks(event, bearer)?;
            if rows.is_empty() {
                duplicate_event_ids.push(event.event_id);
            } else {
                accepted_event_ids.push(event.event_id);
            }
        }
        Ok(PushEventsResponse {
            accepted_event_ids,
            duplicate_event_ids,
        })
    }

    pub fn get_all_events(
        &self,
        repo_id: Option<&str>,
        bearer: &str,
    ) -> Result<ListEventsResponse> {
        let repo_id = ensure_repo_id(repo_id)?;
        let mut response = ureq::get(&format!(
            "{}/rest/v1/brick_events?repo_id=eq.{}&select=event&order=occurred_at.asc",
            self.url,
            urlencoding::encode(repo_id)
        ))
        .header("apikey", &self.anon_key)
        .header("authorization", &format!("Bearer {bearer}"))
        .call()
        .with_context(|| format!("failed to list Brick events from Supabase for repo {repo_id}"))?;
        let rows = response
            .body_mut()
            .read_json::<Vec<EventRow>>()
            .context("failed to decode Supabase Brick events")?;
        let mut events: Vec<TraceEvent> = rows.into_iter().map(|row| row.event).collect();
        self.attach_event_chunks(repo_id, &mut events, bearer)?;
        Ok(ListEventsResponse::all(events))
    }

    pub fn create_org(&self, org_id: &str, bearer: &str) -> Result<()> {
        self.rpc(
            "brick_create_org",
            serde_json::json!({ "p_org_id": org_id }),
            bearer,
        )
    }

    pub fn invite_org_member(&self, org_id: &str, email: &str, bearer: &str) -> Result<()> {
        self.rpc(
            "brick_invite_org_member",
            serde_json::json!({ "p_org_id": org_id, "p_email": email }),
            bearer,
        )
    }

    pub fn accept_invites(&self, bearer: &str) -> Result<()> {
        self.rpc("brick_accept_invites", serde_json::json!({}), bearer)
    }

    fn insert_event(&self, event: &TraceEvent, bearer: &str) -> Result<Vec<EventIdRow>> {
        let row = InsertEventRow::from_event(event)?;
        let mut response = ureq::post(&format!(
            "{}/rest/v1/brick_events?on_conflict=event_id",
            self.url
        ))
        .header("apikey", &self.anon_key)
        .header("authorization", &format!("Bearer {bearer}"))
        .header("content-type", "application/json")
        .header(
            "prefer",
            "resolution=ignore-duplicates,return=representation",
        )
        .send_json(row)
        .with_context(|| {
            format!(
                "failed to insert Brick event {} into Supabase",
                event.event_id
            )
        })?;
        response
            .body_mut()
            .read_json::<Vec<EventIdRow>>()
            .context("failed to decode Supabase insert response")
    }

    fn insert_event_chunks(&self, event: &TraceEvent, bearer: &str) -> Result<()> {
        let rows = EventChunkRows::from_event(event)?;
        for chunk in rows.rows.chunks(500) {
            ureq::post(&format!(
                "{}/rest/v1/brick_event_chunks?on_conflict=event_id,chunk_index",
                self.url
            ))
            .header("apikey", &self.anon_key)
            .header("authorization", &format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .header("prefer", "resolution=ignore-duplicates,return=minimal")
            .send_json(chunk)
            .map_err(|error| chunk_insert_error(error, event.event_id))?;
        }
        Ok(())
    }

    fn attach_event_chunks(
        &self,
        repo_id: &str,
        events: &mut [TraceEvent],
        bearer: &str,
    ) -> Result<()> {
        let rows = self.get_all_event_chunks(repo_id, bearer)?;
        for event in events {
            if event.event_type != EventType::SourceSessionObserved {
                continue;
            }
            let chunks: Vec<serde_json::Value> = rows
                .iter()
                .filter(|row| row.event_id == event.event_id)
                .map(|row| row.raw.clone())
                .collect();
            if !chunks.is_empty() {
                event.payload["normalized_chunks"] = serde_json::Value::Array(chunks);
            }
        }
        Ok(())
    }

    fn get_all_event_chunks(&self, repo_id: &str, bearer: &str) -> Result<Vec<EventChunkRawRow>> {
        let mut rows = Vec::new();
        let mut offset = 0;
        loop {
            let mut response = ureq::get(&format!(
                "{}/rest/v1/brick_event_chunks?repo_id=eq.{}&select=event_id,raw&order=event_id.asc,chunk_index.asc",
                self.url,
                urlencoding::encode(repo_id)
            ))
            .header("apikey", &self.anon_key)
            .header("authorization", &format!("Bearer {bearer}"))
            .header("range", &format!("{offset}-{}", offset + 999))
            .call()
            .with_context(|| {
                format!("failed to list Brick event chunks from Supabase for repo {repo_id}")
            })?;
            let page = response
                .body_mut()
                .read_json::<Vec<EventChunkRawRow>>()
                .context("failed to decode Supabase Brick event chunks")?;
            let page_len = page.len();
            rows.extend(page);
            if page_len < 1000 {
                return Ok(rows);
            }
            offset += page_len;
        }
    }

    fn rpc(&self, name: &str, body: serde_json::Value, bearer: &str) -> Result<()> {
        let endpoint = format!("{}/rest/v1/rpc/{name}", self.url);
        ureq::post(&endpoint)
            .header("apikey", &self.anon_key)
            .header("authorization", &format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .send_json(body)
            .with_context(|| format!("failed to call Supabase RPC {name}"))?;
        Ok(())
    }
}

#[derive(Debug, Serialize)]
struct InsertEventRow {
    event_id: uuid::Uuid,
    repo_id: String,
    org_id: String,
    occurred_at: chrono::DateTime<chrono::Utc>,
    event: TraceEvent,
}

impl InsertEventRow {
    fn from_event(event: &TraceEvent) -> Result<Self> {
        let repo_id = event
            .repo_id
            .as_deref()
            .filter(|repo_id| !repo_id.trim().is_empty())
            .context("Supabase event upload requires event.repo_id")?
            .to_string();
        let org_id = event
            .org_id
            .as_ref()
            .map(ToString::to_string)
            .filter(|org_id| !org_id.trim().is_empty())
            .context("Supabase event upload requires event.org_id; pass --org-id")?;
        Ok(Self {
            event_id: event.event_id,
            repo_id,
            org_id,
            occurred_at: event.occurred_at,
            event: event_without_chunks(event),
        })
    }
}

#[derive(Debug, Serialize)]
struct EventChunkRow<'a> {
    event_id: uuid::Uuid,
    repo_id: &'a str,
    org_id: String,
    source_id: &'a str,
    external_session_id: &'a str,
    chunk_index: i64,
    chunk_kind: Option<&'a str>,
    role: Option<&'a str>,
    actor_id: Option<&'a str>,
    occurred_at: Option<&'a str>,
    text: Option<&'a str>,
    raw: &'a serde_json::Value,
}

struct EventChunkRows<'a> {
    rows: Vec<EventChunkRow<'a>>,
}

impl<'a> EventChunkRows<'a> {
    fn from_event(event: &'a TraceEvent) -> Result<Self> {
        if event.event_type != EventType::SourceSessionObserved {
            return Ok(Self { rows: Vec::new() });
        }
        let repo_id = event
            .repo_id
            .as_deref()
            .filter(|repo_id| !repo_id.trim().is_empty())
            .context("Supabase event upload requires event.repo_id")?;
        let org_id = event
            .org_id
            .as_ref()
            .map(ToString::to_string)
            .filter(|org_id| !org_id.trim().is_empty())
            .context("Supabase event upload requires event.org_id; pass --org-id")?;
        let source_id = event
            .payload
            .get("source_id")
            .and_then(serde_json::Value::as_str)
            .context("source.session_observed payload missing source_id")?;
        let external_session_id = event
            .payload
            .get("external_session_id")
            .and_then(serde_json::Value::as_str)
            .context("source.session_observed payload missing external_session_id")?;
        let chunks = event
            .payload
            .get("normalized_chunks")
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let rows = chunks
            .iter()
            .enumerate()
            .map(|(index, chunk)| EventChunkRow {
                event_id: event.event_id,
                repo_id,
                org_id: org_id.clone(),
                source_id,
                external_session_id,
                chunk_index: index as i64,
                chunk_kind: first_str(
                    chunk,
                    &[&["action_type"], &["kind"], &["type"], &["function"]],
                ),
                role: first_str(
                    chunk,
                    &[
                        &["role"],
                        &["result", "role"],
                        &["result", "message", "role"],
                    ],
                ),
                actor_id: first_str(chunk, &[&["actor_id"], &["actor", "actor_id"]]),
                occurred_at: first_str(chunk, &[&["created_at"], &["occurred_at"], &["timestamp"]])
                    .filter(|value| value.len() >= 10 && value.as_bytes().get(4) == Some(&b'-')),
                text: first_str(
                    chunk,
                    &[
                        &["text"],
                        &["content"],
                        &["message", "content"],
                        &["result", "content"],
                        &["result", "message", "content"],
                        &["result", "observation"],
                        &["result", "output"],
                    ],
                ),
                raw: chunk,
            })
            .collect();
        Ok(Self { rows })
    }
}

fn first_str<'a>(value: &'a serde_json::Value, paths: &[&[&str]]) -> Option<&'a str> {
    paths.iter().find_map(|path| {
        let mut current = value;
        for key in *path {
            current = current.get(*key)?;
        }
        current.as_str()
    })
}

/// Turns a chunk-insert transport error into an actionable one. A `403` here is
/// almost always the known production drift where `brick_event_chunks` lacks its
/// INSERT RLS policy (the `brick_can_insert_event_chunk` helper was never
/// deployed), so point the operator straight at the idempotent patch instead of
/// surfacing a bare `http status: 403`.
fn chunk_insert_error(error: ureq::Error, event_id: uuid::Uuid) -> anyhow::Error {
    if matches!(error, ureq::Error::StatusCode(403)) {
        return anyhow::anyhow!(
            "failed to insert Brick event chunks for event {event_id}: Supabase returned 403 \
             (row-level security). The brick_event_chunks INSERT policy is missing on this \
             project. Apply docs/self-hosting/patches/2026-06-25-event-chunks-insert-rls.sql \
             in the Supabase SQL editor (or re-run docs/self-hosting/supabase.sql), then push \
             again."
        );
    }
    anyhow::Error::new(error).context(format!(
        "failed to insert Brick event chunks for event {event_id} into Supabase"
    ))
}

fn event_without_chunks(event: &TraceEvent) -> TraceEvent {
    let mut event = event.clone();
    if let Some(payload) = event.payload.as_object_mut() {
        payload.remove("normalized_chunks");
    }
    event
}

#[derive(Debug, Deserialize)]
struct EventRow {
    event: TraceEvent,
}

#[derive(Debug, Deserialize)]
struct EventChunkRawRow {
    event_id: uuid::Uuid,
    raw: serde_json::Value,
}

#[derive(Debug, Deserialize)]
struct EventIdRow {
    #[allow(dead_code)]
    event_id: uuid::Uuid,
}

pub fn is_supabase_remote(remote: &str) -> bool {
    remote == "supabase" || remote.starts_with("supabase://")
}

fn ensure_repo_id(repo_id: Option<&str>) -> Result<&str> {
    match repo_id.filter(|repo_id| !repo_id.trim().is_empty()) {
        Some(repo_id) => Ok(repo_id),
        None => bail!("Supabase sync requires --repo-id or a git repository root"),
    }
}

#[cfg(test)]
mod tests {
    use brick_protocol::{
        ActorRef, ActorType, MissionCreatedPayload, MissionId, MissionStatus, OrgId, ProjectId,
        SourceSessionObservedPayload,
    };
    use std::str::FromStr;

    use super::*;

    fn event() -> TraceEvent {
        let mut event = TraceEvent::mission_created(
            ActorRef {
                actor_type: ActorType::Human,
                actor_id: "tester".to_string(),
                display_name: None,
            },
            MissionId::new(),
            MissionCreatedPayload {
                project_id: ProjectId::new(),
                title: "Sync payload".to_string(),
                description: None,
                status: MissionStatus::Planned,
                repo_context_id: None,
            },
        )
        .expect("build event");
        event.repo_id = Some("repo-a".to_string());
        event.org_id = Some(OrgId::from_str("org-a").expect("org id"));
        event
    }

    fn source_session_event() -> TraceEvent {
        let actor = ActorRef {
            actor_type: ActorType::Agent,
            actor_id: "orgii".to_string(),
            display_name: None,
        };
        let mut event = TraceEvent::source_session_observed(
            actor,
            SourceSessionObservedPayload {
                source_id: "orgii".to_string(),
                external_session_id: "session-a".to_string(),
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
                normalized_chunks: vec![serde_json::json!({ "text": "hello" })],
            },
        )
        .expect("build source session event");
        event.repo_id = Some("repo-a".to_string());
        event.org_id = Some(OrgId::from_str("org-a").expect("org id"));
        event
    }

    #[test]
    fn recognizes_supabase_remote_aliases() {
        assert!(is_supabase_remote("supabase"));
        assert!(is_supabase_remote("supabase://default"));
        assert!(!is_supabase_remote("https://example.com"));
    }

    #[test]
    fn event_row_scopes_by_repo_and_org() {
        let event = event();
        let row = InsertEventRow::from_event(&event).expect("row");
        assert_eq!(row.repo_id, "repo-a");
        assert_eq!(row.org_id, "org-a");
        assert_eq!(row.event_id, event.event_id);
    }

    #[test]
    fn supabase_event_row_strips_chunks_from_canonical_event() {
        let event = source_session_event();
        let row = InsertEventRow::from_event(&event).expect("row");
        assert!(event.payload["normalized_chunks"].is_array());
        assert!(row.event.payload.get("normalized_chunks").is_none());
        let rows = EventChunkRows::from_event(&event).expect("chunk rows");
        assert_eq!(rows.rows.len(), 1);
        assert_eq!(rows.rows[0].raw["text"], "hello");
    }

    #[test]
    fn supabase_remote_trims_url() {
        let remote = SupabaseRemote::new("https://example.supabase.co///", "anon");
        assert_eq!(remote.url, "https://example.supabase.co");
    }
}
