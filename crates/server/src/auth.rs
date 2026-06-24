//! Token-based authorization for the self-hosted server (stage A).
//!
//! Replaces the single global bearer token with a small token table persisted
//! as JSON in the server data dir. Each token carries a scope (which orgs/repos
//! it may touch) and an access level (read vs write). Tokens are stored as
//! SHA-256 hashes — the plaintext is shown only once at issuance.
//!
//! Out of scope for stage A (tracked in the handoff): external identity (OIDC),
//! per-actor identity binding, and audit events. Scope matching here is the
//! authorization boundary; richer org/project hierarchy resolution is future
//! work.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use jsonwebtoken::{decode, Algorithm, DecodingKey, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

const TOKENS_FILE: &str = "tokens.json";

/// Access level granted to a token. Write implies read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Access {
    Read,
    Write,
}

impl Access {
    /// Whether this access level permits the given request kind.
    fn permits(self, required: Access) -> bool {
        match (self, required) {
            (Access::Write, _) => true,
            (Access::Read, Access::Read) => true,
            (Access::Read, Access::Write) => false,
        }
    }
}

/// What resources a token may reach. `All` is an unrestricted admin scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "value")]
pub enum Scope {
    /// Every org and repo.
    All,
    /// A single org boundary (matches events/routes carrying this org).
    Org(String),
    /// A single repo boundary (matches the `:repo_id` route segment).
    Repo(String),
}

/// A resource the caller is trying to reach, derived from the request route.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResourceTarget {
    /// Repo-scoped route, e.g. `/v1/repos/:repo_id/...`. The optional org is the
    /// repo's owning org when the server could resolve it from stored events,
    /// letting an `org:<id>` scope authorize the repo route.
    Repo {
        repo_id: String,
        org_id: Option<String>,
    },
    /// Global route not tied to a single repo, e.g. `/v1/sessions`.
    Global,
}

/// One issued token record. The secret is stored only as a SHA-256 hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenRecord {
    /// Human-facing label identifying who/what holds the token.
    pub label: String,
    /// Hex-encoded SHA-256 of the plaintext token.
    pub token_sha256: String,
    pub scopes: Vec<Scope>,
    pub access: Access,
    /// Optional expiry. `None` means the token never expires. Defaults to
    /// `None` so token tables written before expiry support still load.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Optional bound actor identity. When `Some`, pushed events must carry an
    /// `actor.actor_id` equal to this value (enforced at the push route).
    /// `None` (the default) means unbound: events are not actor-checked, which
    /// preserves legacy/admin token behavior. Defaults to `None` so token
    /// tables written before actor binding still load.
    #[serde(default)]
    pub actor_id: Option<String>,
}

impl TokenRecord {
    /// Whether this token may perform `required` access on `target`.
    fn authorizes(&self, target: &ResourceTarget, required: Access) -> bool {
        self.access.permits(required)
            && self.scopes.iter().any(|scope| scope_matches(scope, target))
    }

    /// Whether this token is expired as of `now`.
    fn is_expired(&self, now: DateTime<Utc>) -> bool {
        matches!(self.expires_at, Some(expiry) if now >= expiry)
    }
}

/// Whether a scope grants access to a resource target.
///
/// Global routes (not tied to a single repo) require an `All` scope, because a
/// repo- or org-restricted token must not read across the whole server via an
/// unscoped listing endpoint. A repo route is granted by a matching `Repo`
/// scope, or by an `Org` scope when the server resolved the repo's owning org.
fn scope_matches(scope: &Scope, target: &ResourceTarget) -> bool {
    match (scope, target) {
        (Scope::All, _) => true,
        (Scope::Repo(allowed), ResourceTarget::Repo { repo_id, .. }) => allowed == repo_id,
        (Scope::Org(allowed), ResourceTarget::Repo { org_id, .. }) => {
            org_id.as_deref() == Some(allowed.as_str())
        }
        (Scope::Org(_), ResourceTarget::Global) => false,
        (Scope::Repo(_), ResourceTarget::Global) => false,
    }
}

