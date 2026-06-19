//! Minimal self-hosted server entry point.
//!
//! The server now exposes a small append-only event sync surface while keeping
//! auth, repo authorization, queue draining, and migrations for later phases.

use anyhow::Result;
use clap::Parser;

mod args;
mod auth;
mod index;
mod routes;
mod store;

use args::{Cli, Command};
use auth::{
    generate_token, hash_token, parse_scope, scope_summary, Access, Scope, TokenRecord, TokenStore,
};
use index::{rebuild_server_index, server_index_status};
use routes::{serve, AuthConfig, LocalHistoryBridge};
use store::ServerStore;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Serve {
            bind,
            data_dir,
            enable_local_history,
            brick_bin,
            repo_root,
            auth_token,
        } => {
            let history_bridge =
                enable_local_history.then(|| LocalHistoryBridge::new(brick_bin, repo_root));
            let auth = resolve_serve_auth(&data_dir, auth_token)?;
            serve(bind, ServerStore::new(data_dir), history_bridge, auth).await?
        }
        Command::RebuildIndex { data_dir, repo_id } => {
            let store = ServerStore::new(data_dir);
            let events = store.read_events_for_repo(repo_id.as_deref())?;
            let index = rebuild_server_index(repo_id.as_deref(), &events)?;
            let status = server_index_status(repo_id.as_deref(), &index);
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        Command::Migrate => println!("migrate is not implemented yet"),
        Command::CreateAdmin => println!("create-admin is not implemented yet"),
        Command::CreateToken {
            data_dir,
            label,
            scopes,
            write,
            expires_in_days,
        } => create_token(&data_dir, label, scopes, write, expires_in_days)?,
        Command::ListTokens { data_dir } => list_tokens(&data_dir)?,
        Command::RevokeToken { data_dir, label } => revoke_token(&data_dir, &label)?,
        Command::RotateToken {
            data_dir,
            label,
            expires_in_days,
        } => rotate_token(&data_dir, &label, expires_in_days)?,
        Command::Audit { data_dir, limit } => show_audit(&data_dir, limit)?,
    }

    Ok(())
}

/// Resolves the auth gate for `serve`.
///
/// Precedence: a persisted token table (if it has any tokens) is used as-is.
/// A `--auth-token` value is merged in as a convenience all-access token so the
/// legacy single-token flow keeps working. When neither is present the server
/// stays open.
fn resolve_serve_auth(
    data_dir: &std::path::Path,
    auth_token: Option<String>,
) -> Result<Option<AuthConfig>> {
    let mut tokens = TokenStore::load(data_dir)?;
    if let Some(plaintext) = auth_token.filter(|token| !token.is_empty()) {
        tokens.add(TokenRecord {
            label: "legacy-auth-token".to_string(),
            token_sha256: hash_token(&plaintext),
            scopes: vec![Scope::All],
            access: Access::Write,
            expires_at: None,
        });
    }
    if tokens.is_empty() {
        Ok(None)
    } else {
        Ok(Some(AuthConfig::new(tokens, auth::AuditLog::new(data_dir))))
    }
}

fn create_token(
    data_dir: &std::path::Path,
    label: String,
    scopes: Vec<String>,
    write: bool,
    expires_in_days: Option<u32>,
) -> Result<()> {
    let mut store = TokenStore::load(data_dir)?;
    if store.labels().iter().any(|existing| *existing == label) {
        anyhow::bail!("a token labeled {label:?} already exists; revoke it first");
    }
    let parsed_scopes = if scopes.is_empty() {
        vec![Scope::All]
    } else {
        scopes
            .iter()
            .map(|scope| parse_scope(scope))
            .collect::<Result<Vec<_>>>()?
    };
    let expires_at =
        expires_in_days.map(|days| chrono::Utc::now() + chrono::Duration::days(i64::from(days)));
    let plaintext = generate_token();
    store.add(TokenRecord {
        label: label.clone(),
        token_sha256: hash_token(&plaintext),
        scopes: parsed_scopes,
        access: if write { Access::Write } else { Access::Read },
        expires_at,
    });
    store.save(data_dir)?;
    println!("token_label={label}");
    println!("access={}", if write { "write" } else { "read" });
    if let Some(expiry) = expires_at {
        println!("expires_at={}", expiry.to_rfc3339());
    }
    // Plaintext is shown once and never persisted.
    println!("token={plaintext}");
    Ok(())
}

