---
status: active
---

# Brick Handoff Summary

This document captures the current state of Brick so another agent can continue from the current implementation state.

## Product direction

Brick is a standalone, publishable, self-host-first Rust CLI and server for accountable work provenance. The product model is:

```text
Brick Org
  -> Brick Project
    -> Brick Mission
      -> Brick Session
      -> Brick Artifact
```

Key principles:

- Git remains the source of truth for code.
- Brick is the source of truth for execution/provenance history.
- Humans primarily manage `missions`.
- Agent and human work are both represented as `sessions`.
- Full transcripts or recordings should not be duplicated locally by default.
- Local Brick records default to metadata-only pointers; copying evidence bytes is explicit.
- Long term, `.brick` should become the unified local metadata/provenance root for external coding-app history and Brick's own org/project/mission/session ledger.
- `.orgii` should be narrowed to ORGII-owned runtime sessions, CLI state, and app-private state. ORGII should eventually consume Brick metadata instead of maintaining a parallel external-history metadata store.
- The local JSONL event log is the source of truth for provenance claims. SQLite and Markdown views are derived query/readability layers.
- `.brick/` is local state and is automatically added to `.gitignore` by `brick init` when Brick is initialized inside a repo.

## Repository status

Recent commits:

- `e12182d` — `Update Brick product model and source discovery`
  - Pushed to `origin/main`.
- `88df213` — `Add native source session import`
  - Pushed to `origin/main`.

Current working tree includes the first ORGII offload implementation slice:

- `BRICK_HOME` resolution and `metadata.sqlite` schema/API skeleton.
- Metadata-backed `brick history` JSON command surface for sources, sessions, recent paths, and placeholder chunks.
- Updated handoff/docs for the Brick-owned external-history direction.

Before continuing, run:

```bash
git status --short --branch
```

If the user wants the latest native-import work on remote, push:

```bash
git push origin HEAD
```

## Current architecture

### Pivot: unified Brick metadata root

The latest product decision is that Brick should not be primarily a per-repo `.brick` folder that duplicates ORGII or external-app metadata. Instead, Brick should move toward a unified local metadata root, with repo/org/project/mission used as filter/sync dimensions.

Target model:

```text
~/.brick/ or configured BRICK_HOME
  metadata.sqlite          # source metadata index
  events/                  # provenance ledger events
  sources/                 # source profiles and parser metadata
  views/                   # derived agent-readable views
  blobs/                   # optional copied evidence blobs only
```

Repo-local `.brick/` can still exist for lightweight bootstrap/config or repo-specific overrides, but should not be required as the only storage model. If Brick is initialized in a repo, `.brick/` stays gitignored. If Brick uses global storage, it should bind sessions to repo/workspace roots through explicit metadata fields rather than by placing all state in each repo.

This matters because a single agent session can touch multiple roots/workspaces. The unified DB should model:

- one native/source session
- zero or more workspace roots
- zero or more Git repos/branches/commits
- zero or more Brick org/project/mission links

Sync should then filter by `org_id` / project / mission / repo context, not by assuming one repo-local `.brick` directory contains all relevant metadata.

### Crates

- `crates/cli` — `brick` CLI.
- `crates/server` — `brick-server` self-hosted remote.
- `crates/protocol` — event schema, typed IDs, sync wire types.
- `crates/core` — local storage, derived indexing, source metadata index, source profiles, discovery, native source listing.
- `crates/importers` — explicit-file import normalization.

### Global metadata home

First-stage global metadata support is implemented in:

```text
crates/core/src/global_home.rs
crates/core/src/metadata_db.rs
```

Current behavior:

- `BRICK_HOME` overrides the global Brick home.
- Default global Brick home is `~/.brick`.
- Unified metadata DB path is `<BRICK_HOME>/metadata.sqlite`.
- The metadata DB has schema versioning and resets first-stage source metadata index tables on incompatible version mismatch.
- Implemented typed APIs include `MetadataDb::open_global`, `MetadataDb::open_in_home`, `MetadataDb::open_path`, `upsert_source_session`, `list_source_sessions`, and `count_source_sessions`.
- `brick history sessions`, `brick history recent-paths`, and `brick import native list/ingest` now refresh native source-session rows into `MetadataDb` before returning results.
- Existing repo-local JSONL provenance flow remains unchanged.

### Local storage

Current implemented default store is repo-local:

```text
.brick/
  config.toml
  sources/<name>.toml
  provenance/
    repo.json
    events/queue/*.jsonl
    events/inbound/*.jsonl
    cache/index.json
    cache/brick.sqlite
    views/
      orgs/*.md
      projects/*.md
      missions/*.md
      sessions/*.md
      artifacts/*.md
    blobs/sha256/<hash>
```