/// Hex-encodes the SHA-256 of a plaintext token.
pub fn hash_token(plaintext: &str) -> String {
    let digest = Sha256::digest(plaintext.as_bytes());
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// In-memory view of the token table, loaded from `tokens.json`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct TokenStore {
    tokens: Vec<TokenRecord>,
}

impl TokenStore {
    /// Loads the token table from `data_dir/tokens.json`, or an empty table.
    pub fn load(data_dir: &Path) -> Result<Self> {
        let path = tokens_path(data_dir);
        if !path.exists() {
            return Ok(Self::default());
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("failed to read token table at {}", path.display()))?;
        if contents.trim().is_empty() {
            return Ok(Self::default());
        }
        serde_json::from_str(&contents)
            .with_context(|| format!("failed to parse token table at {}", path.display()))
    }

    /// Persists the token table to `data_dir/tokens.json`.
    pub fn save(&self, data_dir: &Path) -> Result<()> {
        fs::create_dir_all(data_dir).with_context(|| {
            format!("failed to create server data dir at {}", data_dir.display())
        })?;
        let path = tokens_path(data_dir);
        let serialized =
            serde_json::to_string_pretty(self).context("failed to serialize token table")?;
        fs::write(&path, serialized)
            .with_context(|| format!("failed to write token table at {}", path.display()))?;
        Ok(())
    }

    /// Number of tokens in the table.
    pub fn len(&self) -> usize {
        self.tokens.len()
    }

    /// Whether the table has no tokens.
    pub fn is_empty(&self) -> bool {
        self.tokens.is_empty()
    }

    /// Whether any token carries an `Org` scope. The auth gate uses this to
    /// decide whether resolving a repo's owning org is worth the lookup.
    pub fn has_org_scope(&self) -> bool {
        self.tokens.iter().any(|token| {
            token
                .scopes
                .iter()
                .any(|scope| matches!(scope, Scope::Org(_)))
        })
    }

    /// Adds a token record. Caller is responsible for persisting.
    pub fn add(&mut self, record: TokenRecord) {
        self.tokens.push(record);
    }

    /// Returns the labels of every token, for listing.
    pub fn labels(&self) -> Vec<&str> {
        self.tokens
            .iter()
            .map(|token| token.label.as_str())
            .collect()
    }

    /// Rotates the secret of the token with the given label in place, keeping
    /// its scopes and access. The new SHA-256 hash and the optional new expiry
    /// replace the old ones, immediately invalidating the previous secret.
    /// Returns whether a token matched. Caller is responsible for persisting.
    pub fn rotate_by_label(
        &mut self,
        label: &str,
        new_token_sha256: String,
        expires_at: Option<DateTime<Utc>>,
    ) -> bool {
        match self.tokens.iter_mut().find(|token| token.label == label) {
            Some(token) => {
                token.token_sha256 = new_token_sha256;
                token.expires_at = expires_at;
                true
            }
            None => false,
        }
    }

    /// Returns the current expiry of the token with the given label, if any.
    /// The outer `Option` distinguishes "no such token" (`None`) from "token
    /// exists but never expires" (`Some(None)`).
    pub fn expiry_for_label(&self, label: &str) -> Option<Option<DateTime<Utc>>> {
        self.tokens
            .iter()
            .find(|token| token.label == label)
            .map(|token| token.expires_at)
    }

    /// Removes the token with the given label, returning whether one matched.
    pub fn remove_by_label(&mut self, label: &str) -> bool {
        let before = self.tokens.len();
        self.tokens.retain(|token| token.label != label);
        self.tokens.len() != before
    }

    /// Resolves a plaintext token to its record, if present.
    fn lookup(&self, plaintext: &str) -> Option<&TokenRecord> {
        let hash = hash_token(plaintext);
        self.tokens.iter().find(|token| token.token_sha256 == hash)
    }

