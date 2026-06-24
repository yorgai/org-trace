use anyhow::{bail, Context, Result};
use brick_protocol::TraceEvent;
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
        Ok(ListEventsResponse::all(
            rows.into_iter().map(|row| row.event).collect(),
        ))
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
struct InsertEventRow<'a> {
    event_id: uuid::Uuid,
    repo_id: &'a str,
    org_id: String,
    occurred_at: chrono::DateTime<chrono::Utc>,
    event: &'a TraceEvent,
}

impl<'a> InsertEventRow<'a> {
    fn from_event(event: &'a TraceEvent) -> Result<Self> {
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
        Ok(Self {
            event_id: event.event_id,
            repo_id,
            org_id,
            occurred_at: event.occurred_at,
            event,
        })
    }
}

#[derive(Debug, Deserialize)]
struct EventRow {
    event: TraceEvent,
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
    fn supabase_remote_trims_url() {
        let remote = SupabaseRemote::new("https://example.supabase.co///", "anon");
        assert_eq!(remote.url, "https://example.supabase.co");
    }
}
