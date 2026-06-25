//! Local login session for the proprietary sync surface.
//!
//! NOTE ON NAMING: this `identity` is the user's **account login** (Supabase
//! access/refresh tokens). It is unrelated to `brick_core::identity`, which
//! resolves the **authorship identity** of a change (actor/agent/runtime via
//! `ResolvedIdentity`/`CurrentContext`). The two never co-occur in one module;
//! if you import this one, you want login/account, not change-authorship.
//!
//! Holds the user's Supabase identity (access + refresh tokens) on disk under the
//! global Brick home, and the `brick login` / `logout` / `whoami` flows that
//! populate it. This lives in `brick-sync` (the proprietary, feature-gated crate)
//! on purpose: account identity is part of the closed networked surface, not the
//! open-source local recorder. The open `brick-core` crate never learns about
//! login.
//!
//! ## Soft gate, stated honestly
//!
//! `is_logged_in()` is consumed by the CLI/MCP layer to gate line-level blame and
//! planning tools behind a login. Because the gate lives in open-source-adjacent
//! call sites and the underlying `blame_*` functions in `brick-core` are pure and
//! unguarded, this is a **soft gate**: it only holds in the official distributed
//! binary. Anyone building from source can bypass it. It is a registration hook,
//! not a security boundary. The real moat is future server-side full-track data
//! (gated by a server-verified JWT), which a local rebuild cannot fabricate.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use brick_core::resolve_brick_home;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Filename of the persisted login session under the Brick home.
const IDENTITY_FILE: &str = "identity.json";

/// Default Supabase auth base. Overridable via env so a self-hosted or staging
/// Supabase can be pointed at without a rebuild. These are the public project
/// URL + anon key (safe to ship); the user still has to authenticate.
const SUPABASE_URL_ENV: &str = "BRICK_SUPABASE_URL";
const SUPABASE_ANON_KEY_ENV: &str = "BRICK_SUPABASE_ANON_KEY";
const DEFAULT_SUPABASE_URL: &str = "https://vplfljdsixvzglrxubjp.supabase.co";
const DEFAULT_SUPABASE_ANON_KEY: &str = "sb_publishable__ichJ1uIlems6meDErcTxQ_kXudNSgm";

/// A persisted login session: the Supabase tokens plus who they belong to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Identity {
    /// Supabase user id (the JWT `sub`). Stable across token refreshes.
    pub user_id: String,
    /// User's email, for display in `whoami`.
    pub email: Option<String>,
    /// Short-lived Supabase access token (JWT). Sent as the sync bearer.
    pub access_token: String,
    /// Long-lived refresh token used to mint a new access token at expiry.
    pub refresh_token: String,
    /// Absolute expiry of `access_token`. `None` if the server didn't report one.
    pub expires_at: Option<DateTime<Utc>>,
}

impl Identity {
    /// Whether the access token is expired as of `now` (with a small skew so a
    /// token about to expire is treated as already expired and refreshed).
    pub fn is_expired_at(&self, now: DateTime<Utc>) -> bool {
        match self.expires_at {
            Some(expiry) => now + chrono::Duration::seconds(30) >= expiry,
            None => false,
        }
    }

    /// Whether the access token is expired as of now.
    pub fn is_expired(&self) -> bool {
        self.is_expired_at(Utc::now())
    }
}

/// Path of the persisted identity file under the resolved Brick home.
fn identity_path() -> Result<PathBuf> {
    Ok(resolve_brick_home()?.join(IDENTITY_FILE))
}

/// Loads the persisted login session, or `None` if the user is not logged in.
pub fn load() -> Result<Option<Identity>> {
    let path = identity_path()?;
    load_from(&path)
}

/// Loads a login session from an explicit path (used by tests).
pub fn load_from(path: &Path) -> Result<Option<Identity>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read identity at {}", path.display()))?;
    if contents.trim().is_empty() {
        return Ok(None);
    }
    let identity = serde_json::from_str(&contents)
        .with_context(|| format!("failed to parse identity at {}", path.display()))?;
    Ok(Some(identity))
}

/// Persists a login session to `identity.json` with owner-only permissions.
pub fn save(identity: &Identity) -> Result<()> {
    let path = identity_path()?;
    save_to(&path, identity)
}

/// Persists a login session to an explicit path (used by tests).
pub fn save_to(path: &Path, identity: &Identity) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create Brick home at {}", parent.display()))?;
    }
    let serialized =
        serde_json::to_string_pretty(identity).context("failed to serialize identity")?;
    fs::write(path, serialized)
        .with_context(|| format!("failed to write identity at {}", path.display()))?;
    set_owner_only(path);
    Ok(())
}

