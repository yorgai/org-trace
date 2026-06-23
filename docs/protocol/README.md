# Protocol

Brick uses an append-only JSONL event protocol locally and the same event envelope over the self-hosted HTTP sync API. Schema version `1` is the current MVP version.

## Event envelope

Each `TraceEvent` has these stable fields:

- `event_id`: UUID used for idempotency and deduplication.
- `event_type`: stable dotted event name.
- `schema_version` and `payload_schema_version`: currently `1`.
- `occurred_at` and `recorded_at`: UTC timestamps.
- `actor`: actor type, actor ID, and optional display name.
- `repo_id`: optional server-side repository boundary retained by the current self-hosted MVP routes.
- `org_id`, `project_id`, `mission_id`, `session_id`, `artifact_id`, `repo_context_id`: optional graph anchors.
- `confidence`: `explicit`, `observed`, `imported`, `inferred`, or `unknown`.
- `payload`: typed JSON payload for the event family.

Local append-only logs are the source of truth. Derived indexes may denormalize fields, but protocol consumers should treat events as immutable.

## Event families

### Mission events

- `mission.created`: creates an accountability container with title, optional description, and optional repo context.
- `mission.updated`: reserved typed partial update payload for mission title and description.

### Session events

- `session.started`: records a canonical Brick session, optional mission link, session name, and source fields such as `app_id`, app-native session ID/name, and runtime ID. Sessions can represent agent execution, human work, CI execution, or imported external activity.
- `session.linked_to_mission`: links an existing session to a mission with a relationship label.
- `session.log_uploaded`: records metadata for content-addressed session log or transcript bytes. The payload stores log ref ID, original path, format, source, SHA-256, byte size, storage URI, local path, and optional repo context; it does not inline log content.

Session metadata is the default sync tier. Full transcripts, formatted transcript JSON, recordings, and logs are optional evidence tier content addressed by hashes and storage URIs.

### Artifact events

- `artifact.created`: records a reviewable output with kind, title, optional body, and optional repo context.
- `artifact.updated`: records append-only partial metadata updates for title, body, and kind.
- `artifact.linked_to_mission`: links an existing artifact to a mission with a relationship label.
- `artifact.file_ref_recorded`: links an artifact to a repository file path.
- `artifact.attachment_uploaded`: records metadata for content-addressed artifact attachment bytes without inlining content.
- `artifact.reviewed` and `artifact.accepted`: reserved event names for later review workflows.

Artifacts are Brick's work products and proof of work. Agent sessions may attach generated files, diffs, plans, or CI results. Human sessions may attach meeting notes, screen recordings, QA videos, screenshots, approval notes, or manual review documents. Large proof files should be uploaded as attachments so events stay small and syncable.

Artifact kinds are `decision`, `file_ref`, `patch`, `review`, `test_result`, `acceptance`, and `note`.

### Repository and diff events

- `repo_context.captured`: records repo root, working directory, remote URL, branch, upstream branch, HEAD, merge base, dirty state, and context mode.
- `diff.captured`: records patch provenance metadata for `working`, `staged`, or `range` targets. The payload stores optional base/head commits, optional Git patch ID, stable summary hash, and file-level change summaries with additions/deletions when available.

### External reference events

- `external_ref.linked`: links mission, session, or artifact graph entities to external systems such as CI jobs, pull requests, issues, or logs.

### Causal events

- `causal.linked`: a directed causal edge that turns the time-ordered event stream into a **causal graph** — the core of `explain` (WHY), as opposed to a mere timeline (`git log`). Payload fields:
  - `effect_event` (UUID, optional): the event that happened (usually a `diff.captured`). The highest-precision effect anchor.
  - `effect_path` (string, optional): a repo-relative file path used as the effect anchor when no `effect_event` exists — i.e. the agent edited a file with its own tools and recorded a rationale against it without capturing a Brick diff. The next-lower anchor precision after `effect_event`.
  - `repo_context_id` (optional): the lowest-precision anchor — a repo-level standalone rationale (a bare `note` with no file or event). The effect-anchor ladder is `effect_event` → `effect_path` → `repo_context_id`.
  - `cause_events` (UUID list, may be empty): zero or more upstream events that caused it. Any `event_id` — a diff, an artifact, another session's event, a mission. Mission is just one possible cause, not a requirement.
  - `relation` (enum): `triggered_by` (set in motion by the cause, e.g. an A2A `previous_actions` step), `derived_from` (built from the cause, e.g. a test covering a fix), `supersedes` (corrects/replaces the cause), `responds_to` (a response to a request), or `rationale` (a standalone reason with no upstream event — the WHY that cannot be reverse-engineered from the code).
  - `note` (optional string): the one-line WHY.
  - **Invariant:** the edge must carry information — at least one of `effect_event`, `effect_path`, a non-empty `cause_events`, or a non-empty `note`. An edge with none of these is rejected as noise.

  The event's `confidence` distinguishes how the edge was produced: `explicit` (an agent or human asserted it via `link`), `observed` (a hook captured it from session context), or `inferred` (a fallback heuristic, used only when no explicit edge exists). Edges are projected into the rebuildable `causes` / `effects` adjacency tables at index time; the *chains* are traversed at query time by `explain` (a chain is relative to an anchor + depth, so materializing all chains would combinatorially explode). The adjacency tables and the SQLite `causal_edges` table are derived data — deleting them and rebuilding from JSONL reproduces them exactly.

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
cargo run -p brick -- maintenance index rebuild
cargo run -p brick -- maintenance db rebuild
cargo run -p brick -- maintenance db sessions --limit 20
cargo run -p brick -- maintenance db artifacts --limit 20
```

## Importer semantics

Cursor, Codex, Claude Code, and CI importers normalize explicit files into regular events with `confidence=imported`. Importers do not inspect private application databases. JSONL inputs may contain full Brick `TraceEvent` lines or simple records; text and Markdown transcripts become session log metadata events; CI JSON becomes `artifact.created` test-result events and external references when URLs are present.
