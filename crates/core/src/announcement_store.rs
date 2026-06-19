//! Active-work announcements ("claims") — the cross-session bulletin board.
//!
//! Liveness tells you *that* a session is running; an announcement tells you
//! *what it is about to change and to keep your hands off*. A session broadcasts
//! "I'm refactoring auth.rs, don't touch it" before editing; anyone calling
//! `recall_file` on a matching path sees that note first.
//!
//! ## Why a separate database
//!
//! Announcements are **authored user intent**, not derived data. The unified
//! `~/.brick/metadata.sqlite` is a *rebuildable cache* — any schema bump triggers
//! a full `reset_schema`, wiping every row. Storing claims there would silently
//! delete them on the next upgrade. So claims live in their own
//! `~/.brick/announcements.sqlite`, which is migrated additively and never reset.
//!
//! ## Lifecycle (session-tied + TTL)
//!
//! A claim is bound to the publishing session id and carries an explicit
//! `expires_at`. It stops being surfaced when either (a) its TTL passes, or
//! (b) the owning session is no longer live — both checks happen at read time,
//! so a forgotten claim self-expires instead of misleading the next person.
//! Expired rows are also swept opportunistically on write.

use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration, Utc};
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::resolve_brick_home;

/// Filename of the standalone announcements database under the Brick home.
pub const ANNOUNCEMENTS_DB_FILE: &str = "announcements.sqlite";

/// Default time-to-live applied when a claim is published without an explicit
/// duration. Long enough to cover a normal editing session, short enough that a
/// forgotten claim does not haunt the bulletin board for a day.
pub const DEFAULT_CLAIM_TTL: Duration = Duration::hours(4);

/// A single active-work claim, as stored and returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Announcement {
    pub id: String,
    /// Source/app id of the publisher (e.g. `claude_code`, `codex_app`, `orgii`).
    pub source_id: String,
    /// External session id of the publisher; used to suppress self-warnings and
    /// to tie the claim's validity to that session still being live.
    pub session_id: String,
    /// File path or glob the claim covers, stored normalized (absolute when the
    /// caller gave an absolute path).
    pub scope: String,
    /// Free-text one-liner: what the publisher is doing and any warning.
    pub message: String,
    /// Working directory / repo the claim was made from, for display + scoping.
    pub work_dir: Option<String>,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

/// A new claim to publish.
#[derive(Debug, Clone)]
pub struct NewAnnouncement {
    pub source_id: String,
    pub session_id: String,
    pub scope: String,
    pub message: String,
    pub work_dir: Option<String>,
    pub ttl: Option<Duration>,
}

/// Handle to the standalone announcements database.
pub struct AnnouncementStore {
    connection: Connection,
}

impl AnnouncementStore {
    /// Opens the announcements DB under the resolved global Brick home.
    pub fn open_global() -> Result<Self> {
        Self::open_path(resolve_brick_home()?.join(ANNOUNCEMENTS_DB_FILE))
    }

