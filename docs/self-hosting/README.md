# Self-hosting

`brick-server` runs a small provenance remote for local and lab self-hosting. It can run open for localhost experiments, use the legacy self-hosted token table, or verify Supabase Auth JWTs for account-scoped sync.

## Run the server

Open local server:

```bash
cargo run -p brick-server -- serve --bind 127.0.0.1:7821 --data-dir .brick-server
curl http://127.0.0.1:7821/health
```

Supabase-authenticated server:

```bash
export BRICK_SUPABASE_URL="https://<project>.supabase.co"
export BRICK_SUPABASE_JWT_SECRET="<project-jwt-secret>"
cargo run -p brick-server -- serve --bind 127.0.0.1:7821 --data-dir .brick-server
```

The server verifies Supabase access-token JWTs with the project JWT secret. Repo-scoped routes are account-owned: the first Supabase user to push to `/v1/repos/{repo_id}/events` claims that repo on the server, and later Supabase reads/writes for that repo must use the same user. The append-only event store remains `events.jsonl` under `--data-dir`; Supabase is used for Auth in this phase, not as the event database.

## Push and pull

The default sync payload is metadata-first: mission events, session metadata events, artifact metadata, diff summaries, references, hashes, and storage URIs. Large transcript files, recordings, screenshots, and uploaded attachments are represented by content-addressed references in events. Full blob transfer is explicit future work; do not assume `push` uploads every referenced byte.

Use a repo ID when synchronizing a specific repository boundary. The sync CLI is feature-gated for private builds:

```bash
export BRICK_SUPABASE_URL="https://<project>.supabase.co"
export BRICK_SUPABASE_ANON_KEY="<project-anon-key>"
cargo run -p brick --features sync -- sync login --email you@example.com
cargo run -p brick --features sync -- sync login --email you@example.com --code <otp-code>
cargo run -p brick --features sync -- sync push --remote http://127.0.0.1:7821 --repo-id repo-a
cargo run -p brick --features sync -- sync pull --remote http://127.0.0.1:7821 --repo-id repo-a
cargo run -p brick --features sync -- sync run --remote http://127.0.0.1:7821 --repo-id repo-a --dry-run
```

`push` posts local queued events and prints accepted, duplicate, and queued counts. It does not drain or delete the local queue. `pull` pages remote events, deduplicates by event ID against local queued and inbound events, and writes new remote events under the local inbound log. `sync` currently runs pull followed by non-draining push.

With a `--features sync` build and a logged-in Supabase session, Brick also performs best-effort automatic sync on the normal agent path: `explain` tries to pull before reading, and successful `link` / planning writes try to push after appending a local event. Automatic sync uses repo-scoped routes with `repo_id_for_root(repo_root)` and defaults to `http://127.0.0.1:7821`. Set `BRICK_AUTO_SYNC_REMOTE` to another server, or `BRICK_AUTO_SYNC_DISABLE=1` to turn the automatic pull/push attempts off.

Omit `--repo-id` only for the global compatibility endpoint. Supabase account ownership is only enforced on repo-scoped routes, so production sync should prefer `--repo-id`.

```bash
cargo run -p brick --features sync -- sync push --remote http://127.0.0.1:7821
cargo run -p brick --features sync -- sync pull --remote http://127.0.0.1:7821
```

## Repo ID behavior

Repo IDs scope event lists and server-derived query views. On `POST /v1/repos/{repo_id}/events`, the server fills missing event `repo_id` values from the route and rejects mismatches. With Supabase JWT auth enabled, the first user to push a repo claims it in `repo_owner.json`; later Supabase reads/writes for that repo must present a token for the same user. Legacy local token-table auth keeps its explicit token scopes.

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

## Source notes

Brick's current user-facing path is `explain` / `link`. Native AI-tool history is
refreshed automatically as part of those calls when available; the old public
`source` and `import` commands are no longer part of the supported CLI surface.

## Smoke verification

Run the MVP smoke harness from the repository root:

```bash
scripts/smoke_mvp.sh
```

It uses temporary Git repositories and stores, starts `brick-server` on localhost, exercises repo-scoped push/pull, queries server status and sessions, and cleans up the background server on exit. Set `BRICK_SMOKE_PORT` to override the default local port.
