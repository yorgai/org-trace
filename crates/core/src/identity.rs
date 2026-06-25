//! Identity resolution for local CLI and agent capture.
//!
//! The resolution order is explicit overrides, environment variables, persisted
//! current context, then local fallbacks. This lets agents pass exact runtime
//! identifiers while keeping manual CLI use lightweight.

use std::str::FromStr;

use anyhow::{Context, Result};
use brick_protocol::{ActorRef, ActorType, MissionId, SessionId, SessionSource};
use serde::{Deserialize, Serialize};

use crate::SourceProfile;

/// Persisted local context reused when commands omit identity flags.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct CurrentContext {
    pub actor: Option<ActorRef>,
    pub runtime_id: Option<String>,
    pub session_id: Option<SessionId>,
    pub app_id: Option<String>,
    pub app_session_id: Option<String>,
    pub app_session_name: Option<String>,
    pub mission_id: Option<MissionId>,
}

/// Explicit identity values supplied by CLI flags or callers.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct IdentityOverrides {
    pub actor_id: Option<String>,
    pub actor_type: Option<ActorType>,
    pub runtime_id: Option<String>,
    pub session_id: Option<SessionId>,
    pub app_id: Option<String>,
    pub app_session_id: Option<String>,
    pub app_session_name: Option<String>,
    pub mission_id: Option<MissionId>,
}

/// Complete identity bundle attached to newly recorded events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedIdentity {
    pub actor: ActorRef,
    pub runtime_id: Option<String>,
    pub session_id: SessionId,
    pub session_source: SessionSource,
    pub mission_id: Option<MissionId>,
}

impl ResolvedIdentity {
    /// Converts a resolved identity into the persisted current-context shape.
    pub fn current_context(&self) -> CurrentContext {
        CurrentContext {
            actor: Some(self.actor.clone()),
            runtime_id: self.runtime_id.clone(),
            session_id: Some(self.session_id.clone()),
            app_id: self.session_source.app_id.clone(),
            app_session_id: self.session_source.app_session_id.clone(),
            app_session_name: self.session_source.app_session_name.clone(),
            mission_id: self.mission_id.clone(),
        }
    }
}

/// Resolves event identity using overrides, environment, current context, and fallbacks.
pub fn resolve_identity(
    current: Option<&CurrentContext>,
    overrides: IdentityOverrides,
) -> Result<ResolvedIdentity> {
    resolve_identity_with_profile(current, overrides, None)
}

/// Resolves event identity with source profile defaults between environment and context.
pub fn resolve_identity_with_profile(
    current: Option<&CurrentContext>,
    overrides: IdentityOverrides,
    source_profile: Option<&SourceProfile>,
) -> Result<ResolvedIdentity> {
    let env_actor_id = std::env::var("BRICK_ACTOR_ID").ok();
    let env_actor_type = std::env::var("BRICK_ACTOR_TYPE")
        .ok()
        .map(|value| ActorType::from_str(&value))
        .transpose()
        .context("invalid BRICK_ACTOR_TYPE")?;

    let current_actor = current.and_then(|context| context.actor.clone());
    let profile_actor_id = source_profile.and_then(|profile| profile.actor_id.clone());
    let profile_actor_type = source_profile.and_then(|profile| profile.actor_type);
    let actor_id = overrides
        .actor_id
        .or(env_actor_id)
        .or(profile_actor_id)
        .or_else(|| current_actor.as_ref().map(|actor| actor.actor_id.clone()))
        .or_else(|| std::env::var("USER").ok())
        .unwrap_or_else(|| "unknown".to_string());
    let actor_type = overrides
        .actor_type
        .or(env_actor_type)
        .or(profile_actor_type)
        .or_else(|| current_actor.as_ref().map(|actor| actor.actor_type))
        .unwrap_or(ActorType::Human);

    let runtime_id = overrides
        .runtime_id
        .or_else(|| std::env::var("BRICK_RUNTIME_ID").ok())
        .or_else(|| current.and_then(|context| context.runtime_id.clone()));
    let session_id = overrides
        .session_id
        .or(env_id::<SessionId>("BRICK_SESSION_ID")?)
        .or_else(|| current.and_then(|context| context.session_id.clone()))
        .unwrap_or_default();
    let mission_id = overrides
        .mission_id
        .or(env_id::<MissionId>("BRICK_MISSION_ID")?)
        .or_else(|| current.and_then(|context| context.mission_id.clone()));

    let profile_app_id = source_profile.and_then(|profile| profile.app_id.clone());
    let app_id = overrides
        .app_id
        .or_else(|| std::env::var("BRICK_APP_ID").ok())
        .or(profile_app_id)
        .or_else(|| current.and_then(|context| context.app_id.clone()));
    let app_session_id = overrides
        .app_session_id
        .or_else(|| std::env::var("BRICK_APP_SESSION_ID").ok())
        .or_else(|| current.and_then(|context| context.app_session_id.clone()));
    let app_session_name = overrides
        .app_session_name
        .or_else(|| std::env::var("BRICK_APP_SESSION_NAME").ok())
        .or_else(|| current.and_then(|context| context.app_session_name.clone()));

    Ok(ResolvedIdentity {
        actor: ActorRef {
            actor_type,
            actor_id,
            display_name: current_actor.and_then(|actor| actor.display_name),
        },
        runtime_id: runtime_id.clone(),
        session_id,
        session_source: SessionSource {
            app_id,
            app_session_id,
            app_session_name,
            runtime_id,
        },
        mission_id,
    })
}