    /// Opens (creating + migrating) the announcements DB at an explicit path.
    pub fn open_path(path: impl Into<PathBuf>) -> Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "failed to create announcements DB directory {}",
                    parent.display()
                )
            })?;
        }
        let connection = Connection::open(&path)
            .with_context(|| format!("failed to open announcements DB {}", path.display()))?;
        let store = Self { connection };
        store.migrate()?;
        Ok(store)
    }

    /// Creates the schema additively. Never drops data — unlike the metadata
    /// cache, this DB is the source of truth for claims.
    fn migrate(&self) -> Result<()> {
        self.connection
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS announcements (
                     id TEXT PRIMARY KEY,
                     source_id TEXT NOT NULL,
                     session_id TEXT NOT NULL,
                     scope TEXT NOT NULL,
                     message TEXT NOT NULL,
                     work_dir TEXT,
                     created_at TEXT NOT NULL,
                     expires_at TEXT NOT NULL
                 );
                 CREATE INDEX IF NOT EXISTS idx_announcements_session
                     ON announcements(source_id, session_id);
                 CREATE INDEX IF NOT EXISTS idx_announcements_expires
                     ON announcements(expires_at);",
            )
            .context("failed to migrate announcements DB")
    }

    /// Publishes a claim, returning the stored row. Sweeps expired rows first so
    /// the table never accumulates stale claims.
    pub fn publish(&self, new: NewAnnouncement) -> Result<Announcement> {
        self.sweep_expired()?;
        let now = Utc::now();
        let ttl = new.ttl.unwrap_or(DEFAULT_CLAIM_TTL);
        let announcement = Announcement {
            id: format!("ann-{}", uuid::Uuid::new_v4()),
            source_id: new.source_id,
            session_id: new.session_id,
            scope: normalize_scope(&new.scope),
            message: new.message,
            work_dir: new.work_dir,
            created_at: now,
            expires_at: now + ttl,
        };
        self.connection
            .execute(
                "INSERT INTO announcements
                     (id, source_id, session_id, scope, message, work_dir, created_at, expires_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![
                    announcement.id,
                    announcement.source_id,
                    announcement.session_id,
                    announcement.scope,
                    announcement.message,
                    announcement.work_dir,
                    announcement.created_at.to_rfc3339(),
                    announcement.expires_at.to_rfc3339(),
                ],
            )
            .context("failed to insert announcement")?;
        Ok(announcement)
    }

    /// Releases (deletes) claims. With `scope`, only matching scopes are removed;
    /// otherwise every claim for the session is cleared. Returns rows removed.
    pub fn release(&self, source_id: &str, session_id: &str, scope: Option<&str>) -> Result<usize> {
        let removed = match scope {
            Some(scope) => self.connection.execute(
                "DELETE FROM announcements
                 WHERE source_id = ?1 AND session_id = ?2 AND scope = ?3",
                params![source_id, session_id, normalize_scope(scope)],
            ),
            None => self.connection.execute(
                "DELETE FROM announcements WHERE source_id = ?1 AND session_id = ?2",
                params![source_id, session_id],
            ),
        }
        .context("failed to release announcements")?;
        Ok(removed)
    }

    /// Returns all non-expired claims, newest first.
    pub fn list_active(&self) -> Result<Vec<Announcement>> {
        self.sweep_expired()?;
        let now = Utc::now().to_rfc3339();
        let mut statement = self
            .connection
            .prepare(
                "SELECT id, source_id, session_id, scope, message, work_dir, created_at, expires_at
                 FROM announcements
                 WHERE expires_at > ?1
                 ORDER BY created_at DESC",
            )
            .context("failed to prepare announcement list")?;
        let rows = statement
            .query_map(params![now], row_to_announcement)
            .context("failed to query announcements")?;
        let mut announcements = Vec::new();
        for row in rows {
            announcements.push(row.context("failed to read announcement row")?);
        }
        Ok(announcements)
    }

    /// Returns non-expired claims whose scope matches `path` (exact, glob, or
    /// basename), newest first. This is the read path `recall_file` uses.
    pub fn matching(&self, path: &str) -> Result<Vec<Announcement>> {
        let target = normalize_scope(path);
        Ok(self
            .list_active()?
            .into_iter()
            .filter(|announcement| scope_matches(&announcement.scope, &target))
            .collect())
    }

    /// Deletes every claim whose `expires_at` is in the past. Best-effort; called
    /// before reads/writes so stale rows never surface.
    pub fn sweep_expired(&self) -> Result<usize> {
        let now = Utc::now().to_rfc3339();
        let removed = self
            .connection
            .execute(
                "DELETE FROM announcements WHERE expires_at <= ?1",
                params![now],
            )
            .context("failed to sweep expired announcements")?;
        Ok(removed)
    }
}

fn row_to_announcement(row: &rusqlite::Row<'_>) -> rusqlite::Result<Announcement> {
    Ok(Announcement {
        id: row.get(0)?,
        source_id: row.get(1)?,
        session_id: row.get(2)?,
        scope: row.get(3)?,
        message: row.get(4)?,
        work_dir: row.get(5)?,
        created_at: parse_time(row.get::<_, String>(6)?),
        expires_at: parse_time(row.get::<_, String>(7)?),
    })
}

fn parse_time(raw: String) -> DateTime<Utc> {
    DateTime::parse_from_rfc3339(&raw)
        .map(|time| time.with_timezone(&Utc))
        .unwrap_or_else(|_| Utc::now())
}

