# Self-hosting

`brick-server` runs a small unauthenticated provenance remote for local and lab self-hosting. Do not expose it to untrusted networks until authentication and repo authorization are implemented.

## Run the server

```bash
cargo run -p brick-server -- serve --bind 127.0.0.1:7821 --data-dir .brick-server
curl http://127.0.0.1:7821/health
```

The server stores an append-only `events.jsonl` log under `--data-dir`. That log is the server source of truth. Query and index routes rebuild derived views from the log for the MVP.

## Push and pull

Use a repo ID when synchronizing a specific repository boundary:

```bash
cargo run -p brick -- push --remote http://127.0.0.1:7821 --repo-id repo-a
cargo run -p brick -- pull --remote http://127.0.0.1:7821 --repo-id repo-a
cargo run -p brick -- sync --remote http://127.0.0.1:7821 --repo-id repo-a --dry-run
```

`push` posts local queued events and prints accepted, duplicate, and queued counts. It does not drain or delete the local queue. `pull` pages remote events, deduplicates by event ID against local queued and inbound events, and writes new remote events under the local inbound log. `sync` currently runs pull followed by non-draining push.

Omit `--repo-id` only for the global compatibility endpoint:

```bash
cargo run -p brick -- push --remote http://127.0.0.1:7821
cargo run -p brick -- pull --remote http://127.0.0.1:7821
```

## Repo ID behavior

Repo IDs scope event lists and server-derived query views. On `POST /v1/repos/{repo_id}/events`, the server fills missing event `repo_id` values from the route and rejects mismatches. Repo IDs are not access-control boundaries yet.

Preferred repo-scoped HTTP routes:

```http
GET  /v1/repos/{repo_id}/events?after=<cursor>&limit=<n>
POST /v1/repos/{repo_id}/events
GET  /v1/repos/{repo_id}/index/status
GET  /v1/repos/{repo_id}/sessions?limit=20&app_id=cursor
```

Global compatibility routes:

```http
GET  /v1/events?after=<cursor>&limit=<n>&repo_id=<repo-id>
POST /v1/events
GET  /v1/index/status
GET  /v1/sessions?limit=20
```

## Server index/status

The MVP query surface rebuilds projections on demand:

```bash
curl http://127.0.0.1:7821/v1/repos/repo-a/index/status
curl 'http://127.0.0.1:7821/v1/repos/repo-a/sessions?limit=20&app_id=cursor'
cargo run -p brick-server -- rebuild-index --data-dir .brick-server --repo-id repo-a
```

`rebuild-index` prints status JSON and does not write a cache yet.

## Cursor notes

Cursor support is explicit-file based. Brick does not inspect private Cursor workspace databases. Configure a source profile for stable actor/app defaults, then pass exported JSONL, text, or Markdown files to the importer:

```bash
cargo run -p brick -- source configure --name cursor --app-id cursor --actor-id cursor-agent --actor-type agent --session-log-path ./exports/cursor-session.jsonl --notes "Cursor import defaults"
cargo run -p brick -- import cursor --path ./exports/cursor-session.jsonl --mission <mission-id> --session <session-id> --app-session-id <cursor-native-session> --app-session-name "Cursor task"
```

## Smoke verification

Run the MVP smoke harness from the repository root:

```bash
scripts/smoke_mvp.sh
```

It uses temporary Git repositories and stores, starts `brick-server` on localhost, exercises repo-scoped push/pull, queries server status and sessions, and cleans up the background server on exit. Set `BRICK_SMOKE_PORT` to override the default local port.