    /// Authorizes a request: returns the matching record's identity on success.
    ///
    /// Returns `Err` describing why the request is denied so the route layer can
    /// map it to the right status: unknown or expired token → 401, valid token
    /// lacking scope/access → 403.
    pub fn authorize(
        &self,
        plaintext: &str,
        target: &ResourceTarget,
        required: Access,
    ) -> Result<AuthedIdentity, AuthDenial> {
        self.authorize_at(plaintext, target, required, Utc::now())
    }

    /// Authorization with an explicit clock, for deterministic tests.
    pub fn authorize_at(
        &self,
        plaintext: &str,
        target: &ResourceTarget,
        required: Access,
        now: DateTime<Utc>,
    ) -> Result<AuthedIdentity, AuthDenial> {
        let record = self.lookup(plaintext).ok_or(AuthDenial::UnknownToken)?;
        if record.is_expired(now) {
            return Err(AuthDenial::Expired);
        }
        if record.authorizes(target, required) {
            Ok(AuthedIdentity {
                label: record.label.clone(),
                actor_id: record.actor_id.clone(),
                kind: AuthKind::LocalToken,
            })
        } else {
            Err(AuthDenial::Forbidden)
        }
    }
}

/// Verifies Supabase Auth access tokens signed with the project's HS256 JWT secret.
#[derive(Clone)]
pub struct SupabaseJwtVerifier {
    decoding_key: DecodingKey,
    validation: Validation,
}

#[derive(Debug, Clone, Deserialize)]
struct SupabaseClaims {
    sub: String,
    email: Option<String>,
}

impl SupabaseJwtVerifier {
    pub fn new(project_url: String, jwt_secret: String) -> Result<Self> {
        let issuer = format!("{}/auth/v1", project_url.trim_end_matches('/'));
        let mut validation = Validation::new(Algorithm::HS256);
        validation.set_issuer(&[issuer.as_str()]);
        validation.validate_aud = false;
        Ok(Self {
            decoding_key: DecodingKey::from_secret(jwt_secret.as_bytes()),
            validation,
        })
    }

    pub fn verify(&self, token: &str) -> Result<AuthedIdentity, AuthDenial> {
        let claims = decode::<SupabaseClaims>(token, &self.decoding_key, &self.validation)
            .map_err(|_| AuthDenial::UnknownToken)?
            .claims;
        if claims.sub.trim().is_empty() {
            return Err(AuthDenial::UnknownToken);
        }
        Ok(AuthedIdentity {
            label: format!("supabase:{}", claims.sub),
            actor_id: None,
            kind: AuthKind::Supabase {
                user_id: claims.sub,
                email: claims.email,
            },
        })
    }
}

/// Identity resolved by a successful authorization: the token's label plus its
/// optional bound actor. Carried from the auth middleware to request handlers
/// so push routes can enforce actor binding and the audit log can record it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthedIdentity {
    pub label: String,
    pub actor_id: Option<String>,
    pub kind: AuthKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthKind {
    LocalToken,
    Supabase {
        user_id: String,
        email: Option<String>,
    },
}

/// Reason an authorization attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDenial {
    /// No token matched — the caller is unauthenticated (401).
    UnknownToken,
    /// Token matched but has passed its expiry (401).
    Expired,
    /// Token is valid but lacks scope/access for this resource (403).
    Forbidden,
}

fn tokens_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKENS_FILE)
}

const AUDIT_FILE: &str = "audit.jsonl";

/// One append-only audit record for an authorized write request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditEntry {
    pub at: DateTime<Utc>,
    /// Label of the token that authorized the request.
    pub token_label: String,
    /// The token's bound actor, if any. `None` for unbound tokens. Defaults to
    /// `None` so audit logs written before actor binding still load.
    #[serde(default)]
    pub actor_id: Option<String>,
    pub method: String,
    pub path: String,
}

/// Append-only audit log for authorized writes, stored as `audit.jsonl` in the
/// server data dir. Reads are not audited (high volume, low value); only
/// mutating requests that passed the auth gate are recorded.
#[derive(Debug, Clone)]
pub struct AuditLog {
    path: PathBuf,
}