fn list_tokens(data_dir: &std::path::Path) -> Result<()> {
    let store = TokenStore::load(data_dir)?;
    println!("token_count={}", store.len());
    for (label, summary) in scope_summary(&store) {
        println!("token={label} {summary}");
    }
    Ok(())
}

fn revoke_token(data_dir: &std::path::Path, label: &str) -> Result<()> {
    let mut store = TokenStore::load(data_dir)?;
    if store.remove_by_label(label) {
        store.save(data_dir)?;
        println!("revoked={label}");
    } else {
        println!("not_found={label}");
    }
    Ok(())
}

fn rotate_token(
    data_dir: &std::path::Path,
    label: &str,
    expires_in_days: Option<u32>,
) -> Result<()> {
    let mut store = TokenStore::load(data_dir)?;
    // Keep the current expiry unless --expires-in-days was given; 0 clears it.
    let expires_at = match expires_in_days {
        None => match store.expiry_for_label(label) {
            Some(current) => current,
            None => anyhow::bail!("no token labeled {label:?}"),
        },
        Some(0) => None,
        Some(days) => Some(chrono::Utc::now() + chrono::Duration::days(i64::from(days))),
    };
    let plaintext = generate_token();
    if store.rotate_by_label(label, hash_token(&plaintext), expires_at) {
        store.save(data_dir)?;
        println!("rotated={label}");
        if let Some(expiry) = expires_at {
            println!("expires_at={}", expiry.to_rfc3339());
        }
        // Plaintext is shown once and never persisted.
        println!("token={plaintext}");
    } else {
        anyhow::bail!("no token labeled {label:?}");
    }
    Ok(())
}

fn show_audit(data_dir: &std::path::Path, limit: Option<usize>) -> Result<()> {
    let log = auth::AuditLog::new(data_dir);
    let entries = log.read_all()?;
    let start = match limit {
        Some(limit) if limit < entries.len() => entries.len() - limit,
        _ => 0,
    };
    println!("audit_count={}", entries.len() - start);
    for entry in &entries[start..] {
        println!(
            "{} {} {} {}",
            entry.at.to_rfc3339(),
            entry.token_label,
            entry.method,
            entry.path
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serve_auth_is_open_without_tokens_or_flag() {
        let dir = std::env::temp_dir().join(format!(
            "brick-serve-auth-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let auth = resolve_serve_auth(&dir, None).expect("resolve");
        assert!(auth.is_none());
    }

    #[test]
    fn serve_auth_uses_legacy_flag_token() {
        let dir = std::env::temp_dir().join(format!(
            "brick-serve-auth-flag-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        let auth = resolve_serve_auth(&dir, Some("secret".to_string())).expect("resolve");
        assert!(auth.is_some());
    }

    #[test]
    fn create_then_list_then_revoke_token() {
        let dir = std::env::temp_dir().join(format!(
            "brick-token-cli-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        create_token(
            &dir,
            "ci".to_string(),
            vec!["repo:repo-a".to_string()],
            true,
            None,
        )
        .expect("create");
        let store = TokenStore::load(&dir).expect("load");
        assert_eq!(store.len(), 1);
        assert_eq!(store.labels(), vec!["ci"]);
        revoke_token(&dir, "ci").expect("revoke");
        let store = TokenStore::load(&dir).expect("reload");
        assert!(store.is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn create_token_rejects_duplicate_label() {
        let dir = std::env::temp_dir().join(format!(
            "brick-token-dup-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        create_token(&dir, "ci".to_string(), vec![], false, None).expect("create");
        assert!(create_token(&dir, "ci".to_string(), vec![], false, None).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
