---
status: active
---

# ORGII `orgtrack-core` Offload Inventory

This document answers whether ORGII's `orgtrack-core` should move into Brick.

Short answer: **yes, most of `orgtrack-core` is Brick's missing source/provenance core, but it should be absorbed, renamed, and split rather than copied wholesale.** ORGII should keep UI/runtime orchestration and become a consumer/adapter over Brick APIs.

## Migration principle

Do not move `orgtrack-core` as an opaque crate. Instead:

1. Port provider-neutral data contracts into Brick protocol/core.
2. Port external source providers one by one with tests and JSON parity fixtures.
3. Replace ORGII Tauri commands with thin shell/RPC adapters around Brick.
4. Leave ORGII-specific live session runtime and UI state in ORGII.
5. Delete duplicate ORGII source/index logic after parity.

## Buckets

| Bucket | Meaning |
| --- | --- |
| Move to Brick | Generic provenance, source querying, metadata indexing, chunk formatting, blame, or privacy policy logic. |
| Split / refactor | Valuable logic that is currently entangled with ORGII-specific DTOs, DB schema, or runtime assumptions. |
| Keep in ORGII | UI, Tauri registration, live runtime orchestration, and app-specific scheduling. |

## Move to Brick

| ORGII area | Current location | Why it belongs in Brick | Brick target |
| --- | --- | --- | --- |
| Imported history source framework | `orgtrack-core/src/sources/imported_history/*` | This is the generic metadata-index pattern for external/native sources. | `brick-core::sources`, `brick-core::metadata_db`, provider traits. |
| Imported-history metadata index helpers | `imported_history/cache.rs`, `metadata.rs` | Defines discovery signatures, upsert/list/recent-path queries, pruning, changed-record detection, impact stats. | Expand `metadata.sqlite` schema and query APIs. |
| Cursor IDE source provider | `sources/cursor_ide/*` | Mature Cursor DB scanner, composer parser, bubble loader, window APIs, summaries. | `brick-core::sources::cursor_ide`; lazy DB chunk provider. |
| Claude Code source provider | `sources/claude_code/history.rs` | JSONL discovery, metadata extraction, token/impact stats, chunk loading. | `brick-core::sources::claude_code`. |
| Codex App source provider | `sources/codex/app.rs` | JSONL discovery, metadata extraction, patch impact stats, chunk loading. | `brick-core::sources::codex_app`. |
| OpenCode source provider | `sources/opencode/history.rs` | SQLite metadata and chunk reader. | `brick-core::sources::opencode`. |
| Windsurf source provider | `sources/windsurf/history.rs` | Cursor-family DB scanner/chunk reader. | Shared Cursor-family provider module. |
| Recent-path aggregation | imported-history recent path helpers and per-source recent path commands | Recent paths are a metadata-index query, not UI-specific logic. | `brick history recent-paths`. |
| Impact stats extraction | imported-history impact stats and provider parsers | Changed files/line counts are provenance metadata. | Provider metadata extraction and optional evidence projections. |
| Lazy chunk formatting | imported source record formatters to `ActivityChunk` | Source-record JSON formatting belongs in Brick as source APIs. | `brick history chunks` JSON and library API. |
| Source diagnostics | ORGII debug/diagnostic source commands | Provider health and parser diagnostics are Brick source concerns. | `brick source doctor`, `brick history doctor`. |
| Source discovery candidates | per-provider path candidate functions | Brick should discover native agent data roots directly. | `brick source scan`, `brick source config`, provider discovery. |
| Parser versioning | provider parser constants | Brick must own source parser versions for metadata invalidation. | Provider version constants and migration policy. |

## Split / refactor before moving

| ORGII area | Current location | Refactor needed | Brick target |
| --- | --- | --- | --- |
| Canonical records | `orgtrack-core/src/canonical.rs` | Separate ORGII/UI-specific fields from generic provenance/session/activity/file-change records. | `brick-protocol` events and DTOs; compatibility JSON for ORGII. |
| SQLite store | `orgtrack-core/src/store/sqlite.rs` | Split source metadata index tables from ORGII app tables and old stats caches. | `metadata.sqlite` source index plus repo-local derived indexes. |
| Projectors | `orgtrack-core/src/projectors/*` | Make projections rebuildable from Brick events/source metadata; remove app-specific assumptions. | `brick-core::projectors`. |
| Session blame | `projectors/session_blame.rs` | Align naming and evidence model with Brick Mission/Session/Artifact concepts. | `brick blame`, `brick history blame`, or projection API. |
| Stats projection | `projectors/stats.rs` | Separate raw source stats, provider impact stats, and provenance rollups. | Source metadata stats and mission/session projections. |
| Edit extraction | `edit_extraction.rs` and provider edit parsing | Keep provider-specific patch parsing, but expose generic file-change evidence. | Provider parsers + Brick `FileChange`/artifact events. |
| Sync export | `sync_export.rs` | ORGII export format should become Brick protocol events or compatibility JSON. | `brick export`, `brick sync`, ORGII adapter. |
| Privacy policy | `privacy/*` | Keep useful tiering/redaction primitives; remove ORGII product-specific assumptions. | Brick evidence availability and redaction policy. |
| Repository sync | `repo_sync/*` | Align repo context and merge/worktree concepts with Brick's event model. | Brick repo context capture and projections. |
| Analysis backfill inputs | consumers in `src-tauri/src/orgtrack/analysis_backfill.rs` | Backfill scheduling is ORGII-specific; extraction logic can move. | Brick library extraction APIs; ORGII schedules calls. |