Important current behavior:

- `LocalStore::init()` creates provenance directories and repo metadata.
- `LocalStore::init()` also ensures `.brick/` is present in `.gitignore`, idempotently.
- `events/queue` and `events/inbound` are the event sources for local and pulled events.
- `index.json`, `brick.sqlite`, and `views/` are rebuildable.
- SQLite schema versioning is implemented; incompatible derived DBs are reset and rebuilt.

Planned storage direction:

- Move external-history metadata index tables into a unified Brick DB under a global/configured Brick root.
- Keep repo-local `.brick` as optional bootstrap/config only, not as the only metadata home.
- Represent repo/workspace roots explicitly in the DB because sessions may span multiple workspaces.
- Use `org_id` and related links as sync filters.

## Core domain model

Important IDs and entities:

- `OrgId` — sync boundary, similar to a repo/org namespace.
- `ProjectId` — groups missions.
- `MissionId` — human-managed unit of work; replaces earlier “work item” language.
- `SessionId` — agent or human execution/work session.
- `ArtifactId` — decisions, notes, reviews, test results, etc.

Mission statuses:

- `planned`
- `active`
- `blocked`
- `completed`
- `archived`

Evidence availability:

- `local_pointer` — default; Brick stores metadata and path/URI only.
- `local_blob` — copied into content-addressed local blob storage.
- `remote_blob` — available remotely.

## CLI shape

Current major command groups:

```bash
brick init
brick org ...
brick project ...
brick mission ...
brick session ...
brick artifact ...
brick evidence ...
brick import ...
brick source ...
brick history ...
brick sync ...
brick maintenance ...
```

Old commands were intentionally replaced, not kept as aliases.

## Source profiles, discovery, and ORGII migration

Repo-level config:

```text
.brick/config.toml
```

Source profiles:

```text
.brick/sources/<name>.toml
```

`SourceProfile` includes:

- `name`
- `app_id`
- `actor_id`
- `actor_type`
- `store_root`
- `session_db_path`
- `session_log_path`
- `evidence_root`
- `cursor_state_db_path`
- `default_full_evidence_upload`
- `notes`

Implemented source discovery lives in:

```text
crates/core/src/source_discovery.rs
```

It scans common default paths for:

- ORGII
- Cursor
- Claude Code
- Codex
- OpenCode

Important ORGII context:

- ORGII already has hardcoded external-history readers and metadata stores for Cursor IDE, Codex App, Claude Code, OpenCode, and Windsurf.
- Cursor uses the existing ORGII table named `cursor_session_cache` after read-only delta sync from Cursor `state.vscdb`.
- Non-Cursor imported history uses the existing ORGII table named `imported_history_session_cache` keyed by source and source session ID.
- The source-specific loading mechanisms currently live in ORGII: when a user opens an external session, ORGII knows how to re-open the native DB/JSONL/source path, parse the relevant transcript/window, and produce `ActivityChunk` records for rendering.
- Those loaders are not just metadata helpers. They are operational source readers for external history chunk formatting, so migration must move/abstract them into Brick history providers rather than only copying metadata schemas.
- ORGII stores metadata rows and reads transcript chunks lazily from source paths/DBs when rendering read-only history.
- Brick should eventually absorb/migrate the entire ORGII external-history subsystem, not only scan/index tables.
- Scope includes scanners, delta indexing, source-specific parsers, source-specific loading/windowing mechanisms, chunk loaders, `ActivityChunk` normalization, recent paths, impact stats, analysis backfills, diagnostics, and source-specific debug helpers.
- In that future, ORGII is just one consumer of Brick-provided metadata/transcripts, like any other UI, rather than the owner of external-history indexing/parsing/loading/backfill logic.

Relevant CLI:

```bash
brick source scan
brick source scan --write-defaults
```

`brick init` runs discovery automatically:

- In an interactive TTY, it can prompt the user to select discovered sources with arrow keys / space / enter.
- In non-TTY/script mode, it prints findings and does not block.

## Consolidated ORGII external-history offload plan

The subagent audits converged on the same boundary: this is not just a scanner migration. Brick should absorb ORGII's portable external-history subsystem, while ORGII keeps app/UI/runtime orchestration.

### Move into Brick

Portable external-history core:

