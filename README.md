# Brick

Brick is a self-host-first provenance CLI and server for tracking human and AI agent work around missions, sessions, artifacts, files, diffs, imports, and commits.

The name points at a durable unit of accountable work: like historical bricks signed by their makers, each recorded mission, session, and artifact can carry its provenance forward.

## Status

Brick is at an MVP phase for local-first trace capture plus unauthenticated self-hosted sync. The local JSONL event log remains the source of truth; JSON and SQLite projections are rebuildable derived indexes. The server is suitable for localhost and lab use only until authentication and repo authorization are added.

## Packages

Open-source crates (the default `cargo build`):

- `brick`: standalone CLI client (also an MCP server via `brick mcp-serve`)
- `brick-protocol`: shared provenance event schema
- `brick-core`: local storage, indexing, repo context, and announcements
- `brick-importers`: explicit-file importers for agent transcripts and CI summaries

Proprietary crates (excluded from the default build; see "Build surface" below):

- `brick-sync`: cross-server sync client + wire protocol — compiled only with `--features sync`
- `brick-server`: self-hosted provenance remote — built only with `cargo build -p brick-server`

## Build surface (open-source vs proprietary)

Cross-server sync is proprietary and stays out of the open-source build. The
default build produces a `brick` binary with **no** sync command and no
dependency on `brick-sync`:

```bash
cargo build                       # open-source: no sync, no server
cargo build -p brick              # the open-source CLI binary
cargo build -p brick --features sync   # private: adds the `brick sync` command
cargo build -p brick-server       # private: the self-hosted remote
```

`brick mcp-serve` (the MCP server surface) is fully open-source and exposes
memory, planning, work-item management, and coordination tools to any
MCP-capable agent — none of it depends on sync. The `crates/sync` and
`crates/server` directories can be moved into a private submodule/overlay and
dropped from the workspace `members` list without affecting the open build.

## MVP walkthrough

From a Git repository, initialize Brick and configure a source profile. `init` scans common local agent stores (ORGII, Cursor, Claude Code, Codex, Windsurf, and OpenCode). In an interactive terminal it lets you select discovered sources with arrow keys, space, and enter; in scripts it prints findings without blocking.

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

Native source profiles can be listed and ingested without manually locating each transcript file. Ingest records a Brick session plus a metadata-only log pointer by default:

```bash
cargo run -p brick -- --source claude_code import native list --limit 20
cargo run -p brick -- --source claude_code import native ingest --external-session-id <native-id> --mission "$mission_id"
```

The read-through history surface refreshes native source metadata into `<BRICK_HOME>/metadata.sqlite` and emits JSON for ORGII-style callers:

