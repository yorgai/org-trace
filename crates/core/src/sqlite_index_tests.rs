//! Tests for the rebuildable SQLite query cache.

use std::fs;

use brick_protocol::{
    ActorRef, ActorType, ArtifactAttachmentUploadedPayload, ArtifactCreatedPayload,
    ArtifactFileRefRecordedPayload, ArtifactId, ArtifactKind, ArtifactUpdatedPayload, AttachmentId,
    DiffCapturedPayload, DiffFileChange, DiffFileChangeKind, DiffTarget, EvidenceAvailability,
    FileRefId, LogRefId, MissionCreatedPayload, MissionId, MissionStatus, ProjectId, SessionId,
    SessionLogFormat, SessionLogUploadedPayload, SessionSource, SessionStartedPayload, TraceEvent,
};
use chrono::Utc;

use crate::{
    query_sqlite_artifacts, query_sqlite_file_session_blame, query_sqlite_sessions,
    rebuild_sqlite_index, sqlite_index_status, SqliteArtifactQuery, SqliteFileSessionBlameQuery,
    SqliteSessionQuery, TraceIndex, SQLITE_INDEX_FILE, SQLITE_INDEX_SCHEMA_VERSION,
};

fn actor() -> ActorRef {
    ActorRef {
        actor_type: ActorType::Agent,
        actor_id: "agent-1".to_string(),
        display_name: None,
    }
}

fn temp_sqlite_path(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "brick-sqlite-test-{name}-{}",
        Utc::now().timestamp_nanos_opt().unwrap_or_default()
    ));
    fs::create_dir_all(&dir).expect("create temp sqlite dir");
    dir.join(SQLITE_INDEX_FILE)
}

