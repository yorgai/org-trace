//! `brick announce` — the cross-session bulletin board CLI surface.
//!
//! Thin presentation over [`brick_core::AnnouncementStore`]. Publishers default
//! their identity from the global `--source` / `--session` flags so an agent that
//! is already running under a Brick source need only supply scope + message.

use anyhow::{anyhow, Result};
use brick_core::{AnnouncementStore, NewAnnouncement};
use chrono::Duration;
use serde_json::json;

use crate::args::AnnounceCommand;
use crate::history::{ensure_json, print_json};

pub fn handle_announce(
    command: AnnounceCommand,
    global_source: Option<&str>,
    global_session: Option<&str>,
) -> Result<()> {
    let store = AnnouncementStore::open_global()?;
    match command {
        AnnounceCommand::Claim {
            scope,
            message,
            source,
            session,
            work_dir,
            ttl_minutes,
            format,
        } => {
            ensure_json(format);
            let source_id = resolve(source.as_deref(), global_source, "source")?;
            let session_id = resolve(session.as_deref(), global_session, "session")?;
            let work_dir = work_dir.or_else(default_work_dir);
            let ttl = ttl_minutes.map(Duration::minutes);
            let announcement = store.publish(NewAnnouncement {
                source_id,
                session_id,
                scope,
                message,
                work_dir,
                ttl,
            })?;
            print_json(&announcement)
        }
        AnnounceCommand::Release {
            scope,
            source,
            session,
            format,
        } => {
            ensure_json(format);
            let source_id = resolve(source.as_deref(), global_source, "source")?;
            let session_id = resolve(session.as_deref(), global_session, "session")?;
            let removed = store.release(&source_id, &session_id, scope.as_deref())?;
            print_json(&json!({ "released": removed }))
        }
        AnnounceCommand::List { path, format } => {
            ensure_json(format);
            let announcements = match path {
                Some(path) => store.matching(&path)?,
                None => store.list_active()?,
            };
            print_json(&json!({
                "count": announcements.len(),
                "announcements": announcements,
            }))
        }
    }
}

/// Resolves an identity field from the per-command flag, then the global flag,
/// erroring with a clear message naming the field when neither is set.
fn resolve(specific: Option<&str>, global: Option<&str>, field: &str) -> Result<String> {
    specific
        .or(global)
        .map(ToOwned::to_owned)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            anyhow!("missing {field}: pass --{field} or the global --{field} flag so the claim can be attributed")
        })
}

fn default_work_dir() -> Option<String> {
    std::env::current_dir()
        .ok()
        .map(|path| path.display().to_string())
}