fn env_id<T>(name: &str) -> Result<Option<T>>
where
    T: FromStr,
    T::Err: std::error::Error + Send + Sync + 'static,
{
    std::env::var(name)
        .ok()
        .map(|value| value.parse::<T>().map_err(anyhow::Error::from))
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_prefers_overrides_over_current_context() {
        std::env::remove_var("BRICK_ACTOR_ID");
        std::env::remove_var("BRICK_ACTOR_TYPE");
        std::env::remove_var("BRICK_APP_ID");
        let current = CurrentContext {
            actor: Some(ActorRef {
                actor_type: ActorType::Human,
                actor_id: "current".to_string(),
                display_name: None,
            }),
            session_id: Some(SessionId::new()),
            ..CurrentContext::default()
        };
        let resolved = resolve_identity(
            Some(&current),
            IdentityOverrides {
                actor_id: Some("override".to_string()),
                actor_type: Some(ActorType::Agent),
                ..IdentityOverrides::default()
            },
        )
        .expect("resolve identity");

        assert_eq!(resolved.actor.actor_id, "override");
        assert_eq!(resolved.actor.actor_type, ActorType::Agent);
    }

    #[test]
    fn identity_uses_source_profile_before_current_context() {
        std::env::remove_var("BRICK_ACTOR_ID");
        std::env::remove_var("BRICK_ACTOR_TYPE");
        std::env::remove_var("BRICK_APP_ID");
        let current = CurrentContext {
            actor: Some(ActorRef {
                actor_type: ActorType::Human,
                actor_id: "current".to_string(),
                display_name: None,
            }),
            app_id: Some("current-app".to_string()),
            ..CurrentContext::default()
        };
        let profile = SourceProfile {
            name: "cursor".to_string(),
            app_id: Some("cursor".to_string()),
            actor_id: Some("profile-agent".to_string()),
            actor_type: Some(ActorType::Agent),
            store_root: None,
            session_db_path: None,
            session_log_path: None,
            evidence_root: None,
            cursor_state_db_path: None,
            default_full_evidence_upload: None,
            notes: None,
        };

        let resolved = resolve_identity_with_profile(
            Some(&current),
            IdentityOverrides::default(),
            Some(&profile),
        )
        .expect("resolve identity");

        assert_eq!(resolved.actor.actor_id, "profile-agent");
        assert_eq!(resolved.actor.actor_type, ActorType::Agent);
        assert_eq!(resolved.session_source.app_id, Some("cursor".to_string()));
    }
}
