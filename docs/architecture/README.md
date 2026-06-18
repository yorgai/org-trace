# Architecture

Brick is a local-first provenance system with a small self-hosted sync surface. The MVP records accountable work as immutable events, derives query views from those events, and keeps local operation useful without a server.

## Current phase status

Phase 14 completes the MVP documentation and end-to-end smoke harness around the implemented surface:

- Local JSONL trace recording for missions, sessions, artifacts, repo contexts, diffs, logs, and imports.
- Source profiles for stable app/actor defaults and optional store-root selection.
- Content-addressed blob storage for artifact attachments and session logs.
- Rebuildable JSON and SQLite local indexes for read-only inspection.
- Explicit-file importers for Cursor, Codex, Claude Code, and CI summaries.
- Unauthenticated self-hosted append-only event sync with repo-scoped routes.
- Server rebuild-on-read status and session query endpoints.

Still out of scope: authentication, per-repo authorization, queue draining, conflict resolution, background server indexes, full review workflow events, and private application database scraping.

## Product model

Brick's human-facing model is Mission-centered:

```text
Org or sync scope
  └── Project grouping
        └── Mission
              ├── Sessions
              └── Artifacts
```

A Mission is the object people manage. It replaces a task or work item and owns status, planning metadata, linked sessions, artifacts, and the proof-of-work timeline.

A Session is execution evidence. Sessions can come from agents, humans, CI systems, or importers. A human session is valid when a person manually performs work and records evidence such as a review note, meeting transcript, video recording, screenshot, QA pass, or operational log.

Artifacts replace work products. They are reviewable evidence attached to Missions and Sessions: diffs, decisions, reviews, CI results, documents, screenshots, recordings, external references, and uploaded files.

## Two-tier session availability

Brick intentionally keeps Session metadata separate from full Session content:

1. **Metadata tier** — synced and indexed by default. It includes session ID, actor, source app, timestamps, linked missions, linked artifacts, repo contexts, transcript availability, and last update time. Mission pages can show useful session cards from this tier alone.
2. **Full evidence tier** — optional content-addressed files, such as transcript JSON/JSONL, video recordings, screenshots, or raw logs. Events store hashes, sizes, and storage URIs; large bytes live in blob storage and can be fetched or uploaded explicitly.

There is no separate third tier in the MVP. Structured replay is a rendering capability over full evidence when the uploaded transcript format supports it.

## Components

### `brick` CLI

The CLI is the main capture and inspection surface. It discovers the Git repository root, resolves the effective store root, loads source profile defaults, captures repo context for write commands, and appends typed protocol events to the local store.

The next Brick-native CLI should be Mission-first and should not keep legacy command aliases. The current MVP command shape proved the storage and sync substrate; the product CLI should expose only the new model:

- `org` for the sync/share boundary.
- `project` for project lists and grouping.
- `mission` for the human-managed work object, including status and assignment.
- `session` for metadata-tier execution evidence.
- `artifact` for work products and reviewable outputs.
- `evidence` for transcripts, recordings, screenshots, raw logs, attachments, and diff capture.
- `import` for explicit external trace files.
- `sync` for org-scoped push, pull, and status.
- `maintenance` for index rebuilds, SQLite rebuilds, and diagnostics.

Old recorder-shaped commands such as top-level `diff capture`, `artifact upload`, `session upload-log`, `db`, `index`, and standalone `push`/`pull` should be replaced rather than preserved as public aliases. Documentation should show only the Brick-native command set.

### `brick-core`

`brick-core` owns local storage, source profile files, repo context capture, diff summarization, blob stores, JSON index rebuilds, SQLite rebuilds, and sync-oriented deduplication. The append-only event stream remains authoritative; `index.json` and `brick.sqlite` are disposable caches.

Storage root resolution order:

1. `--store-root`
2. `BRICK_STORE_ROOT`
3. selected source profile `store_root`
4. `.brick/provenance` under the Git repository root

### `brick-protocol`

`brick-protocol` defines the `TraceEvent` envelope, typed IDs, actor/source types, event names, payload structs, and sync request/response bodies. Event constructors keep producers typed while the persisted payload remains JSON for append-only compatibility.

### `brick-importers`

Importers accept explicit files supplied by the operator. JSONL can contain full Brick events or simple records. Text and Markdown transcripts become session log metadata. CI JSON creates test-result artifacts and optional external references. Imported events use normal event logs and are marked with imported confidence.

### `brick-server`

The server is an append-only HTTP remote backed by `events.jsonl` under `--data-dir`. It exposes global compatibility routes and preferred repo-scoped routes. Push is idempotent by event ID. Pull uses opaque append-log cursors. Query endpoints rebuild derived views from the server log on demand.

## Data flow

1. A CLI write command resolves identity from flags, current context, and source profile defaults.
2. The CLI captures Git repo context when applicable.
3. The command appends one or more typed `TraceEvent` JSONL records to the effective local store.
4. Attachments and logs are copied to content-addressed blob paths while events store metadata and URIs.
5. Local indexes are rebuilt from queued local events plus inbound remote events.
6. `push` sends queued local events to the server without draining them.
7. `pull` pages server events and stores previously unseen records in inbound logs.
8. Server query endpoints rebuild views from the server append log when requested.

## Verification

The repository includes an MVP smoke harness at `scripts/smoke_mvp.sh`. It runs in temporary directories, avoids the user's working tree, starts and cleans up a background local server, pushes by repo ID, pulls into a second store, and checks local and server query paths.

Recommended verification before release:

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo doc --workspace --no-deps
scripts/smoke_mvp.sh
```