impl AuditLog {
    /// Creates an audit log handle rooted at `data_dir`.
    pub fn new(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join(AUDIT_FILE),
        }
    }

    /// Appends one audit entry. Best-effort: on I/O error the entry is dropped
    /// rather than failing the request it describes.
    pub fn record(&self, entry: &AuditEntry) {
        use std::io::Write;
        let Ok(serialized) = serde_json::to_string(entry) else {
            return;
        };
        if let Some(parent) = self.path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        if let Ok(mut file) = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "{serialized}");
        }
    }

    /// Reads all audit entries, oldest first. Skips blank/corrupt lines.
    pub fn read_all(&self) -> Result<Vec<AuditEntry>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }
        let contents = fs::read_to_string(&self.path)
            .with_context(|| format!("failed to read audit log at {}", self.path.display()))?;
        Ok(contents
            .lines()
            .filter(|line| !line.trim().is_empty())
            .filter_map(|line| serde_json::from_str::<AuditEntry>(line).ok())
            .collect())
    }
}

/// Parses a `--scope` CLI value into a `Scope`.
///
/// Accepts `*` / `all` for `Scope::All`, `org:<id>` for an org boundary, and
/// `repo:<id>` (or a bare id) for a repo boundary.
pub fn parse_scope(value: &str) -> Result<Scope> {
    let trimmed = value.trim();
    match trimmed {
        "*" | "all" => Ok(Scope::All),
        _ => {
            if let Some(org) = trimmed.strip_prefix("org:") {
                ensure_non_empty(org, "org")?;
                Ok(Scope::Org(org.to_string()))
            } else if let Some(repo) = trimmed.strip_prefix("repo:") {
                ensure_non_empty(repo, "repo")?;
                Ok(Scope::Repo(repo.to_string()))
            } else {
                ensure_non_empty(trimmed, "repo")?;
                Ok(Scope::Repo(trimmed.to_string()))
            }
        }
    }
}

fn ensure_non_empty(value: &str, kind: &str) -> Result<()> {
    if value.is_empty() {
        anyhow::bail!("empty {kind} in --scope");
    }
    Ok(())
}

/// Generates a random opaque token string (`brick_<32 hex>`).
///
/// Uses the process/thread entropy available without pulling in a CSPRNG crate:
/// hashes a UUIDv4 (already a dependency) with the current time. Sufficient for
/// a self-hosted issuance flow where tokens are also revocable.
pub fn generate_token() -> String {
    let seed = format!(
        "{}:{}",
        uuid::Uuid::new_v4(),
        chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
    );
    let hash = hash_token(&seed);
    format!("brick_{}", &hash[..32])
}

/// Maps an HTTP method to the access level it requires.
pub fn required_access(method: &axum::http::Method) -> Access {
    match *method {
        axum::http::Method::GET | axum::http::Method::HEAD | axum::http::Method::OPTIONS => {
            Access::Read
        }
        _ => Access::Write,
    }
}

/// Extracts the repo id from a `/v1/repos/<repo_id>/...` path, if present.
pub fn repo_id_for_path(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        ["v1", "repos", repo_id, ..] if !repo_id.is_empty() => Some((*repo_id).to_string()),
        _ => None,
    }
}

/// Builds a `ResourceTarget` from a request path, resolving the repo's owning
/// org via `resolve_org` only when there is at least one `Org` scope in play
/// (so the lookup cost is skipped entirely for repo/all-only token tables).
pub fn resource_target_for_path(
    path: &str,
    needs_org_resolution: bool,
    resolve_org: impl FnOnce(&str) -> Option<String>,
) -> ResourceTarget {
    match repo_id_for_path(path) {
        Some(repo_id) => {
            let org_id = if needs_org_resolution {
                resolve_org(&repo_id)
            } else {
                None
            };
            ResourceTarget::Repo { repo_id, org_id }
        }
        None => ResourceTarget::Global,
    }
}