- Source discovery and configured source roots.
- Source-specific scanners for Cursor IDE, Claude Code, Codex App, OpenCode, and Windsurf.
- Delta indexing algorithms: source path, mtime, size, fingerprint, parser version, live IDs, pruning, and changed-record detection.
- Source-specific parsers and raw DTOs.
- Source-specific loading/windowing mechanisms that currently live in ORGII and reopen native DB/JSONL records on demand.
- `ActivityChunk` JSON formatting for read-only external history.
- Recent-path aggregation and repo/workspace inference.
- Impact stats: touched files, files changed, lines added/removed, model/token metadata.
- Parser diagnostics, parse errors, source status, and source-index debug commands.
- Cursor turn-summary/window APIs as source-history read APIs, not as UI state.

Potentially move later, depending on product boundary:

- Analysis/backfill logic that reopens external histories, converts chunks into raw activity, extracts edits/shell commits, writes inferred artifacts/checkpoints, and computes watermarks.
- If moved, Brick needs equivalent operational controls for memory gates, panic isolation, and long-running analysis status.

### Keep in ORGII

ORGII should remain responsible for:

- Tauri command registration and compatibility wrappers while the UI migrates.
- UI session list merging, pagination atoms, display groups, icons, and read-only routing.
- EventStore/rendering pipeline and `processChunksRust` until a later UI refactor.
- ORGII-owned live/runtime sessions.
- Cursor live automation: debug-port lifecycle, send/watch/unwatch, model/mode setting.
- ORGII repo/workspace import side effects from recent paths.
- UI dashboards and app-specific diagnostics/toasts.

### Brick API surface needed by ORGII

Add a Brick-owned history query surface, with JSON output first so ORGII can shell out before taking a Rust crate dependency:

```bash
brick history sources --format json
brick history sessions --source <source_id> --limit 200 --offset 0 --format json
brick history recent-paths --source <source_id|all> --limit 20 --format json
brick history chunks --source <source_id> --session-id <id> --format json
brick history export --source <source_id> --session-id <id> --schema audit-v1 --format json
brick history export --source <source_id> --session-id <id> --schema audit-v1 --format csv
```

Cursor-specific read APIs should be preserved because ORGII's Cursor UI uses windowed loading:

```bash
brick history cursor initial-window --session-id <id> --recent-limit 100 --format json
brick history cursor full-refresh --session-id <id> --format json
brick history cursor turn-window --session-id <id> --user-bubble-id <id> --format json
```

The DTOs should initially match ORGII-compatible wire shapes:

- source catalog rows
- session rows/pages
- recent paths
- `ActivityChunk`
- Cursor initial/full/turn windows
- parser/source-index diagnostics

### Recommended migration stages

1. Add `BRICK_HOME` and a unified metadata DB for the external source metadata index. — first skeleton implemented.
2. Add JSON history commands and make ORGII wrappers shell out behind feature flags. — first metadata-backed JSON surface implemented; ORGII wrappers still pending.
3. Move shared external-history DTOs and indexing algorithms into Brick.
4. Port file-based sources first: Claude Code and Codex App.
5. Add dedupe on `(source_id, external_session_id)` and a `--force` path.
6. Port OpenCode and Windsurf DB readers.
7. Port Cursor IDE read-only history: list, initial window, full refresh, turn window.
8. Keep Cursor live automation in ORGII.
9. Move/rewrite analysis backfill only after deciding whether Brick owns orgtrack analysis artifacts.
10. Remove ORGII external-history scanners/metadata stores after source-by-source parity and fallback retirement.

### Important schema implication

Brick needs more than the existing event projection DB. Add persistent local source metadata index tables that are not provenance claims:

- `source_profiles`
- `source_roots`
- `source_scans`
- `source_sessions`
- `source_session_resources`
- `workspace_roots`
- `git_repositories`
- `source_session_workspace_roots`
- `source_session_git_repositories`
- `brick_session_source_sessions`

Keep the semantic split clear:

- Source metadata index rows mean Brick observed external app metadata.
- JSONL provenance events mean Brick recorded an accountability/provenance claim.

## History JSON command surface

First metadata-backed history surface is implemented in:

```text
crates/cli/src/history.rs
```

CLI:

```bash
brick history sources --format json
brick history sessions --source <source_id> --limit 20 --offset 0 --format json
brick history recent-paths --source all --limit 20 --format json
brick history chunks --source <source_id> --session-id <native-id> --format json
brick history export --source <source_id> --session-id <native-id> --schema audit-v1 --format json
brick history export --source <source_id> --session-id <native-id> --schema audit-v1 --format csv
```

Current behavior:

- `sources` emits configured source profile rows.
- `sessions` refreshes native source file metadata into `<BRICK_HOME>/metadata.sqlite`, then reads stable JSON DTOs from `MetadataDb`.
- `recent-paths` refreshes one source or all configured sources into `MetadataDb`, then aggregates indexed recent paths.
- `chunks` currently returns an empty chunk array after validating the source profile exists; source-specific chunk loading remains pending.
- This surface is intended as the first ORGII-compatible bridge contract, not the final source parser/index implementation.

## Native source session import

First native importer slice is implemented in:

```text
crates/core/src/native_source.rs
```

Current behavior:

- Lists native session files under the selected profile’s `session_log_path` and/or `evidence_root`.
- Supports files ending in:
  - `.jsonl`
  - `.json`
  - `.txt`
  - `.log`
  - `.md`
  - `.markdown`
- Uses filename stem as `external_session_id`.
- Sorts recent files by modified time.
- `native list` and `native ingest` refresh listed/selected sessions into `MetadataDb`.
- Ingest records metadata-only evidence pointers by default.
- `native ingest` creates a new Brick `SessionId` unless `--session` is explicitly passed.

CLI:

```bash
brick --source claude_code import native list --limit 20
brick --source claude_code import native ingest --external-session-id <native-id> --mission <mission_id>
```

Smoke coverage was added in:

```text
scripts/smoke_mvp.sh
```

The smoke now creates a fake `claude_code` source profile, lists a native JSONL transcript, ingests it, and verifies sync/indexing with two sessions.

## Verification status

The following passed after the native importer work:

```bash
cargo fmt
cargo check
cargo test
cargo doc --no-deps
scripts/smoke_mvp.sh
```

The following passed after integrating the metadata DB and history JSON surface:

```bash
cargo fmt
cargo check
cargo run -q -p brick -- history sources --format json
cargo test -p brick-core -p brick
cargo doc --no-deps
scripts/smoke_mvp.sh
```

Lints were checked for edited files with no errors.

## Important files changed recently

First ORGII offload slice:

- `crates/core/src/global_home.rs`
- `crates/core/src/metadata_db.rs`
- `crates/cli/src/history.rs`
- `crates/core/src/lib.rs`
- `crates/cli/src/args.rs`
- `crates/cli/src/main.rs`
- `crates/cli/Cargo.toml`
- `README.md`
- `docs/architecture/handoff-summary.md`

Native import work:

- `crates/core/src/native_source.rs`
- `crates/core/src/lib.rs`
- `crates/cli/src/args.rs`
- `crates/cli/src/commands.rs`
- `README.md`
- `scripts/smoke_mvp.sh`

Earlier product model / source discovery work:

- `crates/protocol/src/ids.rs`
- `crates/protocol/src/events.rs`
- `crates/protocol/src/payloads.rs`
- `crates/protocol/src/trace_event.rs`
- `crates/core/src/index_types.rs`
- `crates/core/src/index.rs`
- `crates/core/src/sqlite_schema.rs`
- `crates/core/src/sqlite_index.rs`
- `crates/core/src/source_profile.rs`
- `crates/core/src/source_discovery.rs`
- `crates/core/src/store.rs`
- `crates/core/src/attachment_store.rs`
- `crates/cli/src/source.rs`
- `crates/cli/src/main.rs`

## Known gaps / recommended next steps

> **Status update (source metadata index buildout).** The following have since
> landed and are covered by unit tests + live verification:
> - Fingerprint (`mtime:size`) delta indexing in the refresh loop; unchanged
>   sessions only touch `last_seen_at` instead of re-parsing.
> - `source_profiles` and `source_scans` persisted on every refresh, with
>   `{scanned, reindexed, skipped}` stats recorded per scan.
> - `source_roots` recorded per profile; `workspace_roots` / `git_repositories`
>   linked to sessions via the M:N join tables during refresh.
> - `source_session_resources` upsert/list API.
> - `brick_session_source_sessions` bridge with `brick history link` /
>   `brick history linked` commands and core link/list APIs.
> - Native import dedup via the bridge link with `--force` override (gap 3).
> - `brick version --format json` for ORGII adapter compatibility gating.
> - `brick source configure` / `source scan --write-defaults` now also persist
>   `source_profiles` + `source_roots` into the metadata DB (best-effort,
>   non-fatal), via a shared `persist_profile_metadata` helper.
> - Cursor IDE reader now treats `composer.composerHeaders` as the authoritative
>   session list (rich metadata) and merges draft/subagent/parent-link flags from
>   `composerData:` rows; composerData-only remains a fallback for older DBs.
> - ORGII Stage-1 shadow-read adapter (`src/engines/SessionCore/sync/brick/`):
>   typed `BrickHistoryClient`, version gating, DTO validation, parity capture,
>   and a production `createTauriBrickRunner` backed by the Tauri shell plugin
>   (`sh -c 'exec "$0" "$@"'`, array args, no injection). 14 vitest cases.
>
> Remaining within these gaps: repo-context links and
> org/project/mission-filtered sync are unaddressed; ORGII cutover beyond
> Stage-1 shadow read (dual-read, Brick-primary) is future work.

