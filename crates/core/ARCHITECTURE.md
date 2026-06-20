# Brick Core — Data Model & Architecture

This document is the orientation map for `brick-core`. It exists because the data
model has a few places where the *same English word* means different things, and
where the *same concept* is deliberately re-declared across layers. New work that
ignores these distinctions tends to introduce subtle bugs, so read this first.

## 1. Source of truth and derived caches

```
                 ┌─────────────────────────────┐
   append_event  │  JSONL queue  (SOURCE OF      │   <-- the ONLY truth
   ───────────►  │  TRUTH, append-only)          │
                 │  .brick/provenance/queue/*.jsonl
                 └──────────────┬──────────────┘
                                │ TraceIndex::build(read_all_events())
            ┌───────────────────┼────────────────────┐
            ▼                   ▼                     ▼
   index.json (JSON graph)  brick.sqlite          agent views
   IndexedOrg/Project/...   (query cache)         (markdown, human/agent)
   — rebuildable —          — rebuildable —       — rebuildable —
```

Rules (enforced by convention, stated in each module header):

- **The JSONL queue is authoritative.** Everything else can be deleted and
  rebuilt from `read_all_events()`. Never let a derived cache become the truth.
- `index.json` is the in-memory `TraceIndex` serialized. `brick.sqlite` is a flat
  query cache projected from the index.
- Two *additional* SQLite DBs have different truth semantics:
  - `metadata.sqlite` — a **rebuildable cache** of external-tool ("source")
    sessions. A schema bump triggers a full reset.
  - `announcements.sqlite` — **authored user intent, a source of truth.** It is
    migrated additively and never reset.

### Index loading

- `rebuild_index()` — always recompute from events, rewrite `index.json`, and
  regenerate the markdown views directory. Expensive (full scan + disk writes).
- `read_index()` — read the cache; returns `None` if absent.
- `load_or_rebuild_index()` — read the cache, but **rebuild if it is stale**
  (cached `event_count` != live `read_all_events().len()`). Prefer this for read
  paths so back-to-back reads in one flow don't each pay a full rebuild. The MCP
  read tools (`current_context`, `list_missions`, `show_mission`) use this.

## 2. Three layers of the same concept (intentional re-declaration)

A diff capture exists as three structs, each a hand-copied projection of the one
above it:

```
brick_protocol::DiffCapturedPayload   (wire / JSONL truth)
        │  index.rs copies field-by-field
        ▼
brick_core::IndexedDiff               (in-memory graph; adds aggregates)
        │  sqlite_index.rs flattens to columns
        ▼
SqliteDiffRecord                      (flat query-cache row)
```

The same pattern holds for sessions, artifacts, attachments, session logs, repo
contexts. The copies are manual, so **adding a field to the payload does not
automatically propagate** — you must update each layer that needs it.

### The deliberate exception: `patch_id`

`DiffFileChange.patch_id` (in the payload) is **intentionally NOT mirrored** into
`IndexedDiffFileChange` or the SQLite cache. Line-level owner blame is
*events-authoritative*: `blame::blame_file` reads `patch_id` straight from the
JSONL event stream, never from a cache. Mirroring it would invite "blame from the
cache", which would silently mis-attribute whenever the cache lagged the queue. A
regression test (`blame::tests::indexed_diff_file_change_does_not_carry_patch_id`)
guards this.

## 3. Three `*Query` types — different backends, not interchangeable

| Type | Backend | Use when |
|---|---|---|
| `SessionQuery` | in-memory `TraceIndex` | you already have/loaded the index |
| `SqliteSessionQuery` | `brick.sqlite` | SQL-filterable queries over the cache |
| `SourceSessionListQuery` / `SourceSessionTextQuery` | `metadata.sqlite` | external-tool session metadata / FTS |

They share overlapping field names (`app_id`, `actor_id`, `limit`) with subtly
different semantics. Pick the one whose backend you are actually querying.

## 4. Three session id spaces

```
SessionId                — Brick's own typed id  (prefixed UUID, "session_…")
external_session_id      — the id the EXTERNAL tool assigned (string, opaque)
i64 source_session_id    — the metadata.sqlite ROWID for a source session
```

They are joined only through `brick_session_source_sessions`. When you see
"session id" in code, determine which space it belongs to before comparing or
joining. `source_id` is the *external tool name* (e.g. `claude_code`), NOT a
session id — and note the index/event path calls the same value `app_id`.

## 5. Line-blame is OWNER PROVENANCE, not whole-file authorship

`blame_file` answers a *closed* question: **"which lines of this file can this
machine's local AI session history attribute to an agent?"** — not "who wrote
every line".

- Lines changed by others (no local session), edited by hand, or never captured
  are **skipped by design**, reported as a `skipped_lines` count, not guessed.
  `unattributed` is a correct answer ("not in my records"), not a failure.
- Owner scope = **all AI sessions on this machine** across every tool
  (Claude/Codex/Cursor/ORGII…), matching Brick's cross-tool memory model.

### Failure boundary (by design, do not "fix")

Owner attribution is a point-in-time capture (a diff + its `patch_id`) matched
against an evolving tree. Operations that rewrite history — `git commit --squash`,
rebase, large rewrites — change the patch identity, so previously-owned lines
become unattributed. This is inherent: **any** join key (patch-id, content hash,
AST node) fails the same way, just at different thresholds. Line-blame is precise
on *recent, local, captured, un-rewritten* code and degrades honestly elsewhere.
Do not add "coverage" machinery to chase historical/collaborative code — that is
the job of session-level recall (`recall_file` / `search_sessions`), which never
claims line precision.

## 6. Semantic overloading — same word, different meanings

Watch for these when reading code:

- **"source"** — (a) `SourceProfile`: repo-local TOML config for an external
  tool; (b) `SourceProfileRecord`: a row in `metadata.sqlite`; (c) `source_id`:
  the external tool name string; (d) `StorageRootSource`: an unrelated enum for
  where the storage root was resolved from.
- **"status"** — five unrelated types: `MissionStatus`, `SourceScanStatus`,
  `IndexStatus`, `QueueStatus`, `SqliteIndexStatus`. Always read the type.
- **"claim"** — (a) an *announcement* (a session claiming "I'm working on X");
  (b) a *negative* assertion in blame/diff docs ("does NOT claim line-level
  authorship"). Opposite intent, same word.
- **"record"** — (a) a DB read-DTO (`SourceSessionRecord`, `SqliteDiffRecord`,
  …); (b) the act of logging an event ("recorded", `recorded_at`).

These are documented rather than renamed: renaming is high-risk, low-reward, and
the wire/serde names are stable. Prefer reading the type over the bare word.
