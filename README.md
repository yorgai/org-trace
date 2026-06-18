# Brick

Brick is a self-host-first provenance CLI and server for tracking human and AI agent execution history around missions, sessions, artifacts, files, diffs, imports, and commits.

The name points at a durable unit of accountable work: like historical bricks signed by their makers, each recorded mission, session, and artifact can carry its provenance forward.

## Status

Brick is at an MVP phase for local-first trace capture plus unauthenticated self-hosted sync. The local JSONL event log remains the source of truth; JSON and SQLite indexes are rebuildable caches. The server is suitable for localhost and lab use only until authentication and repo authorization are added.

## Packages

- `brick`: standalone CLI client
- `brick-server`: self-hosted provenance remote
- `brick-protocol`: shared event schema and sync types
- `brick-core`: local storage, indexing, repo context, and sync primitives
- `brick-importers`: explicit-file importers for agent transcripts and CI summaries

## MVP walkthrough

From a Git repository, initialize Brick and configure a source profile:

```bash
cargo run -p brick -- init
cargo run -p brick -- source configure --name cursor --app-id cursor --actor-id agent-1 --actor-type agent --notes "Cursor agent"
cargo run -p brick -- source use --name cursor
```

Create a mission, start an agent-friendly current session, and record artifacts:

```bash
mission_id=$(cargo run -p brick -- --source cursor mission create "Ship MVP" | awk -F= '/^mission_id=/ {print $2}')
session_id=$(cargo run -p brick -- --source cursor session start --mission "$mission_id" --name "MVP session" --set-current --print-env | awk -F= '/^session_id=/ {print $2}')
artifact_id=$(cargo run -p brick -- --source cursor artifact decision --mission "$mission_id" --session "$session_id" "Implementation decision" --body "Record the MVP path" | awk -F= '/^artifact_id=/ {print $2}')

cargo run -p brick -- --source cursor artifact update --artifact "$artifact_id" --session "$session_id" --kind review --title "Reviewed decision"
cargo run -p brick -- --source cursor artifact upload --artifact "$artifact_id" --session "$session_id" --path ./report.txt --content-type text/plain
cargo run -p brick -- --source cursor session upload-log --session "$session_id" --path ./session.jsonl --format jsonl --source cursor
cargo run -p brick -- --source cursor diff capture --artifact "$artifact_id" --session "$session_id" --target working
```

Rebuild and query local derived views:

```bash
cargo run -p brick -- --source cursor index rebuild
cargo run -p brick -- --source cursor index status
cargo run -p brick -- --source cursor db rebuild
cargo run -p brick -- --source cursor db sessions --limit 20 --app-id cursor --actor-id agent-1
cargo run -p brick -- --source cursor db artifacts --limit 20 --session "$session_id" --mission "$mission_id"
```

Import explicit transcript and CI fixtures:

```bash
cargo run -p brick -- --source cursor import cursor --path ./cursor-session.jsonl --mission "$mission_id" --session "$session_id" --app-session-id cursor-native-1 --app-session-name "Cursor MVP"
cargo run -p brick -- --source cursor import ci --path ./ci-job.json --mission "$mission_id" --session "$session_id"
```

Run a local server, push by repo ID, and pull into another store:

```bash
cargo run -p brick-server -- serve --bind 127.0.0.1:7821 --data-dir .brick-server
cargo run -p brick -- push --remote http://127.0.0.1:7821 --repo-id repo-a
cargo run -p brick -- --store-root /tmp/brick-store pull --remote http://127.0.0.1:7821 --repo-id repo-a
curl http://127.0.0.1:7821/v1/repos/repo-a/index/status
curl 'http://127.0.0.1:7821/v1/repos/repo-a/sessions?limit=20'
```

## End-to-end smoke harness

`scripts/smoke_mvp.sh` exercises the MVP in temporary Git repositories and stores. It covers init, source profiles, missions, sessions, artifact create/update/upload, session log upload, working and staged diff capture, local JSON and SQLite indexes, Cursor and CI imports, server startup, repo-scoped push, repo-scoped pull into a second store, server index/session routes, and cleanup.

```bash
scripts/smoke_mvp.sh
```

Set `BRICK_SMOKE_PORT` if the default local port is busy.

## Local storage model

Local writes use append-only JSONL under `.brick/provenance/` by default. The effective store root resolves in this order: `--store-root`, `BRICK_STORE_ROOT`, selected source profile `store_root`, then repo-local `.brick/provenance`. Artifact attachments and session logs are copied into content-addressed blob storage; events store metadata, hashes, and storage URIs rather than inlining bytes.

`index.json` and `brick.sqlite` are derived caches under the effective cache directory. Rebuilding them never mutates the source event log. Pull writes remote events to separate inbound logs and deduplicates them by event ID when rebuilding indexes.

## Documentation

- `docs/architecture/README.md`: current architecture and phase status
- `docs/protocol/README.md`: event families, envelope fields, sync routes, and query routes
- `docs/self-hosting/README.md`: local server operation, push/pull, repo IDs, and Cursor notes
- `examples/`: explicit importer examples for Cursor, Codex, Claude Code, and CI

## Development

```bash
cargo fmt --all
cargo check --workspace
cargo test --workspace
cargo doc --workspace --no-deps
```

## License

AGPL-3.0-or-later.
