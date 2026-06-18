# External source provider fixtures

This directory holds sanitized, minimal fixtures for validating Brick external source providers. Fixtures should be real-ish enough to capture provider schemas and edge cases, but must never include private user data or large native databases.

## Directory convention

Use one directory per source and scenario:

```text
external_sources/
  <source_id>/
    <scenario_name>/
      manifest.json
      logs/                  # JSONL/text fixtures when applicable
      db-spec/               # text SQL/JSON specs for generated SQLite DBs
```

Supported source IDs are `cursor_ide`, `windsurf`, `opencode`, `claude_code`, and `codex_app`.

Each scenario must include a `manifest.json` with:

- `source`: one supported source ID.
- `description`: short human-readable purpose.
- `format`: `jsonl`, `cursor_kv_sqlite`, or `opencode_sqlite`.
- `dbSpecPath`: required for generated SQLite fixtures; relative path to a spec under `db-spec/`.
- `profile`: provider path hints relative to the scenario directory for committed text/log fixtures. Generated SQLite fixtures can leave this empty because the harness injects a temporary DB path.
- `expected`: stable metadata, chunk assertions, and optional plan assertions.

## Generated SQLite fixture policy

Do not commit real `state.vscdb`, `opencode.db`, WAL/SHM files, or other binary provider stores. SQLite-backed fixtures should be generated at test time from text specs:

- `cursor_kv_sqlite`: JSON object with a `rows` array. Each row has a `key` and JSON `value`; the harness creates a temporary `cursorDiskKV` table and serializes each value into the table. Use this for Cursor IDE and Windsurf Cursor-family stores.
- `opencode_sqlite`: SQL file that creates only the minimal `session`, `message`, and `part` schema needed by the provider parser. Keep the SQL deterministic and synthetic.

Only commit a binary DB when there is a strong compatibility reason that cannot be represented as text, and document that reason in the manifest. Prefer adding a small builder extension to the harness instead.

## Privacy and size rules

Do not commit private data. Before adding a fixture, replace or remove:

- Real prompts, file contents, command output, proprietary code, stack traces, and API responses.
- Names, emails, usernames, repository names, absolute home paths, hostnames, tokens, keys, URLs, and organization identifiers.
- Full production databases, WAL/SHM files, caches, embeddings, screenshots, and binary blobs.

Use synthetic placeholder values such as `/workspace/repo`, `feature/example`, `example-model`, `session-basic`, and short messages like `Run tests`. Keep fixture transcripts to the minimum number of records needed to validate parser behavior.

## Adding a sanitized real-ish fixture

1. Copy only the smallest source records needed for the behavior being tested.
2. Redact every private value and replace it with deterministic placeholders.
3. Normalize timestamps to fixed values, ideally in 2026 UTC.
4. Replace absolute paths with `/workspace/repo` or `/workspace/<source>`.
5. Keep native IDs stable but fake, such as `composer-basic` or `session-basic`.
6. For SQLite-backed sources, translate the minimal rows into `db-spec/` text and let the test harness generate the temporary DB.
7. Run the provider fixture tests and inspect any expected data before committing.

The committed `claude_code/basic_session` and `codex_app/patch_session` scenarios are templates for JSONL-backed providers. Cursor IDE, Windsurf, and OpenCode scenarios use generated SQLite DBs from text specs rather than committed `state.vscdb` or `opencode.db` files.
