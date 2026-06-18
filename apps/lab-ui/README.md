# Brick Lab UI

A small React/Vite dashboard for exercising Brick's local server while developing features. It now includes a local external-history workflow for source diagnostics, metadata refresh/backfill, paginated session browsing, and session export.

## Run locally

From the repository root, build the local `brick` binary used by the history bridge:

```bash
cargo build -p brick
```

Start the Brick server. The local-history HTTP bridge is opt-in because it reads local source profile paths and exports local session metadata:

```bash
cargo run -p brick-server -- serve \
  --bind 127.0.0.1:5353 \
  --data-dir .brick-server \
  --enable-local-history \
  --brick-bin /Users/laptop-h/.cargo/shared-target/debug/brick \
  --repo-root "$PWD"
```

In another terminal, install and run the UI:

```bash
cd apps/lab-ui
npm install
npm run dev
```

Open <http://127.0.0.1:5454>. The Vite dev server proxies `/api/*` to `http://127.0.0.1:5353`, so the Server URL field can stay blank.

## Local history workflow

1. Click **Detect local sources** to scan well-known local stores such as Cursor, Claude Code, Codex, OpenCode, and ORGII.
2. Select the sources you want and click **Index selected**. This writes source profiles and refreshes their metadata index.
3. Reload sources in the dashboard and select a source profile.
4. Run diagnostics to inspect configured paths, provider status, and indexed counts.
5. Click **Refresh metadata** to invoke the same local history refresh path used by `brick history sessions` and `brick history plans`.
6. Page through sessions with limit/offset controls.
7. Select a session and export `audit-v1` or `source-metadata-v1` as JSON or CSV. The result is displayed and exposed as a browser download.

## Useful probes

Existing sync/index probes remain available in the dashboard:

- `GET /health`
- `GET /v1/repos/:repo_id/index/status`
- `GET /v1/repos/:repo_id/sessions?limit=20`
- `GET /v1/repos/:repo_id/events?limit=20`

Local history endpoints exposed by the opt-in bridge:

- `GET /v1/local-history/sources`
- `GET /v1/local-history/source-detection`
- `POST /v1/local-history/source-detection`
- `GET /v1/local-history/doctor?source=all|:source`
- `POST /v1/local-history/sources/:source/refresh?limit=100`
- `GET /v1/local-history/sources/:source/sessions?limit=25&offset=0`
- `GET /v1/local-history/sources/:source/sessions/:session_id/export?schema=audit-v1&format=json`

## Security caveat

The local-history bridge shells out to a configured `brick` binary with a fixed allowlist of `history` subcommands and arguments. It is intended for localhost development only and is disabled unless `brick-server serve --enable-local-history` is set. Do not expose this server to untrusted networks while the bridge is enabled.