### 1. Finish global metadata integration

First-stage `BRICK_HOME` resolution, metadata DB schema/API, and metadata-backed native history rows are implemented, but Brick still primarily uses repo-local source profile files and provenance queues.

Needed work:

- Persist source profile rows and source roots from `brick source scan/configure`, not only source sessions from history/import refreshes. (History/import refresh now persists profiles + roots; `source scan/configure` still does not.)
- Persist scan rows, workspace roots, repo contexts, and Brick-session links. (Scan rows, workspace roots, and Brick-session links now persisted; repo contexts still pending.)
- Decide when repo-local `.brick` is bootstrap/config only versus when it owns repo-local provenance events.
- Model many-to-many relationships between source sessions and workspace roots/repos during actual scans. (Done for history/import refresh; not yet for `source scan`.)
- Sync by `org_id` / project / mission / repo context filters rather than by physical repo-local storage.

### 2. Migrate ORGII external-history subsystem into Brick

Port the complete ORGII external-history subsystem into Brick rather than maintaining duplicate systems:

- Cursor `state.vscdb` read-only delta sync.
- Cursor session metadata table equivalent to ORGII's current `cursor_session_cache` data.
- Generic imported-history metadata table keyed by `(source, source_session_id)`.
- Signature-based change detection using source path, mtime, size, fingerprint, and parser version.
- Source-specific parsers for Cursor IDE, Codex App, Claude Code, OpenCode, and Windsurf.
- Lazy transcript/chunk loading from original source paths/DBs.
- `ActivityChunk` normalization and source-specific chunk windowing.
- Recent path aggregation and repo/workspace inference.
- Impact statistics: touched files, files changed, lines added/removed, model/tokens.
- Analysis/backfill jobs that currently re-open external history to enrich session metadata.
- Diagnostics/debug endpoints around source parse/index state.

After this, ORGII should use Brick as the external-history API and keep `.orgii` for ORGII-owned runtime state only.

### 3. Native import deduplication

Current `native ingest` can import the same native session repeatedly. Add a dedupe check using:

```text
(app_id, app_session_id)
```

Suggested behavior:

- If matching session exists, print it and do not append new events.
- Add `--force` to import again.
- Add tests and smoke coverage.

### 4. Better native metadata extraction — DONE

Implemented per source. Each `crates/core/src/sources/*.rs` reader now parses
real titles, timestamps, repo/cwd paths, and model/token metadata rather than
file stems:

- Claude Code JSONL (`claude_code.rs`): timestamps, cwd/repo, first message
  title, model/token metadata.
- Codex JSONL (`codex_app.rs`): `turn_context` cwd/model, rollout metadata.
- Generic JSONL (`jsonl.rs`): title inferred from first meaningful message.

### 5. Cursor native DB importer — DONE

`cursor_ide.rs` + `cursor_family/` read Cursor `state.vscdb` read-only:
`composer.composerHeaders` are the authoritative session list with rich
metadata, draft/subagent/parent-link flags merged from `composerData:` rows.

### 6. OpenCode native DB importer — DONE

`opencode.rs` reads `opencode.db` for session metadata and transcript pointers.

### 7. Interactive native pick — DONE

```bash
brick --source claude_code import native pick --mission <mission_id>
```

`import native pick` shows a `dialoguer::MultiSelect` of native sessions and
ingests the selected ones (reusing the shared `ingest_native_session` helper,
which also powers `ingest` and records the brick-session bridge link + dedup).
In non-interactive contexts it prints the session count and ingest guidance
instead of blocking.

### 8. Server auth and repo/org permissions — DONE (stages A–G; self-hosted multi-tenant)

**Stage A (scoped token table) landed.** The server auth gate is now a token
table persisted as `tokens.json` in the data dir. Each token has a label, a
SHA-256 hash of the secret (plaintext shown only once at issuance), one or more
scopes (`*`/`all`, `org:<id>`, or `repo:<id>`), and an access level (read or
write).

- Middleware: derives the resource target from the route (`/v1/repos/:repo_id/...`
  → repo target, everything else → global) and the required access from the HTTP
  method (GET/HEAD/OPTIONS → read, else write). Unknown token → 401; valid token
  without scope/access → 403. `/health` is always open.
