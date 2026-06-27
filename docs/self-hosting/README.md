# Supabase-native sharing

Brick sharing can run directly on Supabase: Supabase Auth handles login, and Postgres/RLS stores events plus manages org membership. You do not need to run `brick-server` for this path.

## One-time Supabase setup

1. Create a Supabase project.
2. In the Supabase SQL editor, run `docs/self-hosting/supabase.sql`.
3. Set these client-side environment variables:

```bash
export BRICK_SUPABASE_URL="https://<project>.supabase.co"
export BRICK_SUPABASE_ANON_KEY="<project-anon-or-publishable-key>"
```

Do not put the service-role key on client machines.

## Login, create an org, and invite a coworker

Owner machine:

```bash
brick sync login --email you@example.com
brick sync login --email you@example.com --code <otp-code>
brick sync create-org --org-id org_shared
brick sync invite --org-id org_shared --email coworker@example.com
```

Coworker machine:

```bash
brick sync login --email coworker@example.com
brick sync login --email coworker@example.com --code <otp-code>
brick sync accept-invites
```

## Push and pull through Supabase

Use `--remote supabase` to bypass `brick-server` entirely:

```bash
brick sync push --remote supabase --repo-id repo-a --org-id org_shared
brick sync pull --remote supabase --repo-id repo-a
brick sync run --remote supabase --repo-id repo-a --org-id org_shared
```

For automatic sync:

```bash
export BRICK_AUTO_SYNC_REMOTE=supabase
export BRICK_SYNC_ORG_ID=org_shared
```

Events are stored in the `public.brick_events` table. The RLS policies in `supabase.sql` allow reads/writes only for users who are members of the event's org.

## Legacy brick-server mode

`brick-server` still exists for local/lab deployments. It can run open for localhost experiments, use the legacy self-hosted token table, or verify Supabase Auth JWTs while storing events on its own disk. New sharing setups should prefer the Supabase-native path above.

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

The server verifies Supabase access-token JWTs with the project JWT secret. Repo-scoped routes are org-owned via the server's own `org_members.json` and `repo_org.json` files; this legacy mode does not use JWT `org_ids` claims and does not store events in Supabase.

## Legacy brick-server push and pull

The default sync payload is one fused normalized stream: source session metadata plus normalized transcript chunks, mission events, artifacts, and diffs. Provider-local sqlite/jsonl files are ingestion sources only; `push` uploads rows from the unified local event/chunk DB, and `pull` writes new remote events back into that same DB. Pulled `source.session_observed` rows are also projected into `metadata.sqlite` for source-session listing/search.

Use a repo ID when synchronizing a specific repository boundary:

```bash
cargo run -p brick --features sync -- sync push --remote http://127.0.0.1:7821 --repo-id repo-a --org-id org_shared
cargo run -p brick --features sync -- sync pull --remote http://127.0.0.1:7821 --repo-id repo-a
cargo run -p brick --features sync -- sync run --remote http://127.0.0.1:7821 --repo-id repo-a --org-id org_shared --dry-run
```

`push` posts locally stored normalized events and prints accepted, duplicate, and event counts. It does not delete local events. `pull` pages remote events, deduplicates by event ID against the local event/chunk DB, writes new remote events into that DB, and projects pulled `source.session_observed` metadata + normalized chunks into `metadata.sqlite` for query helpers. `sync` currently runs pull followed by non-draining push.

With a `--features sync` build and a logged-in Supabase session, Brick also performs best-effort automatic sync on the normal agent path: `explain` tries to pull before reading, and successful planning writes try to push after appending a local event. Automatic sync uses repo-scoped routes with `repo_id_for_root(repo_root)`. Set `BRICK_AUTO_SYNC_REMOTE=supabase` for Supabase-native sync, set it to an HTTP server URL for legacy brick-server sync, or set `BRICK_AUTO_SYNC_DISABLE=1` to turn the automatic pull/push attempts off.

## Repo ID behavior

Repo IDs scope event lists and server-derived query views. Supabase-native sync stores events in `public.brick_events` with `repo_id` and `org_id` columns; legacy brick-server sync uses `/v1/repos/{repo_id}/events` and binds repos to orgs in `repo_org.json`.

Preferred legacy repo-scoped HTTP routes:

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

Brick's current user-facing path is `explain`. Native AI-tool history is
refreshed automatically as part of those calls when available; the old public
`source` and `import` commands are no longer part of the supported CLI surface.

## Smoke verification

Run the MVP smoke harness from the repository root:

```bash
scripts/smoke_mvp.sh
```

It uses temporary Git repositories and stores, starts `brick-server` on localhost, exercises repo-scoped push/pull, queries server status and sessions, and cleans up the background server on exit. Set `BRICK_SMOKE_PORT` to override the default local port.
