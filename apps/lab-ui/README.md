# Brick Lab UI

A small React/Vite dashboard for exercising Brick's local server while developing features.

## Run locally

From the repository root, start the Brick server:

```bash
cargo run -p brick-server -- serve --bind 127.0.0.1:7821 --data-dir .brick-server
```

In another terminal, install and run the UI:

```bash
cd apps/lab-ui
npm install
npm run dev
```

Open <http://127.0.0.1:5454>. The Vite dev server proxies `/api/*` to `http://127.0.0.1:7821`, so the Server URL field can stay blank.

## Useful probes

- `GET /health`
- `GET /v1/repos/:repo_id/index/status`
- `GET /v1/repos/:repo_id/sessions?limit=20`
- `GET /v1/repos/:repo_id/events?limit=20`

Use the terminal recipes in the dashboard to seed data, push a repo into the lab server, and compare API output with direct `curl` calls.