/// Removes the persisted login session. Returns whether a file was present.
pub fn clear() -> Result<bool> {
    let path = identity_path()?;
    if path.exists() {
        fs::remove_file(&path)
            .with_context(|| format!("failed to remove identity at {}", path.display()))?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Whether a non-expired login session exists. This is the soft gate the
/// CLI/MCP layer checks before allowing line-level blame / planning tools.
///
/// Returns `false` on any read error (a malformed identity file is treated as
/// "not logged in" rather than failing the user's command).
pub fn is_logged_in() -> bool {
    matches!(load(), Ok(Some(identity)) if !identity.is_expired())
}

#[cfg(unix)]
fn set_owner_only(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_owner_only(_path: &Path) {}

/// Resolves the Supabase project URL + anon key from the environment, falling
/// back to the compiled-in defaults. Returns an error only if no URL is set and
/// no default is compiled in (so login fails loudly rather than hitting a bogus
/// host).
pub fn supabase_config() -> Result<(String, String)> {
    let url = std::env::var(SUPABASE_URL_ENV)
        .ok()
        .or_else(|| option_env!("BRICK_SUPABASE_URL").map(str::to_string))
        .unwrap_or_else(|| DEFAULT_SUPABASE_URL.to_string());
    let anon_key = std::env::var(SUPABASE_ANON_KEY_ENV)
        .ok()
        .or_else(|| option_env!("BRICK_SUPABASE_ANON_KEY").map(str::to_string))
        .unwrap_or_else(|| DEFAULT_SUPABASE_ANON_KEY.to_string());
    Ok((url.trim_end_matches('/').to_string(), anon_key))
}

/// Supabase token response shared by the OTP-verify and refresh endpoints.
#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    /// Seconds until the access token expires.
    expires_in: Option<i64>,
    user: Option<SupabaseUser>,
}

#[derive(Debug, Deserialize)]
struct SupabaseUser {
    id: String,
    email: Option<String>,
}

impl TokenResponse {
    fn into_identity(self) -> Identity {
        let expires_at = self
            .expires_in
            .map(|secs| Utc::now() + chrono::Duration::seconds(secs));
        let (user_id, email) = match self.user {
            Some(user) => (user.id, user.email),
            None => (String::new(), None),
        };
        Identity {
            user_id,
            email,
            access_token: self.access_token,
            refresh_token: self.refresh_token,
            expires_at,
        }
    }
}

/// Step 1 of email-OTP login: asks Supabase to email a one-time code to `email`.
pub fn request_email_otp(email: &str) -> Result<()> {
    let (url, anon_key) = supabase_config()?;
    let endpoint = format!("{url}/auth/v1/otp");
    ureq::post(&endpoint)
        .header("apikey", &anon_key)
        .header("content-type", "application/json")
        .send_json(serde_json::json!({ "email": email, "create_user": true }))
        .with_context(|| format!("failed to request OTP from {endpoint}"))?;
    Ok(())
}

/// Step 2 of email-OTP login: exchanges the emailed `code` for tokens and
/// persists the resulting login session. Returns the saved identity.
///
/// Supabase tags the verify call with an OTP `type` that differs by account
/// state: a brand-new account confirming for the first time needs `signup`,
/// while an already-confirmed user signing in again needs `email`. We try
/// `email` first (the steady-state case) and fall back to `signup` so a
/// first-ever login also works without the caller knowing the account state.
pub fn verify_email_otp(email: &str, code: &str) -> Result<Identity> {
    let identity = match verify_otp_with_type(email, code, "email") {
        Ok(identity) => identity,
        Err(_) => verify_otp_with_type(email, code, "signup")
            .context("failed to verify the one-time code (tried both email and signup types)")?,
    };
    save(&identity)?;
    Ok(identity)
}

pub fn save_magic_link_callback(callback_url: &str) -> Result<Identity> {
    let url = url::Url::parse(callback_url).context("failed to parse Supabase callback URL")?;
    let fragment = url
        .fragment()
        .context("Supabase callback URL is missing the #access_token fragment")?;
    let params: std::collections::HashMap<_, _> =
        url::form_urlencoded::parse(fragment.as_bytes()).collect();
    let access_token = params
        .get("access_token")
        .context("Supabase callback URL is missing access_token")?
        .to_string();
    let refresh_token = params
        .get("refresh_token")
        .context("Supabase callback URL is missing refresh_token")?
        .to_string();
    let expires_at = params
        .get("expires_at")
        .and_then(|value| value.parse::<i64>().ok())
        .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, 0));
    let claims = decode_jwt_claims(&access_token)?;
    let identity = Identity {
        user_id: claims.sub,
        email: claims.email,
        access_token,
        refresh_token,
        expires_at,
    };
    save(&identity)?;
    Ok(identity)
}