/// Builds a `TokenStore` seeded with a single all-access token. Used by tests
/// and as the shape behind the legacy `--auth-token` convenience flow.
#[cfg(test)]
pub fn single_token_store(plaintext: &str) -> TokenStore {
    let mut store = TokenStore::default();
    store.add(TokenRecord {
        label: "legacy-auth-token".to_string(),
        token_sha256: hash_token(plaintext),
        scopes: vec![Scope::All],
        access: Access::Write,
        expires_at: None,
        actor_id: None,
    });
    store
}

/// Returns a stable map of scope summaries for display, keyed by label.
pub fn scope_summary(store: &TokenStore) -> BTreeMap<String, String> {
    store
        .tokens
        .iter()
        .map(|token| {
            let scopes = token
                .scopes
                .iter()
                .map(describe_scope)
                .collect::<Vec<_>>()
                .join(",");
            let access = match token.access {
                Access::Read => "read",
                Access::Write => "write",
            };
            let expiry = match token.expires_at {
                Some(at) => format!(" expires={}", at.to_rfc3339()),
                None => String::new(),
            };
            let actor = match &token.actor_id {
                Some(id) => format!(" actor={id}"),
                None => String::new(),
            };
            (
                token.label.clone(),
                format!("{access} [{scopes}]{expiry}{actor}"),
            )
        })
        .collect()
}

