# Brick

Brick is a self-host-first provenance CLI and server for tracking human and AI agent work around missions, sessions, artifacts, files, diffs, imports, and commits.

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

From a Git repository, initialize Brick and configure a source profile. `init` scans common local agent stores (ORGII, Cursor, Claude Code, Codex, and OpenCode). In an interactive terminal it lets you select discovered sources with arrow keys, space, and enter; in scripts it prints findings without blocking.

```bash
cargo run -p brick -- init
cargo run -p brick -- source config --default-full-evidence-upload false --metadata-only-local true
cargo run -p brick -- source scan --write-defaults
cargo run -p brick -- source use --name cursor
```

You can still override paths manually when the scanner does not find the desired store:

```bash
cargo run -p brick -- source configure --name cursor --app-id cursor --actor-id agent-1 --actor-type agent --evidence-root ~/.orgii --cursor-state-db-path "$HOME/Library/Application Support/Cursor/User/globalStorage/state.vscdb" --default-full-evidence-upload false --notes "Cursor agent"
```

Create an Org, Project, Mission, agent-friendly current Session, and Artifacts:

```bash
org_id=$(cargo run -p brick -- --source cursor org create "Acme Engineering" | awk -F= '/^org_id=/ {print $2}')
project_id=$(cargo run -p brick -- --source cursor project create --org "$org_id" "Brick MVP" | awk -F= '/^project_id=/ {print $2}')
mission_id=$(cargo run -p brick -- --source cursor mission create --project "$project_id" "Ship MVP" --status active | awk -F= '/^mission_id=/ {print $2}')
session_id=$(cargo run -p brick -- --source cursor session start --mission "$mission_id" --name "MVP session" --set-current --print-env | awk -F= '/^session_id=/ {print $2}')
artifact_id=$(cargo run -p brick -- --source cursor artifact create --mission "$mission_id" --session "$session_id" --kind decision "Implementation decision" --body "Record the MVP path" | awk -F= '/^artifact_id=/ {print $2}')

cargo run -p brick -- --source cursor artifact update "$artifact_id" --session "$session_id" --kind review --title "Reviewed decision"
cargo run -p brick -- --source cursor evidence attach --artifact "$artifact_id" --session "$session_id" --path ./report.txt --content-type text/plain
cargo run -p brick -- --source cursor evidence log --session "$session_id" --path ./session.jsonl --format jsonl --source cursor
cargo run -p brick -- --source cursor evidence diff --artifact "$artifact_id" --session "$session_id" --target working
```

Rebuild and query local derived views:

```bash
cargo run -p brick -- --source cursor maintenance index rebuild
cargo run -p brick -- --source cursor maintenance index status
cargo run -p brick -- --source cursor maintenance db rebuild
cargo run -p brick -- --source cursor maintenance db sessions --limit 20 --app-id cursor --actor-id agent-1
cargo run -p brick -- --source cursor maintenance db artifacts --limit 20 --session "$session_id" --mission "$mission_id"
```

Import explicit transcript and CI fixtures, or record human proof of work:

```bash
cargo run -p brick -- --source cursor import cursor --path ./cursor-session.jsonl --mission "$mission_id" --session "$session_id" --app-session-id cursor-native-1 --app-session-name "Cursor MVP"
cargo run -p brick -- --source cursor import ci --path ./ci-job.json --mission "$mission_id" --session "$session_id"

human_session_id=$(cargo run -p brick -- --actor-type human --actor-id alice session start --mission "$mission_id" --name "Manual QA pass" | awk -F= '/^session_id=/ {print $2}')
human_artifact_id=$(cargo run -p brick -- --actor-type human --actor-id alice artifact create --mission "$mission_id" --session "$human_session_id" --kind acceptance "QA sign-off" --body "Manual pass completed" | awk -F= '/^artifact_id=/ {print $2}')
cargo run -p brick -- --actor-type human --actor-id alice evidence attach --artifact "$human_artifact_id" --session "$human_session_id" --path ./qa-recording.mp4 --content-type video/mp4
```

Run a local server, push by repo ID, and pull into another store:

```bash
cargo run -p brick-server -- serve --bind 127.0.0.1:7821 --data-dir .brick-server
cargo run -p brick -- sync push --remote http://127.0.0.1:7821 --repo-id repo-a --org-id "$org_id"
cargo run -p brick -- --store-root /tmp/brick-store sync pull --remote http://127.0.0.1:7821 --repo-id repo-a --org-id "$org_id"
curl http://127.0.0.1:7821/v1/repos/repo-a/index/status
curl 'http://127.0.0.1:7821/v1/repos/repo-a/sessions?limit=20'
```

## End-to-end smoke harness

`scripts/smoke_mvp.sh` exercises the MVP in temporary Git repositories and stores. It covers init, source profiles, orgs, projects, missions, sessions, artifact create/update, evidence attachments/logs/diffs/files, local JSON and SQLite indexes, Cursor and CI imports, server startup, repo-scoped sync push, repo-scoped sync pull into a second store, server index/session routes, and cleanup.

```bash
scripts/smoke_mvp.sh
```

Set `BRICK_SMOKE_PORT` if the default local port is busy.

## Product model

Humans manage Missions. A Mission is the accountability unit that replaces a task or work item: it carries the title, specification, status, project grouping, linked sessions, artifacts, and proof of work.

Sessions are evidence attached to Missions. A Session may be produced by an agent or by a human. Human sessions can record manual work, design review, meetings, QA passes, or operational activity. The lightweight Session metadata is synced by default: source app, actor, timestamps, linked artifacts, linked missions, transcript availability, and last update time. Full transcripts or recordings are optional content-addressed evidence.

Artifacts are the work products and proof attached to Missions and Sessions. They can represent decisions, reviews, diffs, CI results, documents, screenshots, recordings, notes, or uploaded files. Video recordings and other large human proof-of-work files should be stored as artifact attachments so events keep only metadata, hashes, and storage URIs.

## Local storage model

Local writes use append-only JSONL under `.brick/provenance/` by default. `brick init` automatically adds `.brick/` to the repository `.gitignore` idempotently, because Brick local state is not source code and should not be committed. The effective store root resolves in this order: `--store-root`, `BRICK_STORE_ROOT`, selected source profile `store_root`, then repo-local `.brick/provenance`.

Repo-level behavior lives in `.brick/config.toml`; source-specific paths live in `.brick/sources/<name>.toml`. `brick init` and `brick source scan` discover common external stores such as `~/.orgii`, ORGII `sessions.db`, Cursor `state.vscdb`, Claude Code `~/.claude/projects`, Codex `sessions/`, and OpenCode `opencode.db`. Local Brick events default to metadata-only pointers with hashes, sizes, source paths, and availability. Full transcript or recording bytes are copied into local content-addressed blobs only when `--copy` is passed or the config/source profile opts into `default_full_evidence_upload = true`.

`index.json`, `brick.sqlite`, and `views/` are derived caches under the effective store. Rebuilding them never mutates the source event log. `views/` contains agent-readable Markdown files for orgs, projects, missions, sessions, and artifacts. Pull writes remote events to separate inbound logs and deduplicates them by event ID when rebuilding indexes.

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