- CLI: `brick-server create-token --label <l> --scope repo:<id> [--write]`,
  `list-tokens` (labels + scope/access summary, never plaintext), and
  `revoke-token --label <l>`.
- Backward compatible: `--auth-token` / `BRICK_SERVER_AUTH_TOKEN` still works as
  a convenience all-access write token merged into the table; with no tokens and
  no flag the server stays open.
- Verified: 21 server unit tests plus a live scope×access matrix (reader limited
  to repo-a 200, cross-repo/global/write all 403, admin global+write pass, bogus
  token 401).

**Stage B (token expiry) landed.** `TokenRecord` carries an optional
`expires_at`; `create-token --expires-in-days <n>` sets it, `list-tokens` shows
it, and the auth gate returns 401 (`AuthDenial::Expired`) once a token is past
its expiry. Tokens without an expiry never expire; the field is `#[serde(default)]`
so older token tables still load.

**Stage C (write audit log) landed.** Every authorized *write* (non-GET/HEAD/OPTIONS)
appends a record to `audit.jsonl` in the data dir — `{at, token_label, method,
path}`. Reads are not audited. View with `brick-server audit [--limit N]`.
Recording is best-effort (an I/O failure drops the entry, never the request).

**Stage D (org-scope resolution) landed.** An `org:<id>` scope now authorizes a
repo route when the server can resolve the repo's owning org. The gate resolves
repo→org by scanning stored events for the first event carrying both the
`repo_id` and an `org_id` (`ServerStore::resolve_repo_org`), and only pays that
cost when the token table actually contains an org scope
(`TokenStore::has_org_scope`). A repo whose org cannot be resolved is denied for
org-scoped tokens (deny-by-default). `ResourceTarget::Repo` now carries the
resolved `org_id`.

**Stage E (token rotation) landed.** `brick-server rotate-token --label <l>`
issues a fresh secret for an existing token in place, keeping its scopes and
access; the old secret stops working immediately. Expiry is preserved by default,
`--expires-in-days <n>` resets it, and `--expires-in-days 0` clears it. Rotating
an unknown label errors rather than creating a token.

**Stage F (actor binding) landed.** A token may be bound to an actor identity
via `create-token --actor-id <id>`. When bound, the push routes reject any event
whose `actor.actor_id` differs from the token's bound actor (`403`, whole-batch,
no partial accept), so a token can only speak for its own actor. Unbound tokens
(legacy `--auth-token`, admin tokens) are not actor-checked — binding is opt-in
and backward compatible (`TokenRecord.actor_id` is `#[serde(default)]`). The
resolved identity flows from the auth middleware to handlers via request
extensions (`AuthedIdentity`), and the audit log now records the verified actor
(`AuditEntry.actor_id`), surfaced by `list-tokens` and `audit`. For bound tokens
the audit actor is guaranteed equal to the event actor, making it the
trustworthy "who did this write" field.

**Stage G (persisted repo→org projection) landed.** Org-scope resolution no
longer rescans the event log per request. `ServerStore` maintains a repo→org map
(`repo_org.json`) incrementally: each accepted push updates it (first org per
repo wins) and commits it atomically (temp file + rename). It is cached in memory
behind an `Arc<RwLock<_>>` shared across store clones (handlers and the auth
gate), so steady-state resolution is an O(1) lookup. On a cold start the cache
loads from the file, or rebuilds once from the log when the file is absent
(legacy data dirs) and writes it back. Verified live: after a restart with the
event log removed, org-scoped routes still resolve purely from the projection.

§8 is now functionally complete for self-hosted multi-tenant use: token scopes
(repo/org/all), read vs write, expiry, rotation, write-audit with verified actor,
actor binding, and O(1) org resolution. Further hardening (external identity /
OIDC, richer org hierarchy, projection compaction) is post-MVP and not blocking.

### Agent awareness (`brick agent install`) — DONE

`brick agent install` makes coding agents use Brick on their own by injecting a
managed instruction block into their native memory files: `CLAUDE.md` (Claude
Code), `AGENTS.md` (Codex/Cursor/Copilot/OpenCode/…), `GEMINI.md` (Gemini). The
block (in `crates/cli/src/agent.rs`) points agents at `brick history` — primarily
`brick history file-session-blame --path <file> --format json` — so they recall
what prior sessions did to a file before editing it. This is the Multica-style
convention-file mechanism: no MCP, daemon, or agent forks.