/// Normalizes a scope/path: trims, and strips a single trailing slash so
/// `src/` and `src` compare equal. Does not resolve symlinks or `..`.
fn normalize_scope(scope: &str) -> String {
    let trimmed = scope.trim();
    trimmed
        .strip_suffix('/')
        .filter(|stripped| !stripped.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

/// True when a stored claim `scope` covers a queried `target` path.
///
/// Matching is intentionally generous so a caller need not reproduce the exact
/// string a publisher used:
/// 1. exact (after normalization);
/// 2. glob — `scope` containing `*`/`?`/`[` is matched as a path glob against
///    `target` (and, when `scope` has no slash, against `target`'s basename);
/// 3. basename — a slash-free literal `scope` matches any `target` with that
///    file name, so `auth.rs` covers `crates/core/src/auth.rs`;
/// 4. prefix — a `scope` that is an ancestor directory of `target`.
pub fn scope_matches(scope: &str, target: &str) -> bool {
    let scope = normalize_scope(scope);
    let target = normalize_scope(target);
    if scope == target {
        return true;
    }
    let is_glob = scope.contains(['*', '?', '[']);
    if is_glob {
        if glob_match(&scope, &target) {
            return true;
        }
        if !scope.contains('/') {
            if let Some(base) = basename(&target) {
                if glob_match(&scope, base) {
                    return true;
                }
            }
        }
        return false;
    }
    // Slash-free literal: treat as a basename claim.
    if !scope.contains('/') && basename(&target) == Some(scope.as_str()) {
        return true;
    }
    // Path-suffix equivalence: a relative claim (`crates/core/src/auth.rs`) and
    // an absolute query (`/home/me/proj/crates/core/src/auth.rs`) name the same
    // file. Match when one path is a `/`-boundary suffix of the other.
    if path_suffix_eq(&scope, &target) {
        return true;
    }
    // Directory prefix: scope is an ancestor of target.
    target.starts_with(&format!("{scope}/"))
}

/// True when `a` and `b` denote the same file via a path-component suffix: one is
/// equal to, or ends with `"/" + other`. Avoids false hits like `auth.rs` vs
/// `oauth.rs` by requiring a slash boundary.
fn path_suffix_eq(a: &str, b: &str) -> bool {
    let (short, long) = if a.len() <= b.len() { (a, b) } else { (b, a) };
    if short == long {
        return true;
    }
    long.ends_with(short) && long.as_bytes()[long.len() - short.len() - 1] == b'/'
}

fn basename(path: &str) -> Option<&str> {
    path.rsplit('/')
        .next()
        .filter(|segment| !segment.is_empty())
}

/// Minimal path-aware glob: `*` matches any run of non-`/` characters, `**`
/// matches across `/` (including none), `?` matches one non-`/` char, `[...]` a
/// char class. Anchored at both ends. Recursive so mixed `**` and `*` compose
/// correctly without pulling in a glob crate.
fn glob_match(pattern: &str, text: &str) -> bool {
    glob_recurse(pattern.as_bytes(), text.as_bytes())
}

fn glob_recurse(pattern: &[u8], text: &[u8]) -> bool {
    match pattern.first() {
        None => text.is_empty(),
        Some(b'*') => {
            if pattern.get(1) == Some(&b'*') {
                // `**` — match zero or more of anything, including `/`.
                let rest = &pattern[2..];
                // `**/` may collapse to nothing, so a pattern like
                // `src/**/*.rs` still covers a file directly under `src`.
                if rest.first() == Some(&b'/') && glob_recurse(&rest[1..], text) {
                    return true;
                }
                // Try consuming nothing, then one char at a time.
                if glob_recurse(rest, text) {
                    return true;
                }
                (0..text.len()).any(|skip| glob_recurse(rest, &text[skip + 1..]))
            } else {
                // `*` — match zero or more non-`/` characters.
                let rest = &pattern[1..];
                if glob_recurse(rest, text) {
                    return true;
                }
                let mut index = 0;
                while index < text.len() && text[index] != b'/' {
                    index += 1;
                    if glob_recurse(rest, &text[index..]) {
                        return true;
                    }
                }
                false
            }
        }
        Some(b'?') => {
            !text.is_empty() && text[0] != b'/' && glob_recurse(&pattern[1..], &text[1..])
        }
        Some(b'[') => {
            if text.is_empty() {
                return false;
            }
            match match_class(pattern, text[0]) {
                Some((matched, consumed)) => {
                    matched && glob_recurse(&pattern[consumed..], &text[1..])
                }
                // Unterminated class: treat `[` as a literal.
                None => {
                    !text.is_empty() && text[0] == b'[' && glob_recurse(&pattern[1..], &text[1..])
                }
            }
        }
        Some(&ch) => !text.is_empty() && text[0] == ch && glob_recurse(&pattern[1..], &text[1..]),
    }
}

/// Matches a `[...]` class at the start of `pattern` against `ch`. Returns
/// `(matched, bytes_consumed_in_pattern)`. Supports negation `[!...]` and ranges.
fn match_class(pattern: &[u8], ch: u8) -> Option<(bool, usize)> {
    debug_assert_eq!(pattern.first(), Some(&b'['));
    let mut index = 1;
    let negate = pattern.get(index) == Some(&b'!');
    if negate {
        index += 1;
    }
    let mut matched = false;
    while index < pattern.len() && pattern[index] != b']' {
        if pattern.get(index + 1) == Some(&b'-')
            && pattern.get(index + 2).is_some_and(|c| *c != b']')
        {
            let low = pattern[index];
            let high = pattern[index + 2];
            if low <= ch && ch <= high {
                matched = true;
            }
            index += 3;
        } else {
            if pattern[index] == ch {
                matched = true;
            }
            index += 1;
        }
    }
    if index >= pattern.len() {
        return None; // unterminated class
    }
    index += 1; // consume ']'
    Some((matched != negate, index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> AnnouncementStore {
        let path = std::env::temp_dir().join(format!(
            "brick-ann-{}.sqlite",
            Utc::now().timestamp_nanos_opt().unwrap_or_default()
        ));
        AnnouncementStore::open_path(path).expect("open store")
    }

    fn claim(scope: &str, message: &str) -> NewAnnouncement {
        NewAnnouncement {
            source_id: "claude_code".into(),
            session_id: "sess-1".into(),
            scope: scope.into(),
            message: message.into(),
            work_dir: Some("/repo".into()),
            ttl: None,
        }
    }

    #[test]
    fn publish_and_match_exact() {
        let store = store();
        store
            .publish(claim("crates/core/src/auth.rs", "refactoring"))
            .unwrap();
        let hits = store.matching("crates/core/src/auth.rs").unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].message, "refactoring");
    }

    #[test]
    fn basename_claim_matches_full_path() {
        let store = store();
        store.publish(claim("auth.rs", "hands off")).unwrap();
        assert_eq!(store.matching("crates/core/src/auth.rs").unwrap().len(), 1);
        assert_eq!(store.matching("other.rs").unwrap().len(), 0);
    }

    #[test]
    fn glob_claim_matches() {
        let store = store();
        store
            .publish(claim("crates/core/src/sources/*.rs", "reworking sources"))
            .unwrap();
        assert_eq!(
            store
                .matching("crates/core/src/sources/orgii.rs")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            store
                .matching("crates/core/src/sources/sub/deep.rs")
                .unwrap()
                .len(),
            0
        );
        assert_eq!(store.matching("crates/cli/src/main.rs").unwrap().len(), 0);
    }

    #[test]
    fn double_star_glob_crosses_slashes() {
        let store = store();
        store
            .publish(claim("crates/**/*.rs", "big refactor"))
            .unwrap();
        assert_eq!(
            store
                .matching("crates/core/src/sources/orgii.rs")
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn directory_prefix_claim_matches_children() {
        let store = store();
        store
            .publish(claim("crates/core/src/sources", "whole module"))
            .unwrap();
        assert_eq!(
            store
                .matching("crates/core/src/sources/orgii.rs")
                .unwrap()
                .len(),
            1
        );
        assert_eq!(store.matching("crates/core/src/lib.rs").unwrap().len(), 0);
    }

    #[test]
    fn release_by_scope_and_by_session() {
        let store = store();
        store.publish(claim("a.rs", "x")).unwrap();
        store.publish(claim("b.rs", "y")).unwrap();
        assert_eq!(
            store
                .release("claude_code", "sess-1", Some("a.rs"))
                .unwrap(),
            1
        );
        assert_eq!(store.list_active().unwrap().len(), 1);
        assert_eq!(store.release("claude_code", "sess-1", None).unwrap(), 1);
        assert_eq!(store.list_active().unwrap().len(), 0);
    }

    #[test]
    fn expired_claims_are_swept() {
        let store = store();
        let new = NewAnnouncement {
            ttl: Some(Duration::seconds(-1)),
            ..claim("a.rs", "stale")
        };
        store.publish(new).unwrap();
        assert_eq!(store.list_active().unwrap().len(), 0);
        assert_eq!(store.matching("a.rs").unwrap().len(), 0);
    }

    #[test]
    fn char_class_glob() {
        assert!(scope_matches("v[0-9].rs", "v3.rs"));
        assert!(!scope_matches("v[0-9].rs", "vx.rs"));
        assert!(scope_matches("file[!x].rs", "filea.rs"));
        assert!(!scope_matches("file[!x].rs", "filex.rs"));
    }

    #[test]
    fn question_mark_glob_single_char() {
        assert!(scope_matches("a?.rs", "ab.rs"));
        assert!(!scope_matches("a?.rs", "abc.rs"));
    }

    #[test]
    fn relative_claim_matches_absolute_query() {
        // A claim made with a repo-relative path covers an absolute query for
        // the same file, and vice versa — but not a different file with a
        // confusingly similar name.
        assert!(scope_matches(
            "crates/core/src/auth.rs",
            "/home/me/proj/crates/core/src/auth.rs"
        ));
        assert!(!scope_matches("src/auth.rs", "/home/me/oauth.rs"));
    }

    #[test]
    fn double_star_slash_collapses_to_direct_child() {
        // `src/**/*.rs` must cover a file directly under `src` (zero dirs).
        assert!(scope_matches(
            "crates/cli/src/**/*.rs",
            "crates/cli/src/commands.rs"
        ));
        assert!(scope_matches(
            "crates/cli/src/**/*.rs",
            "crates/cli/src/sub/deep.rs"
        ));
    }
}