## Keep in ORGII

| ORGII area | Why it stays | Desired future shape |
| --- | --- | --- |
| Tauri command registration | Tauri is ORGII app plumbing. | Thin commands call Brick library/RPC/CLI and translate responses. |
| Frontend TypeScript wrappers | UI API layer is ORGII-specific. | Wrappers consume Brick-shaped JSON DTOs. |
| GlobalSpotlight hooks and UI state | Product UI behavior. | Query Brick APIs for external history rows/recent paths. |
| Live ORGII agent runtime sessions | ORGII owns process/session orchestration for active sessions. | Runtime emits Brick provenance events or exports evidence. |
| Cursor live automation / IDE bridge | ORGII integration surface, not historical metadata indexing. | May attach evidence to Brick, but not part of Brick source scanner. |
| Analysis scheduling and memory gates | App lifecycle policy. | Call Brick extraction/projection APIs where useful. |
| EventStore UI sync | ORGII state-management detail. | Consume Brick updates rather than duplicating scanner state. |

## Proposed Brick module shape

```text
crates/core/src/
  metadata_db.rs
  sources/
    mod.rs
    traits.rs
    discovery.rs
    imported_history.rs
    cursor_family/
      mod.rs
      sqlite_kv.rs
      composer.rs
      chunks.rs
    cursor_ide.rs
    windsurf.rs
    claude_code.rs
    codex_app.rs
    opencode.rs
  history/
    dto.rs
    sessions.rs
    chunks.rs
    recent_paths.rs
  projectors/
    mod.rs
    stats.rs
    blame.rs
    file_changes.rs
  privacy/
    mod.rs
```

CLI surface:

```text
brick source scan
brick source doctor
brick source config
brick history sources
brick history refresh
brick history sessions
brick history recent-paths
brick history chunks
brick history export
brick blame session
```

## Migration slices

### Slice 1: metadata index parity for file-backed sources

- Port Claude Code metadata parser.
- Port Codex App metadata parser.
- Add metadata schema columns needed by ORGII rows: model, tokens, repo path, branch, impact stats, touched files, listability, source metadata JSON.
- Add JSON parity fixtures against ORGII DTOs.

### Slice 2: lazy chunks for file-backed sources

- Port Claude Code chunk formatter.
- Port Codex App chunk formatter.
- Add `brick history chunks --source ... --session-id ... --format json`.
- Preserve ORGII-compatible `ActivityChunk` JSON during migration.

### Slice 3: DB-backed providers

- Port OpenCode provider.
- Port Cursor-family SQLite KV utility.
- Port Windsurf provider.
- Port Cursor IDE provider last, because it has the largest windowing/summary surface.

### Slice 4: ORGII adapter cutover

- Add feature flag in ORGII to call Brick for external history metadata and chunks.
- Keep ORGII command names but route implementation through Brick.
- Compare responses with old ORGII implementation in diagnostics.
- Delete duplicate ORGII scanner/cache code after parity.

### Slice 5: projections and blame

- Move generic projectors and blame logic into Brick.
- Rebuild stats/blame from Brick events plus source metadata rows.
- Keep ORGII UI rendering as a consumer.

## Risks and constraints

- Cursor and Windsurf DB schemas are private and can drift. Provider parser versions and diagnostics are mandatory.
- ORGII uses `ActivityChunk` today; Brick should preserve compatibility while defining its own stable history DTOs.
- `metadata.sqlite` must not become transcript storage. Chunk APIs should lazy-read native storage unless the user explicitly copies evidence into Brick blobs.
- Source rows need stable invalidation. File-backed sources can use mtime/size first; DB-backed sources need DB mtime/size plus per-session fingerprints when available.
- Avoid a second cache layer. The source metadata index is the durable query index for external history metadata.

## Conclusion

Most of `orgtrack-core` should migrate into Brick because it implements exactly the cross-app source querying, metadata indexing, source-record formatting, projection, and blame logic Brick is meant to own. The migration should not be a crate move. It should be a sequence of provider and projection ports that leaves ORGII as a thin UI/runtime consumer over Brick's source and provenance APIs.
