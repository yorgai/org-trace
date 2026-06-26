//! Synthesizes a `source.session_observed` `TraceEvent` from an indexed
//! `SourceSessionRecord`.
//!
//! This is the single mapping point for source-session event ids and payloads.
//! Source profiles and scan watermarks live in `metadata.sqlite`, but the durable
//! local event truth is `brick_events` / `brick_event_chunks` in `brick.sqlite`.
//! History refresh uses this builder, then `LocalEventStore` compacts the event
//! JSON and stores chunks separately.
//!
//! The event id is a deterministic UUIDv5 over
//! `source_id + external_session_id + source_fingerprint + parser_version`, so
//! re-synthesizing the same indexed session always yields the same id and the
//! remote upserts it idempotently. A changed provider source file changes the
//! fingerprint and therefore the id (a genuinely new observation).

use brick_protocol::{ActorRef, SessionId, SourceSessionObservedPayload, TraceEvent};
use std::str::FromStr;
use uuid::Uuid;

use crate::{ActivityChunk, SourceSessionRecord};

/// Deterministic UUIDv5 id for the observation of one indexed source session.
/// Kept stable across re-indexing so the remote dedupes by event id.
pub fn source_session_event_id(record: &SourceSessionRecord) -> Uuid {
    Uuid::new_v5(
        &Uuid::NAMESPACE_URL,
        format!(
            "brick:source-session-observed-v2:{}:{}:{}:{}",
            record.source_id,
            record.external_session_id,
            record.source_fingerprint.as_deref().unwrap_or_default(),
            record.parser_version.as_deref().unwrap_or_default(),
        )
        .as_bytes(),
    )
}

/// The `session_id` carried on the event: the provider's external session id
/// when it is UUID-shaped (Cursor/ORGII/Codex ids are), else a fresh id.
fn session_id_for_record(record: &SourceSessionRecord) -> SessionId {
    SessionId::from_str(&record.external_session_id).unwrap_or_else(|_| SessionId::new())
}

/// Builds the canonical `source.session_observed` event for one indexed
/// source-session row, tagged with `repo_id` so the push path can scope it.
///
/// This variant carries no chunks; callers that have provider transcript chunks
/// should use `build_source_session_event_with_chunks` before appending to the
/// local event store.
pub fn build_source_session_event(
    actor: ActorRef,
    record: &SourceSessionRecord,
    repo_id: &str,
) -> serde_json::Result<TraceEvent> {
    build_source_session_event_with_chunks(actor, record, repo_id, &[])
}

pub fn build_source_session_event_with_chunks(
    actor: ActorRef,
    record: &SourceSessionRecord,
    repo_id: &str,
    chunks: &[ActivityChunk],
) -> serde_json::Result<TraceEvent> {
    let mut payload = source_session_payload(record);
    payload.normalized_chunks = chunks
        .iter()
        .map(serde_json::to_value)
        .collect::<serde_json::Result<Vec<_>>>()?;
    let mut event = TraceEvent::source_session_observed(actor, payload)?;
    event.event_id = source_session_event_id(record);
    event.session_id = Some(session_id_for_record(record));
    event.repo_id = Some(repo_id.to_string());
    Ok(event)
}

fn source_session_payload(record: &SourceSessionRecord) -> SourceSessionObservedPayload {
    SourceSessionObservedPayload {
        source_id: record.source_id.clone(),
        external_session_id: record.external_session_id.clone(),
        title: record.title.clone(),
        name: record.name.clone(),
        source_path: record
            .source_path
            .as_ref()
            .map(|path| path.display().to_string()),
        source_uri: record.source_uri.clone(),
        source_mtime: record.source_mtime.map(|time| time.to_rfc3339()),
        source_size: record.source_size,
        source_fingerprint: record.source_fingerprint.clone(),
        parser_version: record.parser_version.clone(),
        session_created_at: record.session_created_at.map(|time| time.to_rfc3339()),
        session_updated_at: record.session_updated_at.map(|time| time.to_rfc3339()),
        model: record.model.clone(),
        input_tokens: record.input_tokens,
        output_tokens: record.output_tokens,
        repo_path: record
            .repo_path
            .as_ref()
            .map(|path| path.display().to_string()),
        branch: record.branch.clone(),
        files_changed: record.files_changed,
        lines_added: record.lines_added,
        lines_removed: record.lines_removed,
        touched_files: record
            .touched_files_json
            .as_ref()
            .and_then(|value| value.as_array())
            .map(|files| {
                files
                    .iter()
                    .filter_map(|file| file.as_str().map(ToOwned::to_owned))
                    .collect()
            })
            .unwrap_or_default(),
        metadata_json: record.metadata_json.clone(),
        normalized_chunks: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use brick_protocol::ActorType;
    use chrono::Utc;
    use std::path::PathBuf;

    fn record(external_id: &str, fingerprint: &str) -> SourceSessionRecord {
        let now = Utc::now();
        SourceSessionRecord {
            source_id: "orgii".to_string(),
            external_session_id: external_id.to_string(),
            title: Some("t".to_string()),
            name: None,
            source_path: None,
            source_uri: None,
            source_mtime: None,
            source_size: None,
            source_fingerprint: Some(fingerprint.to_string()),
            parser_version: Some("v1".to_string()),
            session_created_at: None,
            session_updated_at: None,
            model: None,
            input_tokens: None,
            output_tokens: None,
            repo_path: Some(PathBuf::from("/tmp/repo")),
            branch: None,
            files_changed: None,
            lines_added: None,
            lines_removed: None,
            touched_files_json: None,
            listable: true,
            discovered_at: now,
            last_seen_at: now,
            created_at: now,
            updated_at: now,
            metadata_json: None,
        }
    }

    fn actor() -> ActorRef {
        ActorRef {
            actor_type: ActorType::Agent,
            actor_id: "orgii".to_string(),
            display_name: None,
        }
    }

    #[test]
    fn event_id_is_stable_for_same_inputs_and_varies_with_fingerprint() {
        let a = record("s1", "fp-1");
        assert_eq!(source_session_event_id(&a), source_session_event_id(&a));
        // Different session id -> different event id.
        assert_ne!(
            source_session_event_id(&a),
            source_session_event_id(&record("s2", "fp-1"))
        );
        // Re-indexed with a changed source fingerprint -> different event id.
        assert_ne!(
            source_session_event_id(&a),
            source_session_event_id(&record("s1", "fp-2"))
        );
    }

    #[test]
    fn build_event_uses_canonical_id_and_repo_id() {
        let r = record("s1", "fp-1");
        let event = build_source_session_event(actor(), &r, "repo-xyz").expect("build");
        assert_eq!(event.event_id, source_session_event_id(&r));
        assert_eq!(event.repo_id.as_deref(), Some("repo-xyz"));
        // Chunks are hydrated separately at push time, never inline here. An
        // empty chunk list is omitted from the serialized payload entirely.
        assert!(event.payload.get("normalized_chunks").is_none());
    }
}
