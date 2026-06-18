//! Read-only context and session discovery commands.
//!
//! These handlers report the effective local context and indexed session records
//! without appending events. Output remains compact key-value text for agents.

use std::str::FromStr;

use anyhow::{anyhow, Context, Result};
use brick_core::{
    query_indexed_sessions, IdentityOverrides, IndexedSession, LocalStore, SessionQuery,
    SourceProfile,
};
use brick_protocol::{ActorType, MissionId, SessionId};

use crate::args::{ContextCommand, IdentityArgs, SessionCommand};

/// Executes read-only context inspection commands.
pub fn handle_context(
    command: ContextCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    source_profile: Option<&SourceProfile>,
) -> Result<()> {
    match command {
        ContextCommand::Show => {
            let identity = resolve_cli_identity(store, identity_args, None, None, source_profile)?;
            print_resolved_identity(&identity);
        }
    }
    Ok(())
}

/// Executes read-only session discovery commands.
pub fn handle_session_read(
    command: &SessionCommand,
    identity_args: &IdentityArgs,
    store: &LocalStore,
    source_profile: Option<&SourceProfile>,
) -> Result<bool> {
    match command {
        SessionCommand::Current => {
            let current = store
                .read_current_context()?
                .ok_or_else(|| anyhow!("no current session exists in the effective local store"))?;
            let session_id = current
                .session_id
                .ok_or_else(|| anyhow!("no current session exists in the effective local store"))?;
            println!("session_id={session_id}");
            println!(
                "mission_id={}",
                current
                    .mission_id
                    .as_ref()
                    .map(ToString::to_string)
                    .unwrap_or_default()
            );
            if let Some(actor) = current.actor.as_ref() {
                println!("actor_id={}", actor.actor_id);
                println!("actor_type={}", format_actor_type(actor.actor_type));
                println!(
                    "actor_display_name={}",
                    actor.display_name.as_deref().unwrap_or("")
                );
            } else {
                println!("actor_id=");
                println!("actor_type=");
                println!("actor_display_name=");
            }
            println!("app_id={}", current.app_id.as_deref().unwrap_or(""));
            println!(
                "app_session_id={}",
                current.app_session_id.as_deref().unwrap_or("")
            );
            println!(
                "app_session_name={}",
                current.app_session_name.as_deref().unwrap_or("")
            );
            println!("runtime_id={}", current.runtime_id.as_deref().unwrap_or(""));
            Ok(true)
        }
        SessionCommand::List {
            limit,
            app_id,
            actor_id,
            runtime_id,
        } => {
            let index = store.load_or_rebuild_index()?;
            let query = SessionQuery {
                app_id: app_id.clone(),
                actor_id: actor_id.clone(),
                runtime_id: runtime_id.clone(),
                ..SessionQuery::default()
            };
            let sessions = query_indexed_sessions(&index, &query);
            println!("session_count={}", sessions.len().min(*limit));
            for session in sessions.into_iter().take(*limit) {
                print_session_summary(session);
            }
            Ok(true)
        }
        SessionCommand::Find {
            app_id,
            app_session_id,
            app_session_name,
            runtime_id,
            actor_id,
        } => {
            let index = store.load_or_rebuild_index()?;
            let query = SessionQuery {
                app_id: app_id.clone(),
                app_session_id: app_session_id.clone(),
                app_session_name: app_session_name.clone(),
                runtime_id: runtime_id.clone(),
                actor_id: actor_id.clone(),
            };
            let sessions = query_indexed_sessions(&index, &query);
            if sessions.is_empty() {
                return Err(anyhow!("no matching sessions found"));
            }
            println!("session_count={}", sessions.len());
            for session in sessions {
                print_session_summary(session);
            }
            Ok(true)
        }
        SessionCommand::Start { .. }
        | SessionCommand::Link { .. }
        | SessionCommand::Show { .. } => {
            let _ = (identity_args, source_profile);
            Ok(false)
        }
    }
}

pub(crate) fn resolve_cli_identity(
    store: &LocalStore,
    identity_args: &IdentityArgs,
    mission_id: Option<MissionId>,
    session_id: Option<SessionId>,
    source_profile: Option<&SourceProfile>,
) -> Result<brick_core::ResolvedIdentity> {
    let current = store.read_current_context()?;
    let overrides = IdentityOverrides {
        actor_id: identity_args.actor_id.clone(),
        actor_type: identity_args
            .actor_type
            .as_deref()
            .map(ActorType::from_str)
            .transpose()
            .context("invalid --actor-type")?,
        runtime_id: identity_args.runtime_id.clone(),
        session_id: session_id.or(parse_optional_id::<SessionId>(
            identity_args.session.as_deref(),
            "session",
        )?),
        app_id: identity_args.app_id.clone(),
        app_session_id: identity_args.app_session_id.clone(),
        app_session_name: identity_args.app_session_name.clone(),
        mission_id: mission_id.or(parse_optional_id::<MissionId>(
            identity_args.mission.as_deref(),
            "mission",
        )?),
    };

    brick_core::resolve_identity_with_profile(current.as_ref(), overrides, source_profile)
}

pub(crate) fn parse_optional_id<T>(value: Option<&str>, label: &str) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    value
        .map(|raw| {
            raw.parse::<T>()
                .with_context(|| format!("invalid {label} id"))
        })
        .transpose()
}

pub(crate) fn format_actor_type(actor_type: ActorType) -> &'static str {
    match actor_type {
        ActorType::Human => "human",
        ActorType::Agent => "agent",
        ActorType::System => "system",
    }
}

fn print_resolved_identity(identity: &brick_core::ResolvedIdentity) {
    println!("session_id={}", identity.session_id);
    println!(
        "mission_id={}",
        identity
            .mission_id
            .as_ref()
            .map(ToString::to_string)
            .unwrap_or_default()
    );
    println!("actor_id={}", identity.actor.actor_id);
    println!(
        "actor_type={}",
        format_actor_type(identity.actor.actor_type)
    );
    println!(
        "actor_display_name={}",
        identity.actor.display_name.as_deref().unwrap_or("")
    );
    println!(
        "app_id={}",
        identity.session_source.app_id.as_deref().unwrap_or("")
    );
    println!(
        "app_session_id={}",
        identity
            .session_source
            .app_session_id
            .as_deref()
            .unwrap_or("")
    );
    println!(
        "app_session_name={}",
        identity
            .session_source
            .app_session_name
            .as_deref()
            .unwrap_or("")
    );
    println!(
        "runtime_id={}",
        identity.runtime_id.as_deref().unwrap_or("")
    );
}

fn print_session_summary(session: &IndexedSession) {
    println!(
        "session={} mission={} actor_id={} actor_type={} app_id={} app_session_id={} app_session_name={} runtime_id={} last_event_at={}",
        session.session_id,
        session
            .mission_ids
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(","),
        session.actor_id.as_deref().unwrap_or(""),
        session.actor_type.map(format_actor_type).unwrap_or(""),
        session.source.app_id.as_deref().unwrap_or(""),
        session.source.app_session_id.as_deref().unwrap_or(""),
        session.source.app_session_name.as_deref().unwrap_or(""),
        session.source.runtime_id.as_deref().unwrap_or(""),
        session.last_event_at,
    );
}