#[derive(Debug, Deserialize)]
struct AccessTokenClaims {
    sub: String,
    email: Option<String>,
}

fn decode_jwt_claims(access_token: &str) -> Result<AccessTokenClaims> {
    let payload = access_token
        .split('.')
        .nth(1)
        .context("Supabase access token is not a JWT")?;
    let decoded = URL_SAFE_NO_PAD
        .decode(payload)
        .context("failed to decode Supabase access token claims")?;
    serde_json::from_slice(&decoded).context("failed to parse Supabase access token claims")
}

/// Posts a single `/auth/v1/verify` attempt with an explicit OTP `type`.
fn verify_otp_with_type(email: &str, code: &str, otp_type: &str) -> Result<Identity> {
    let (url, anon_key) = supabase_config()?;
    let endpoint = format!("{url}/auth/v1/verify");
    let mut response = ureq::post(&endpoint)
        .header("apikey", &anon_key)
        .header("content-type", "application/json")
        .send_json(serde_json::json!({ "type": otp_type, "email": email, "token": code }))
        .with_context(|| format!("failed to verify OTP ({otp_type}) at {endpoint}"))?;
    let token: TokenResponse = response
        .body_mut()
        .read_json()
        .context("failed to decode Supabase token response")?;
    Ok(token.into_identity())
}

/// Refreshes the access token using the stored refresh token, persisting and
/// returning the renewed identity. Returns an error if not logged in.
pub fn refresh() -> Result<Identity> {
    let current = load()?.context("not logged in; run `brick login` first")?;
    let (url, anon_key) = supabase_config()?;
    let endpoint = format!("{url}/auth/v1/token?grant_type=refresh_token");
    let mut response = ureq::post(&endpoint)
        .header("apikey", &anon_key)
        .header("content-type", "application/json")
        .send_json(serde_json::json!({ "refresh_token": current.refresh_token }))
        .with_context(|| format!("failed to refresh token at {endpoint}"))?;
    let token: TokenResponse = response
        .body_mut()
        .read_json()
        .context("failed to decode Supabase refresh response")?;
    let identity = token.into_identity();
    save(&identity)?;
    Ok(identity)
}

/// Returns a valid (non-expired) identity, refreshing if the access token has
/// expired. Returns an error if not logged in.
pub fn refresh_if_needed() -> Result<Identity> {
    let current = load()?.context("not logged in; run `brick login` first")?;
    if current.is_expired() {
        refresh()
    } else {
        Ok(current)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(expires_at: Option<DateTime<Utc>>) -> Identity {
        Identity {
            user_id: "user-123".to_string(),
            email: Some("a@b.c".to_string()),
            access_token: "access".to_string(),
            refresh_token: "refresh".to_string(),
            expires_at,
        }
    }

    fn tmp_path(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "brick-identity-{tag}-{}.json",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ))
    }

    #[test]
    fn round_trips_through_disk() {
        let path = tmp_path("round-trip");
        let identity = sample(Some(Utc::now() + chrono::Duration::hours(1)));
        save_to(&path, &identity).expect("save");
        let loaded = load_from(&path).expect("load").expect("present");
        assert_eq!(loaded, identity);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn missing_file_is_not_logged_in() {
        let path = tmp_path("missing");
        assert_eq!(load_from(&path).expect("load"), None);
    }

    #[test]
    fn parses_magic_link_callback_url() {
        let token = "header.eyJzdWIiOiJ1c2VyLTEyMyIsImVtYWlsIjoiYUBiLmMifQ.sig";
        let callback = format!(
            "http://localhost:3000/#access_token={token}&refresh_token=refresh&expires_at=1893456000"
        );

        let url = url::Url::parse(&callback).expect("url");
        let fragment = url.fragment().expect("fragment");
        let params: std::collections::HashMap<_, _> =
            url::form_urlencoded::parse(fragment.as_bytes()).collect();
        let claims =
            decode_jwt_claims(params.get("access_token").expect("access token")).expect("claims");

        assert_eq!(claims.sub, "user-123");
        assert_eq!(claims.email.as_deref(), Some("a@b.c"));
        assert_eq!(
            params.get("refresh_token").map(|v| v.as_ref()),
            Some("refresh")
        );
    }

    #[test]
    fn expiry_uses_skew() {
        // Expires in 10s → treated as expired (within the 30s skew).
        let soon = sample(Some(Utc::now() + chrono::Duration::seconds(10)));
        assert!(soon.is_expired());
        // Expires in 1h → still valid.
        let later = sample(Some(Utc::now() + chrono::Duration::hours(1)));
        assert!(!later.is_expired());
        // No expiry → never expires.
        assert!(!sample(None).is_expired());
    }
}
