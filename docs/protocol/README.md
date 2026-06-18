# Protocol

Brick uses an append-only JSONL event protocol locally and the same event envelope over the self-hosted HTTP sync API. Schema version `1` is the current MVP version.

## Event envelope

Each `TraceEvent` has these stable fields:

- `event_id`: UUID used for idempotency and deduplication.
- `event_type`: stable dotted event name.
- `schema_version` and `payload_schema_version`: currently `1`.
- `occurred_at` and `recorded_at`: UTC timestamps.
- `actor`: actor type, actor ID, and optional display name.
- `repo_id`: optional server-side repository boundary.
- `mission_id`, `session_id`, `artifact_id`, `repo_context_id`: optional graph anchors.
- `confidence`: `explicit`, `observed`, `imported`, `inferred`, or `unknown`.
- `payload`: typed JSON payload for the event family.

Local append-only logs are the source of truth. Derived indexes may denormalize fields, but protocol consumers should treat events as immutable.

## Event families

### Mission events

- `mission.created`: creates an accountability container with title, optional description, and optional repo context.
- `mission.updated`: reserved typed partial update payload for mission title and description.

### Session events

- `session.started`: records a canonical Brick session, optional mission link, session name, and source fields such as `app_id`, app-native session ID/name, and runtime ID.
- `session.linked_to_mission`: links an existing session to a mission with a relationship label.
- `session.log_uploaded`: records metadata for content-addressed session log bytes. The payload stores log ref ID, original path, format, source, SHA-256, byte size, storage URI, local path, and optional repo context; it does not inline log content.

### Artifact events

- `artifact.created`: records a reviewable output with kind, title, optional body, and optional repo context.
- `artifact.updated`: records append-only partial metadata updates for title, body, and kind.
- `artifact.linked_to_mission`: links an existing artifact to a mission with a relationship label.
- `artifact.file_ref_recorded`: links an artifact to a repository file path.
- `artifact.attachment_uploaded`: records metadata for content-addressed artifact attachment bytes without inlining content.
- `artifact.reviewed` and `artifact.accepted`: reserved event names for later review workflows.

Artifact kinds are `decision`, `file_ref`, `patch`, `review`, `test_result`, `acceptance`, and `note`.

### Repository and diff events

- `repo_context.captured`: records repo root, working directory, remote URL, branch, upstream branch, HEAD, merge base, dirty state, and context mode.
- `diff.captured`: records patch provenance metadata for `working`, `staged`, or `range` targets. The payload stores optional base/head commits, optional Git patch ID, stable summary hash, and file-level change summaries with additions/deletions when available.

### External reference events

- `external_ref.linked`: links mission, session, or artifact graph entities to external systems such as CI jobs, pull requests, issues, or logs.

## Sync endpoints

The self-hosted MVP exposes append-only event transfer. Routes are unauthenticated; repo IDs are data boundaries, not authorization boundaries.

Global compatibility routes:

```http
GET  /v1/events?after=<cursor>&limit=<n>&repo_id=<repo-id>
POST /v1/events
```

Preferred repo-scoped routes:

```http
GET  /v1/repos/{repo_id}/events?after=<cursor>&limit=<n>
POST /v1/repos/{repo_id}/events
```

`POST` accepts a JSON body:

```json
{
  "events": []
}
```

The response reports idempotency by event ID:

```json
{
  "accepted_event_ids": [],
  "duplicate_event_ids": []
}
```

On repo-scoped `POST`, the server fills missing event `repo_id` values from the route and rejects events whose existing `repo_id` does not match the route.

`GET` returns:

```json
{
  "events": [],
  "cursor": "10",
  "next_cursor": "20"
}
```

Cursors are server append-log sequence values in the MVP, but clients must treat them as opaque and pass `next_cursor` back as the next `after` value. Missing cursors remain valid for compatibility responses.

## Query endpoints

Server query routes rebuild derived views from the append-only server log on demand:

```http
GET /health
GET /v1/index/status
GET /v1/repos/{repo_id}/index/status
GET /v1/sessions?limit=20&app_id=cursor&actor_id=agent-1
GET /v1/repos/{repo_id}/sessions?limit=20&app_id=cursor
```

Local query commands use equivalent rebuildable local projections:

```bash
cargo run -p brick -- index rebuild
cargo run -p brick -- db rebuild
cargo run -p brick -- db sessions --limit 20
cargo run -p brick -- db artifacts --limit 20
```

## Importer semantics

Cursor, Codex, Claude Code, and CI importers normalize explicit files into regular events with `confidence=imported`. Importers do not inspect private application databases. JSONL inputs may contain full Brick `TraceEvent` lines or simple records; text and Markdown transcripts become session log metadata events; CI JSON becomes `artifact.created` test-result events and external references when URLs are present.