fn describe_scope(scope: &Scope) -> String {
    match scope {
        Scope::All => "*".to_string(),
        Scope::Org(org) => format!("org:{org}"),
        Scope::Repo(repo) => format!("repo:{repo}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use jsonwebtoken::{encode, EncodingKey, Header};

    #[derive(Debug, Serialize)]
    struct TestClaims<'a> {
        sub: &'a str,
        email: &'a str,
        iss: &'a str,
        exp: usize,
    }

    const TEST_SUPABASE_ISSUER: &str = "https://brick-example.supabase.co/auth/v1";

    fn test_supabase_token(secret: &str, subject: &str, exp: usize) -> String {
        encode(
            &Header::new(Algorithm::HS256),
            &TestClaims {
                sub: subject,
                email: "user@example.com",
                iss: TEST_SUPABASE_ISSUER,
                exp,
            },
            &EncodingKey::from_secret(secret.as_bytes()),
        )
        .expect("encode jwt")
    }

    #[test]
    fn supabase_verifier_accepts_valid_access_token() {
        let verifier = SupabaseJwtVerifier::new(
            "https://brick-example.supabase.co".to_string(),
            "jwt-secret".to_string(),
        )
        .expect("verifier");
        let token = test_supabase_token(
            "jwt-secret",
            "user-123",
            (Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        );

        let identity = verifier.verify(&token).expect("verify");
        assert_eq!(identity.actor_id, None);
        assert_eq!(identity.label, "supabase:user-123");
        assert_eq!(
            identity.kind,
            AuthKind::Supabase {
                user_id: "user-123".to_string(),
                email: Some("user@example.com".to_string()),
            }
        );
    }

    #[test]
    fn supabase_verifier_rejects_wrong_secret_and_expired_token() {
        let verifier = SupabaseJwtVerifier::new(
            "https://brick-example.supabase.co".to_string(),
            "jwt-secret".to_string(),
        )
        .expect("verifier");
        let wrong_secret = test_supabase_token(
            "other-secret",
            "user-123",
            (Utc::now() + chrono::Duration::hours(1)).timestamp() as usize,
        );
        assert_eq!(
            verifier.verify(&wrong_secret).expect_err("wrong secret"),
            AuthDenial::UnknownToken
        );

        let expired = test_supabase_token(
            "jwt-secret",
            "user-123",
            (Utc::now() - chrono::Duration::hours(1)).timestamp() as usize,
        );
        assert_eq!(
            verifier.verify(&expired).expect_err("expired"),
            AuthDenial::UnknownToken
        );
    }

    #[test]
    fn write_token_permits_read_and_write_in_scope() {
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "ci".to_string(),
            token_sha256: hash_token("secret"),
            scopes: vec![Scope::Repo("repo-a".to_string())],
            access: Access::Write,
            expires_at: None,
            actor_id: None,
        });

        let target = ResourceTarget::Repo {
            repo_id: "repo-a".to_string(),
            org_id: None,
        };
        assert_eq!(
            store
                .authorize("secret", &target, Access::Read)
                .unwrap()
                .label,
            "ci"
        );
        assert_eq!(
            store
                .authorize("secret", &target, Access::Write)
                .unwrap()
                .label,
            "ci"
        );
    }

    #[test]
    fn read_token_is_denied_write() {
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "viewer".to_string(),
            token_sha256: hash_token("secret"),
            scopes: vec![Scope::All],
            access: Access::Read,
            expires_at: None,
            actor_id: None,
        });
        let target = ResourceTarget::Repo {
            repo_id: "repo-a".to_string(),
            org_id: None,
        };
        assert_eq!(
            store
                .authorize("secret", &target, Access::Read)
                .unwrap()
                .label,
            "viewer"
        );
        assert_eq!(
            store
                .authorize("secret", &target, Access::Write)
                .unwrap_err(),
            AuthDenial::Forbidden
        );
    }

    #[test]
    fn repo_scope_does_not_match_other_repo_or_global() {
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "repo-a-only".to_string(),
            token_sha256: hash_token("secret"),
            scopes: vec![Scope::Repo("repo-a".to_string())],
            access: Access::Write,
            expires_at: None,
            actor_id: None,
        });

        assert_eq!(
            store
                .authorize(
                    "secret",
                    &ResourceTarget::Repo {
                        repo_id: "repo-b".to_string(),
                        org_id: None,
                    },
                    Access::Read
                )
                .unwrap_err(),
            AuthDenial::Forbidden
        );
        assert_eq!(
            store
                .authorize("secret", &ResourceTarget::Global, Access::Read)
                .unwrap_err(),
            AuthDenial::Forbidden
        );
    }

    #[test]
    fn all_scope_reaches_global_and_repo() {
        let store = single_token_store("admin");
        assert!(store
            .authorize("admin", &ResourceTarget::Global, Access::Write)
            .is_ok());
        assert!(store
            .authorize(
                "admin",
                &ResourceTarget::Repo {
                    repo_id: "anything".to_string(),
                    org_id: None,
                },
                Access::Write
            )
            .is_ok());
    }

    #[test]
    fn unknown_token_is_unauthenticated() {
        let store = single_token_store("admin");
        assert_eq!(
            store
                .authorize("wrong", &ResourceTarget::Global, Access::Read)
                .unwrap_err(),
            AuthDenial::UnknownToken
        );
    }

    #[test]
    fn expired_token_is_denied_before_scope_check() {
        let mut store = TokenStore::default();
        let issued = Utc::now();
        store.add(TokenRecord {
            label: "temp".to_string(),
            token_sha256: hash_token("secret"),
            scopes: vec![Scope::All],
            access: Access::Write,
            expires_at: Some(issued + chrono::Duration::days(1)),
            actor_id: None,
        });

        // Valid before expiry.
        assert!(store
            .authorize_at("secret", &ResourceTarget::Global, Access::Read, issued)
            .is_ok());
        // Denied at/after expiry, and reported as Expired (→ 401), not Forbidden.
        assert_eq!(
            store
                .authorize_at(
                    "secret",
                    &ResourceTarget::Global,
                    Access::Read,
                    issued + chrono::Duration::days(2)
                )
                .unwrap_err(),
            AuthDenial::Expired
        );
    }

    #[test]
    fn token_without_expiry_never_expires() {
        let store = single_token_store("admin");
        let far_future = Utc::now() + chrono::Duration::days(36500);
        assert!(store
            .authorize_at("admin", &ResourceTarget::Global, Access::Read, far_future)
            .is_ok());
    }

    #[test]
    fn org_scope_grants_repo_only_when_org_resolves() {
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "org-team".to_string(),
            token_sha256: hash_token("secret"),
            scopes: vec![Scope::Org("acme".to_string())],
            access: Access::Write,
            expires_at: None,
            actor_id: None,
        });

        let in_org = ResourceTarget::Repo {
            repo_id: "repo-a".to_string(),
            org_id: Some("acme".to_string()),
        };
        assert_eq!(
            store
                .authorize("secret", &in_org, Access::Write)
                .unwrap()
                .label,
            "org-team"
        );

        let other_org = ResourceTarget::Repo {
            repo_id: "repo-b".to_string(),
            org_id: Some("globex".to_string()),
        };
        assert_eq!(
            store
                .authorize("secret", &other_org, Access::Read)
                .unwrap_err(),
            AuthDenial::Forbidden
        );

        let unknown_org = ResourceTarget::Repo {
            repo_id: "repo-c".to_string(),
            org_id: None,
        };
        assert_eq!(
            store
                .authorize("secret", &unknown_org, Access::Read)
                .unwrap_err(),
            AuthDenial::Forbidden
        );
    }

    #[test]
    fn rotate_replaces_secret_and_keeps_scope() {
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "ci".to_string(),
            token_sha256: hash_token("old"),
            scopes: vec![Scope::Repo("repo-a".to_string())],
            access: Access::Write,
            expires_at: None,
            actor_id: None,
        });
        let target = ResourceTarget::Repo {
            repo_id: "repo-a".to_string(),
            org_id: None,
        };

        // Rotate to a new secret, keeping scope/access.
        assert!(store.rotate_by_label("ci", hash_token("new"), None));
        // Old secret no longer authorizes; new one does, with the same scope.
        assert_eq!(
            store.authorize("old", &target, Access::Read).unwrap_err(),
            AuthDenial::UnknownToken
        );
        assert_eq!(
            store
                .authorize("new", &target, Access::Write)
                .unwrap()
                .label,
            "ci"
        );
        // Scope is unchanged: a different repo is still denied.
        let other = ResourceTarget::Repo {
            repo_id: "repo-b".to_string(),
            org_id: None,
        };
        assert_eq!(
            store.authorize("new", &other, Access::Read).unwrap_err(),
            AuthDenial::Forbidden
        );
    }

    #[test]
    fn rotate_unknown_label_is_noop() {
        let mut store = TokenStore::default();
        assert!(!store.rotate_by_label("ghost", hash_token("x"), None));
        assert!(store.is_empty());
    }

    #[test]
    fn expiry_for_label_distinguishes_missing_from_never() {
        let mut store = TokenStore::default();
        assert_eq!(store.expiry_for_label("ci"), None);
        store.add(TokenRecord {
            label: "ci".to_string(),
            token_sha256: hash_token("s"),
            scopes: vec![Scope::All],
            access: Access::Read,
            expires_at: None,
            actor_id: None,
        });
        assert_eq!(store.expiry_for_label("ci"), Some(None));
    }

    #[test]
    fn has_org_scope_detects_org_tokens() {
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "repo-only".to_string(),
            token_sha256: hash_token("a"),
            scopes: vec![Scope::Repo("repo-a".to_string())],
            access: Access::Read,
            expires_at: None,
            actor_id: None,
        });
        assert!(!store.has_org_scope());
        store.add(TokenRecord {
            label: "org".to_string(),
            token_sha256: hash_token("b"),
            scopes: vec![Scope::Org("acme".to_string())],
            access: Access::Read,
            expires_at: None,
            actor_id: None,
        });
        assert!(store.has_org_scope());
    }

    #[test]
    fn resource_target_resolves_org_only_when_requested() {
        let target = resource_target_for_path("/v1/repos/repo-a/events", false, |_| {
            panic!("resolver must not run when org resolution is disabled")
        });
        assert_eq!(
            target,
            ResourceTarget::Repo {
                repo_id: "repo-a".to_string(),
                org_id: None,
            }
        );
        let target = resource_target_for_path("/v1/repos/repo-a/events", true, |repo| {
            assert_eq!(repo, "repo-a");
            Some("acme".to_string())
        });
        assert_eq!(
            target,
            ResourceTarget::Repo {
                repo_id: "repo-a".to_string(),
                org_id: Some("acme".to_string()),
            }
        );
    }

    #[test]
    fn parse_scope_accepts_known_forms() {
        assert_eq!(parse_scope("*").unwrap(), Scope::All);
        assert_eq!(parse_scope("all").unwrap(), Scope::All);
        assert_eq!(
            parse_scope("org:acme").unwrap(),
            Scope::Org("acme".to_string())
        );
        assert_eq!(
            parse_scope("repo:repo-a").unwrap(),
            Scope::Repo("repo-a".to_string())
        );
        assert_eq!(
            parse_scope("repo-a").unwrap(),
            Scope::Repo("repo-a".to_string())
        );
        assert!(parse_scope("org:").is_err());
    }

    #[test]
    fn resource_target_parses_repo_routes() {
        assert_eq!(
            resource_target_for_path("/v1/repos/repo-a/events", false, |_| None),
            ResourceTarget::Repo {
                repo_id: "repo-a".to_string(),
                org_id: None,
            }
        );
        assert_eq!(
            resource_target_for_path("/v1/sessions", false, |_| None),
            ResourceTarget::Global
        );
        assert_eq!(
            resource_target_for_path("/health", false, |_| None),
            ResourceTarget::Global
        );
    }

    #[test]
    fn token_store_round_trips_through_disk() {
        let dir = std::env::temp_dir().join(format!(
            "brick-token-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "ci".to_string(),
            token_sha256: hash_token("secret"),
            scopes: vec![Scope::Repo("repo-a".to_string())],
            access: Access::Read,
            expires_at: None,
            actor_id: None,
        });
        store.save(&dir).expect("save tokens");
        let loaded = TokenStore::load(&dir).expect("load tokens");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded
                .authorize(
                    "secret",
                    &ResourceTarget::Repo {
                        repo_id: "repo-a".to_string(),
                        org_id: None,
                    },
                    Access::Read
                )
                .unwrap()
                .label,
            "ci"
        );
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn authorize_returns_bound_actor_id() {
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "agent-ci".to_string(),
            token_sha256: hash_token("secret"),
            scopes: vec![Scope::All],
            access: Access::Write,
            expires_at: None,
            actor_id: Some("agent-ci".to_string()),
        });
        let identity = store
            .authorize("secret", &ResourceTarget::Global, Access::Write)
            .expect("authorized");
        assert_eq!(identity.label, "agent-ci");
        assert_eq!(identity.actor_id.as_deref(), Some("agent-ci"));
    }

    #[test]
    fn authorize_returns_none_actor_for_unbound_token() {
        let store = single_token_store("admin");
        let identity = store
            .authorize("admin", &ResourceTarget::Global, Access::Read)
            .expect("authorized");
        assert_eq!(identity.actor_id, None);
    }

    #[test]
    fn audit_entry_round_trips_actor() {
        let entry = AuditEntry {
            at: Utc::now(),
            token_label: "agent-ci".to_string(),
            actor_id: Some("agent-ci".to_string()),
            method: "POST".to_string(),
            path: "/v1/repos/repo-a/events".to_string(),
        };
        let serialized = serde_json::to_string(&entry).expect("serialize");
        let parsed: AuditEntry = serde_json::from_str(&serialized).expect("parse");
        assert_eq!(parsed, entry);

        let legacy = r#"{"at":"2026-01-01T00:00:00Z","token_label":"ci","method":"POST","path":"/v1/events"}"#;
        let parsed: AuditEntry = serde_json::from_str(legacy).expect("parse legacy");
        assert_eq!(parsed.actor_id, None);
    }

    #[test]
    fn token_record_without_actor_field_defaults_to_unbound() {
        let legacy =
            r#"{"label":"ci","token_sha256":"abc","scopes":[{"kind":"all"}],"access":"write"}"#;
        let record: TokenRecord = serde_json::from_str(legacy).expect("parse legacy token");
        assert_eq!(record.actor_id, None);
        assert_eq!(record.expires_at, None);
    }
}