Contract: the block is delimited by `<!-- brick:managed:start v=N -->` /
`<!-- brick:managed:end -->` sentinels with a version `N`. Edits are confined to
that region and written atomically (temp + rename), so a user's existing memory
file is never clobbered; `install` is idempotent and replaces a stale-version
block in place. Subcommands: `install` (`--target claude|codex|gemini|all`,
`--global`, `--dir`, `--force`, `--print`), `uninstall` (removes only the block),
`status` (present/stale/absent). Default scope is the working directory; `--global`
writes per-user locations (`~/.claude`, `~/.codex`, `~/.gemini`) best-effort and
skips-with-reason when a path can't be resolved. `brick init` offers to run it.

Out of scope (future): a higher-level `brick memory` system beyond metadata-only
recall/query, and per-turn dynamic prompt injection via tool hooks/daemon. The
static convention-file layer ships first because it is the highest-leverage step
with no runtime dependencies.

### Touched-files backfill (`file-session-blame` now returns real data) — DONE

`brick history file-session-blame` is what the agent block points at, but on
real local history it returned `status: empty` because parsers rarely populated
`source_sessions.touched_files`. Fixed end to end:

- **Codex apply_patch** (`sources/codex_app.rs`): `parse_patch_impact` now
  understands Codex's own envelope (`*** Begin Patch` / `*** Add File:` /
  `*** Update File:` / `*** Delete File:` / `*** Move to:`) in addition to
  git-diff, so structured edits yield touched_files.
- **Shell-driven edits** (`sources/shell_edits.rs`, new): a conservative,
  high-confidence extractor maps `Bash`/`exec_command` strings to written paths
  (redirects `>`/`>>`, `tee`, `sed -i`, `touch`, `cp`/`mv`), used by both Codex
  and Claude. apply_patch heredocs are parsed as patches, not shell tokens;
  read-only commands and operator/fd noise (`2>&1`, numbers, `{}`/`=`) are
  rejected.
- **Claude Edit/Write/Bash** (`sources/claude_code.rs`): `extract()` now folds
  `Edit`/`MultiEdit`/`Write` `file_path` and `Bash` write targets into the
  session-level touched_files (previously only Cursor did this).
- **Blame query relaxed** (`metadata_db.rs`): the hard `repo_path =` filter is
  gone (repo is now a ranking *preference*, same-repo first), and `path_matches`
  handles absolute-vs-relative form (exact / repo-resolved / component-boundary
  suffix), so blame works from any CWD. Unit-tested.
- **Auto-reindex on parser upgrade** (`cli/src/history.rs`): the source
  fingerprint now includes `parser_version`, so bumping a parser (these went to
  `…-jsonl-v3`) invalidates indexed rows and forces a re-parse — no manual
  reindex command needed.

Verified live against the user's real `~/.codex`: 52 Codex sessions now carry
touched_files (0 junk after hardening), and
`file-session-blame --path …/sample_sales.csv` returns the editing session by
absolute *and* relative path. Note: a tool with no file-edit signal in its
history (the user's recent Claude sessions are chat/research) correctly yields no
hits — a data fact, not a parser failure (Claude extraction is unit-tested).

### All-source blame coverage (every provider verified) — DONE

Extended touched-files extraction across every provider and verified
`file-session-blame` live against the user's real machine. Per-source result
(`status: ok` with real rows unless noted):

- **codex_app** ✅ — Codex apply_patch envelope + exec_command shell edits;
  52 sessions carry touched_files; blamed `sample_sales.csv`.
- **cursor_ide** ✅ — already populated via composer `touchedFiles`; blamed
  `src-tauri/Cargo.toml`.
- **claude_code** ✅ — Edit/Write/Bash extraction; verified by *triggering the
  real `claude` CLI* to create `blame_target.py`, then blaming it.
- **orgii** ✅ — **new provider** `sources/orgii.rs`: reads
  `~/.orgii/sessions.db` (`agent_sessions` + `events`), mapping
  `edit_file`/`write_file`/`apply_patch` args and `run_shell` commands to
  touched_files; blamed a real ORGII `.tsx` across 5 sessions.
- **gemini** ✅ — **new provider** `sources/gemini.rs` + new
  `DiscoveredSourceKind::Gemini` (discovery root `~/.gemini/tmp`): parses
  `<projectHash>/chats/session-*.json`, mapping `write_file`/`replace` and
  `run_shell_command` tool calls to touched_files; 7 sessions populated, blamed
  `handler.py`.
- **windsurf** ✅ (synthetic) — parser already extracts composer `touchedFiles`;
  the user's machine has *no* Windsurf data (empty `~/.codeium/windsurf` store,
  no `.vscdb`), so the full path was proven with a synthetic `state.vscdb` in the
  real `cursorDiskKV` format → blame returned the session. Real-data parity is
  identical to cursor_ide (shared `cursor_family`).