```bash
cargo run -q -p brick -- history sources --format json
cargo run -q -p brick -- history doctor --source all --format json
cargo run -q -p brick -- history sessions --source claude_code --limit 20 --format json
cargo run -q -p brick -- history plans --source cursor_ide --limit 20 --offset 0 --format json
cargo run -q -p brick -- history recent-paths --source all --limit 20 --format json
cargo run -q -p brick -- history chunks --source claude_code --session-id <native-id> --format json
cargo run -q -p brick -- history chunks --source codex_app --session-id <native-id> --format json
cargo run -q -p brick -- history export --source claude_code --session-id <native-id> --schema audit-v1 --format json
cargo run -q -p brick -- history export --source claude_code --session-id <native-id> --schema source-metadata-v1 --format json
cargo run -q -p brick -- history export --source claude_code --session-id <native-id> --schema audit-v1 --format csv
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

Humans and agent manage Missions together. A Mission is the accountability unit that replaces a task or work item: it carries the title, specification, status, project grouping, linked sessions, artifacts, and proof of work.

Sessions are evidence attached to Missions. A Session may be produced by an agent or by a human. Human sessions can record manual work, design review, meetings, QA passes, or operational activity. The lightweight Session metadata is synced by default: source app, actor, timestamps, linked artifacts, linked missions, transcript availability, and last update time. Full transcripts or recordings are optional content-addressed evidence.

Artifacts are the work products and proof attached to Missions and Sessions. They can represent decisions, reviews, diffs, CI results, documents, screenshots, recordings, notes, or uploaded files. Video recordings and other large human proof-of-work files should be stored as artifact attachments so events keep only metadata, hashes, and storage URIs.

## Agent awareness

`brick agent install` injects a Brick instruction block into the memory files
coding agents read as standing context — `CLAUDE.md` (Claude Code), `AGENTS.md`
(Codex, Cursor, Copilot, OpenCode, …), and `GEMINI.md` (Gemini). The block tells
the agent to call `brick metadata recall --path <file> --format json` before
editing. That command returns indexed session metadata — who touched the file,
why, change size, and transcript chunk hints — without loading full transcripts by
default.

```bash
brick agent install            # inject into this repo's CLAUDE.md/AGENTS.md/GEMINI.md
brick agent install --target claude   # one tool only
brick agent install --global   # per-user memory locations (best-effort)
brick agent status             # report present / stale / absent per file
brick agent uninstall          # remove only Brick's block
```

The injected text lives between `<!-- brick:managed:start v=N -->` and
`<!-- brick:managed:end -->` sentinels. Edits are confined to that region and
written atomically, so a user's existing memory file is never clobbered;
re-running `install` is idempotent and rolls the block forward when the template
version changes. `brick init` offers to run this for the current repo. `brick
metadata` and `brick history` commands emit `--format json` so agents can parse
them directly.

## MCP capability kit

`brick mcp-serve` runs Brick as an MCP server over stdio, turning one install
into a shared work surface any MCP-capable agent (Claude Code, Cursor, ORGII, …)
can call. `brick agent install` registers it automatically. The tools group into
three capabilities:

- **Memory** — `explore_memory`, `recall_file`, `search_sessions`, `read_session`.
- **Planning & work-item management** — `current_context`, `list_missions`,
  `show_mission`, `manage_mission` (create/update a goal), `record_artifact`
  (log a deliverable), `attach_evidence` (back it with files).
- **Coordination & awareness** — `announce_work` and `list_announcements` (the
  cross-session bulletin board), `live_sessions` (who's running now). `recall_file`
  also surfaces active claims on the file you ask about.

A natural agent flow: `current_context` → `list_missions` → `manage_mission`
(create) → do the work → `record_artifact` → `attach_evidence` → `announce_work`.
All of this is open-source and independent of the proprietary sync layer.

## Local storage model

Local writes use append-only JSONL under `.brick/provenance/` by default. `brick init` automatically adds `.brick/` to the repository `.gitignore` idempotently, because Brick local state is not source code and should not be committed. The effective store root resolves in this order: `--store-root`, `BRICK_STORE_ROOT`, selected source profile `store_root`, then repo-local `.brick/provenance`.

Repo-level behavior lives in `.brick/config.toml`; source-specific paths live in `.brick/sources/<name>.toml`. `brick init` and `brick source scan` discover common external stores such as `~/.orgii`, ORGII `sessions.db`, Cursor `state.vscdb`, Claude Code `~/.claude/projects`, Codex `sessions/`, Windsurf `state.vscdb`, and OpenCode `opencode.db`. Local Brick events default to metadata-only pointers with hashes, sizes, source paths, and availability. Full transcript or recording bytes are copied into local content-addressed blobs only when `--copy` is passed or the config/source profile opts into `default_full_evidence_upload = true`.

`index.json`, `brick.sqlite`, and `views/` are derived indexes under the effective store. Rebuilding them never mutates the source event log. `views/` contains agent-readable Markdown files for orgs, projects, missions, sessions, and artifacts. Pull writes remote events to separate inbound logs and deduplicates them by event ID when rebuilding indexes.

Global source-history metadata lives under `<BRICK_HOME>/metadata.sqlite` (`~/.brick/metadata.sqlite` by default). This file is the source metadata index, not a second cache layer or transcript copy. `brick history sessions`, `brick history plans`, `brick history recent-paths`, and `brick import native list/ingest` refresh native source-session or source-plan rows into that metadata index before returning results.

## Documentation

- `docs/architecture/architecture.md`: source metadata index architecture and Mermaid diagrams
- `docs/architecture/source-querying.md`: platform-specific querying methods and history JSON/CSV contracts
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

### Local React lab UI

`apps/lab-ui` is a small Vite/React dashboard for exercising the localhost server routes while developing Brick features.

```bash
cargo run -p brick-server -- serve --bind 127.0.0.1:5353 --data-dir .brick-server --enable-local-history --brick-bin /Users/laptop-h/.cargo/shared-target/debug/brick --repo-root "$PWD"
cd apps/lab-ui
npm install
npm run dev
```

Open <http://127.0.0.1:5454>. The UI proxies `/api/*` to `http://127.0.0.1:5353` by default.

## License

AGPL-3.0-or-later.