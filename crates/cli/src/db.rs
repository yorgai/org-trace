//! Read-only SQLite cache command handlers.
//!
//! The `db` commands expose typed queries over a rebuildable SQLite cache. They
//! never mutate provenance source data and avoid arbitrary SQL execution.

use anyhow::Result;
use brick_core::{
    query_sqlite_artifacts, query_sqlite_sessions, LocalStore, SqliteArtifactQuery,
    SqliteArtifactRecord, SqliteSessionQuery, SqliteSessionRecord,
};

use crate::args::DbCommand;

/// Executes SQLite cache maintenance and typed query subcommands.
pub fn handle_db(command: DbCommand, store: &LocalStore) -> Result<()> {
    match command {
        DbCommand::Rebuild => {
            let status = store.rebuild_sqlite_index()?;
            println!("sqlite_rebuilt=true");
            print_status_fields(&status);
        }
        DbCommand::Status => {
            let status = store.sqlite_index_status()?;
            print_status_fields(&status);
        }
        DbCommand::Sessions {
            limit,
            app_id,
            actor_id,
            runtime_id,
        } => {
            let records = query_sqlite_sessions(
                &store.sqlite_index_path(),
                &SqliteSessionQuery {
                    app_id,
                    actor_id,
                    runtime_id,
                    limit,
                },
            )?;
            println!("session_count={}", records.len());
            for record in &records {
                print_session_record(record);
            }
        }
        DbCommand::Artifacts {
            limit,
            session,
            mission,
        } => {
            let records = query_sqlite_artifacts(
                &store.sqlite_index_path(),
                &SqliteArtifactQuery {
                    session_id: session,
                    mission_id: mission,
                    limit,
                },
            )?;
            println!("artifact_count={}", records.len());
            for record in &records {
                print_artifact_record(record);
            }
        }
    }
    Ok(())
}

fn print_status_fields(status: &brick_core::SqliteIndexStatus) {
    println!("sqlite_exists={}", status.exists);
    println!("sqlite_path={}", status.path);
    println!(
        "sqlite_schema_version={}",
        status
            .schema_version
            .map(|value| value.to_string())
            .unwrap_or_default()
    );
    println!("sqlite_event_count={}", status.event_count);
    println!("sqlite_mission_count={}", status.mission_count);
    println!("sqlite_session_count={}", status.session_count);
    println!("sqlite_artifact_count={}", status.artifact_count);
    println!("sqlite_file_count={}", status.file_count);
    println!("sqlite_session_log_count={}", status.session_log_count);
    println!("sqlite_diff_count={}", status.diff_count);
    println!(
        "sqlite_rebuilt_at={}",
        status
            .rebuilt_at
            .map(|value| value.to_rfc3339())
            .unwrap_or_default()
    );
}

fn print_session_record(record: &SqliteSessionRecord) {
    println!(
        "session={} mission={} artifact={} log_count={} log_refs={} actor_id={} actor_type={} app_id={} app_session_id={} app_session_name={} runtime_id={} started_at={} last_event_at={}",
        record.session_id,
        record.mission_ids.join(","),
        record.artifact_ids.join(","),
        record.log_ref_ids.len(),
        record.log_ref_ids.join(","),
        record.actor_id.as_deref().unwrap_or(""),
        record.actor_type.as_deref().unwrap_or(""),
        record.app_id.as_deref().unwrap_or(""),
        record.app_session_id.as_deref().unwrap_or(""),
        record.app_session_name.as_deref().unwrap_or(""),
        record.runtime_id.as_deref().unwrap_or(""),
        record.started_at,
        record.last_event_at,
    );
}

fn print_artifact_record(record: &SqliteArtifactRecord) {
    println!(
        "artifact={} kind={} title={} body={} mission={} session={} files={} attachment_count={} attachments={} diff_count={} diffs={} created_at={} last_event_at={}",
        record.artifact_id,
        record.artifact_kind.as_deref().unwrap_or(""),
        record.title.as_deref().unwrap_or(""),
        record.body.as_deref().unwrap_or(""),
        record.mission_ids.join(","),
        record.session_ids.join(","),
        record.file_paths.join(","),
        record.attachments.len(),
        record
            .attachments
            .iter()
            .map(|attachment| format!(
                "{}:{}:{}:{}",
                attachment.attachment_id,
                attachment.sha256,
                attachment.size_bytes,
                attachment.storage_uri
            ))
            .collect::<Vec<_>>()
            .join(","),
        record.diffs.len(),
        record
            .diffs
            .iter()
            .map(|diff| format!(
                "{}:{}:{}:{}:{}:{}",
                diff.diff_id,
                diff.diff_target,
                diff.summary_hash,
                diff.file_count,
                diff.additions,
                diff.deletions
            ))
            .collect::<Vec<_>>()
            .join(","),
        record.created_at,
        record.last_event_at,
    );
}