Shared infra reused: the `shell_edits` helper (redirect/tee/sed/apply_patch
heredoc recognition, with bare-punctuation and fd-token rejection) is now used by
codex, claude, orgii, and gemini. The blame query's repo-path relaxation +
`path_matches` (added earlier) means every source works from any CWD by absolute
or relative path. Parser versions fold into the source fingerprint, so each parser
upgrade auto-reindexes without a manual command.

### `brick metadata recall` — one-call metadata recall — DONE

`crates/cli/src/metadata.rs` adds `brick metadata recall --path <file>`, the single
command the agent-awareness block now points at (TEMPLATE_VERSION bumped to 2).
It reuses `build_file_session_blame_response` (so it aggregates across every
source) and enriches each blame row by joining `(source_id, external_session_id)`
back to the metadata DB for the session title — the *intent*/"why". Output
(`metadata-recall-v1`):

- `summary` — one natural-language line ("N prior sessions touched <file> (via
  <tools>). Most recent: \"<intent>\".") for direct agent consumption.
- `sessions[]` — per prior session: source, intent, change size
  (files/lines), confidence, and a ready-to-run `recall_chunks_hint`
  (`brick history chunks …`) for the full transcript on demand.
- `status` is `ok` / `empty` / `error`, mirroring blame.

Verified live: recall returns real intent across codex (`sample_sales.csv`),
claude (`blame_target.py`), orgii (`index.tsx`, 5 sessions), and gemini
(`handler.py`); a never-touched path returns `status: empty` with a friendly
summary. Wiring note: `Command::Metadata` is exempted from global
`selected_profile` resolution in `main.rs` (like `History`) so `--source all`
works.

### `brick metadata query` — free-text session metadata search — DONE

`brick metadata query --query "<keywords>"` finds past sessions by topic when you
don't have a specific file in hand. `metadata_db.query_source_sessions_text`
(new) does case-insensitive substring matching over the *already-indexed*
session metadata — title/name (intent), touched files, repo path, branch, and
model — so it is instant and never loads a transcript. Output
(`metadata-query-v1`): a one-line `summary` plus `matches[]` (source, intent,
repo/branch, change size, touched files, and a `recall_chunks_hint`). `status`
is `ok` / `empty` / `error`.

This was deliberately scoped to *metadata* search, not full-text: the chat
content lives in source files (loaded on demand as chunks) and there is no FTS
index. Metadata search reuses the existing index, keeps the JSON small, and lets
an agent triage hits then drill into one session's chunks. A future FTS5 layer
over chunk text is the natural next step if content search is needed.

Verified live: `--query csv` → codex session (intent = the user's prompt),
`--query diff` → 5 orgii sessions ("Investigate Mismatched Diff Output" …),
`--query nonexistent` → `status: empty`. Agent block (TEMPLATE_VERSION 3) now
documents both `recall` and `query`.

## Design cautions

- Treat ORGII external-history code as a whole subsystem: scan, parse, metadata indexing, source-specific loading/windowing, chunk load, recent paths, impact stats, backfill, and diagnostics move together into Brick over time.
- Be explicit that today these source-specific loading mechanisms still live in `.orgii`/ORGII code; the migration target is to make Brick own them and expose a stable history provider API.
- Leave ORGII-only app/runtime behavior in ORGII: UI state, Tauri registration, ORGII-owned live sessions, repo import UI, Cursor live send/watch automation, and rendering/event-store plumbing until a later UI refactor.
- Do not duplicate local transcripts by default.
- Do not silently swallow source parsing errors if the user explicitly selected a source/session.
- Keep repo-local `.brick/` ignored by Git when repo-local bootstrap/config exists.
- Prefer a unified global/configured Brick root for the source metadata index.
- Keep source-profile config in TOML unless/until the unified DB fully replaces file profiles.
- Keep local views derived and rebuildable.
- Avoid adding aliases for removed old commands; this repo has not shipped, so prefer clean command shape.
- Prefer typed enums/constants for domain values.

## Useful commands for the next agent

```bash
cargo fmt
cargo check
cargo test
cargo doc --no-deps
scripts/smoke_mvp.sh
```

List native sessions from a configured profile:

```bash
cargo run -p brick -- --source claude_code import native list --limit 20
```

Ingest one native session:

```bash
cargo run -p brick -- --source claude_code import native ingest --external-session-id <native-id> --mission <mission_id>
```

Check source discovery:

```bash
cargo run -p brick -- source scan
```