#[test]
fn rebuilds_and_queries_sqlite_cache() {
    let path = temp_sqlite_path("query");
    let project_id = ProjectId::new();
    let mission_id = MissionId::new();
    let session_id = SessionId::new();
    let artifact_id = ArtifactId::new();
    let attachment_id = AttachmentId::new();
    let log_ref_id = LogRefId::new();
    let diff_artifact_id = ArtifactId::new();
    let events = vec![
        TraceEvent::mission_created(
            actor(),
            mission_id.clone(),
            MissionCreatedPayload {
                project_id: project_id.clone(),
                title: "Phase 6".to_string(),
                description: None,
                status: MissionStatus::Planned,
                repo_context_id: None,
            },
        )
        .expect("mission event"),
        TraceEvent::session_started(
            actor(),
            session_id.clone(),
            Some(mission_id.clone()),
            SessionStartedPayload {
                session_name: Some("SQLite work".to_string()),
                source: SessionSource {
                    app_id: Some("cursor".to_string()),
                    app_session_id: Some("native-1".to_string()),
                    app_session_name: Some("Phase 6 chat".to_string()),
                    runtime_id: Some("runtime-1".to_string()),
                },
                repo_context_id: None,
            },
        )
        .expect("session event"),
        TraceEvent::artifact_created(
            actor(),
            artifact_id.clone(),
            Some(mission_id.clone()),
            Some(session_id.clone()),
            ArtifactCreatedPayload {
                artifact_kind: ArtifactKind::Decision,
                title: "Use SQLite cache".to_string(),
                body: Some("Original body".to_string()),
                repo_context_id: None,
            },
        )
        .expect("artifact event"),
        TraceEvent::artifact_updated(
            actor(),
            artifact_id.clone(),
            Some(session_id.clone()),
            ArtifactUpdatedPayload {
                title: Some("Use updated SQLite cache".to_string()),
                body: Some("Updated body".to_string()),
                artifact_kind: Some(ArtifactKind::Review),
                repo_context_id: None,
            },
        )
        .expect("artifact update event"),
        TraceEvent::artifact_file_ref_recorded(
            actor(),
            artifact_id.clone(),
            Some(session_id.clone()),
            ArtifactFileRefRecordedPayload {
                file_ref_id: FileRefId::new(),
                path: "crates/core/src/sqlite_index.rs".to_string(),
                repo_context_id: None,
            },
        )
        .expect("file event"),
        TraceEvent::artifact_attachment_uploaded(
            actor(),
            artifact_id.clone(),
            Some(session_id.clone()),
            ArtifactAttachmentUploadedPayload {
                attachment_id: attachment_id.clone(),
                name: "report.txt".to_string(),
                original_path: "/tmp/report.txt".to_string(),
                content_type: Some("text/plain".to_string()),
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                size_bytes: 5,
                storage_uri: "brick-blob://sha256/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                external_uri: Some("file:///tmp/report.txt".to_string()),
                availability: EvidenceAvailability::LocalBlob,
                repo_context_id: None,
            },
        )
        .expect("attachment event"),
        TraceEvent::session_log_uploaded(
            actor(),
            session_id.clone(),
            SessionLogUploadedPayload {
                log_ref_id: log_ref_id.clone(),
                original_path: "/tmp/session.jsonl".to_string(),
                format: SessionLogFormat::Jsonl,
                source: "cursor".to_string(),
                sha256: "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                size_bytes: 5,
                storage_uri: "brick-blob://sha256/2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824".to_string(),
                local_path: "/tmp/blob".to_string(),
                external_uri: Some("file:///tmp/session.jsonl".to_string()),
                availability: EvidenceAvailability::LocalBlob,
                repo_context_id: None,
            },
        )
        .expect("session log event"),
        TraceEvent::artifact_created(
            actor(),
            diff_artifact_id.clone(),
            Some(mission_id.clone()),
            Some(session_id.clone()),
            ArtifactCreatedPayload {
                artifact_kind: ArtifactKind::Patch,
                title: "Captured diff".to_string(),
                body: None,
                repo_context_id: None,
            },
        )
        .expect("diff artifact event"),
        TraceEvent::diff_captured(
            actor(),
            diff_artifact_id.clone(),
            Some(session_id.clone()),
            Some(mission_id.clone()),
            DiffCapturedPayload {
                diff_target: DiffTarget::Working,
                base_commit: None,
                head_commit: None,
                patch_id: Some("patch-1".to_string()),
                summary_hash: "summary-1".to_string(),
                file_changes: vec![DiffFileChange {
                    path: "crates/core/src/file_session_blame.rs".to_string(),
                    old_path: None,
                    change_kind: DiffFileChangeKind::Modified,
                    additions: Some(12),
                    deletions: Some(3),
                }],
                repo_context_id: None,
            },
        )
        .expect("diff captured event"),
    ];
    let index = TraceIndex::build(&events).expect("build trace index");

    rebuild_sqlite_index(&path, &events, &index).expect("rebuild sqlite index");
    let status = sqlite_index_status(&path).expect("sqlite status");
    assert!(status.exists);
    assert_eq!(status.schema_version, Some(SQLITE_INDEX_SCHEMA_VERSION));
    assert_eq!(status.event_count, 9);
    assert_eq!(status.session_log_count, 1);

    let sessions = query_sqlite_sessions(
        &path,
        &SqliteSessionQuery {
            app_id: Some("cursor".to_string()),
            actor_id: Some("agent-1".to_string()),
            runtime_id: Some("runtime-1".to_string()),
            limit: 20,
        },
    )
    .expect("query sessions");
    assert_eq!(sessions.len(), 1);
    assert_eq!(sessions[0].session_id, session_id.to_string());
    assert_eq!(sessions[0].mission_ids, vec![mission_id.to_string()]);
    assert_eq!(sessions[0].log_ref_ids, vec![log_ref_id.to_string()]);

    let artifacts = query_sqlite_artifacts(
        &path,
        &SqliteArtifactQuery {
            session_id: Some(session_id.to_string()),
            mission_id: Some(mission_id.to_string()),
            limit: 20,
        },
    )
    .expect("query artifacts");
    assert_eq!(artifacts.len(), 2);
    let artifact = artifacts
        .iter()
        .find(|record| record.artifact_id == artifact_id.to_string())
        .expect("original artifact row");
    assert_eq!(artifact.artifact_kind.as_deref(), Some("review"));
    assert_eq!(artifact.title.as_deref(), Some("Use updated SQLite cache"));
    assert_eq!(artifact.body.as_deref(), Some("Updated body"));
    assert_eq!(
        artifact.file_paths,
        vec!["crates/core/src/sqlite_index.rs".to_string()]
    );
    assert_eq!(artifact.attachments.len(), 1);
    assert_eq!(
        artifact.attachments[0].attachment_id,
        attachment_id.to_string()
    );
    assert_eq!(artifact.attachments[0].size_bytes, 5);
    assert_eq!(
        artifact.attachments[0].content_type.as_deref(),
        Some("text/plain")
    );

    let blame = query_sqlite_file_session_blame(
        &path,
        &SqliteFileSessionBlameQuery {
            file_path: "crates/core/src/file_session_blame.rs".to_string(),
            limit: 20,
        },
    )
    .expect("query file blame");
    assert_eq!(blame.len(), 1);
    assert_eq!(blame[0].session_id.as_deref(), Some(session_id.as_str()));
    assert_eq!(blame[0].app_id.as_deref(), Some("cursor"));
    assert_eq!(blame[0].actor_id.as_deref(), Some("agent-1"));
    assert!(blame.iter().any(|row| row.lines_added == Some(12)
        && row.lines_removed == Some(3)
        && row.files_changed == Some(1)
        && row.evidence_kind.as_str() == "runtime_event"
        && row.confidence.as_deref() == Some("explicit")));
    assert!(blame.iter().any(|row| row
        .source_pointer
        .as_ref()
        .is_some_and(|pointer| pointer.get("diff_id").is_some())));

    let folder_blame = query_sqlite_file_session_blame(
        &path,
        &SqliteFileSessionBlameQuery {
            file_path: "crates/core/src".to_string(),
            limit: 20,
        },
    )
    .expect("query folder blame");
    assert_eq!(folder_blame.len(), 2);
    assert!(folder_blame
        .iter()
        .any(|row| row.file_path == "crates/core/src/file_session_blame.rs"));
    assert!(folder_blame
        .iter()
        .any(|row| row.file_path == "crates/core/src/sqlite_index.rs"));

    let boundary_blame = query_sqlite_file_session_blame(
        &path,
        &SqliteFileSessionBlameQuery {
            file_path: "crates/core/sr".to_string(),
            limit: 20,
        },
    )
    .expect("query folder boundary blame");
    assert!(boundary_blame.is_empty());
}
