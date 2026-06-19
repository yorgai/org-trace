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
    /// Repo-scoped route, e.g. `/v1/repos/:repo_id/...`.
    Repo(String),
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
}

impl TokenRecord {
    /// Whether this token may perform `required` access on `target`.
    fn authorizes(&self, target: &ResourceTarget, required: Access) -> bool {
        self.access.permits(required)
            && self.scopes.iter().any(|scope| scope_matches(scope, target))
    }
}

/// Whether a scope grants access to a resource target.
///
/// Global routes (not tied to a single repo) require an `All` scope, because a
/// repo- or org-restricted token must not read across the whole server via an
/// unscoped listing endpoint.
fn scope_matches(scope: &Scope, target: &ResourceTarget) -> bool {
    match (scope, target) {
        (Scope::All, _) => true,
        (Scope::Repo(allowed), ResourceTarget::Repo(requested)) => allowed == requested,
        // Org scoping is enforced at the repo route only when a repo carries an
        // org; without per-repo org resolution here, an org scope does not by
        // itself grant a bare repo route. Org-scoped tokens reach org-tagged
        // global routes once org resolution lands (future work).
        (Scope::Org(_), _) => false,
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

    /// Authorizes a request: returns the matching record's label on success.
    ///
    /// Returns `Err` describing why the request is denied (unknown token vs
    /// insufficient scope/access) so the route layer can map it to 401 vs 403.
    pub fn authorize(
        &self,
        plaintext: &str,
        target: &ResourceTarget,
        required: Access,
    ) -> Result<String, AuthDenial> {
        let record = self.lookup(plaintext).ok_or(AuthDenial::UnknownToken)?;
        if record.authorizes(target, required) {
            Ok(record.label.clone())
        } else {
            Err(AuthDenial::Forbidden)
        }
    }
}

/// Reason an authorization attempt failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthDenial {
    /// No token matched — the caller is unauthenticated (401).
    UnknownToken,
    /// Token is valid but lacks scope/access for this resource (403).
    Forbidden,
}

fn tokens_path(data_dir: &Path) -> PathBuf {
    data_dir.join(TOKENS_FILE)
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

/// Extracts a `ResourceTarget` from a request path.
///
/// Recognizes `/v1/repos/<repo_id>/...` as a repo target; everything else is a
/// global target.
pub fn resource_target_for_path(path: &str) -> ResourceTarget {
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match segments.as_slice() {
        ["v1", "repos", repo_id, ..] if !repo_id.is_empty() => {
            ResourceTarget::Repo((*repo_id).to_string())
        }
        _ => ResourceTarget::Global,
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
            (token.label.clone(), format!("{access} [{scopes}]"))
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

    #[test]
    fn write_token_permits_read_and_write_in_scope() {
        let mut store = TokenStore::default();
        store.add(TokenRecord {
            label: "ci".to_string(),
            token_sha256: hash_token("secret"),
            scopes: vec![Scope::Repo("repo-a".to_string())],
            access: Access::Write,
        });

        let target = ResourceTarget::Repo("repo-a".to_string());
        assert_eq!(
            store.authorize("secret", &target, Access::Read).unwrap(),
            "ci"
        );
        assert_eq!(
            store.authorize("secret", &target, Access::Write).unwrap(),
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
        });
        let target = ResourceTarget::Repo("repo-a".to_string());
        assert_eq!(
            store.authorize("secret", &target, Access::Read).unwrap(),
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
        });

        assert_eq!(
            store
                .authorize(
                    "secret",
                    &ResourceTarget::Repo("repo-b".to_string()),
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
                &ResourceTarget::Repo("anything".to_string()),
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
            resource_target_for_path("/v1/repos/repo-a/events"),
            ResourceTarget::Repo("repo-a".to_string())
        );
        assert_eq!(
            resource_target_for_path("/v1/sessions"),
            ResourceTarget::Global
        );
        assert_eq!(resource_target_for_path("/health"), ResourceTarget::Global);
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
        });
        store.save(&dir).expect("save tokens");
        let loaded = TokenStore::load(&dir).expect("load tokens");
        assert_eq!(loaded.len(), 1);
        assert_eq!(
            loaded.authorize(
                "secret",
                &ResourceTarget::Repo("repo-a".to_string()),
                Access::Read
            ),
            Ok("ci".to_string())
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
