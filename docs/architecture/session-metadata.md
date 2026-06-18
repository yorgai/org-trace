---
status: active
---

# Session Metadata Structure

This document tracks Brick's source-session metadata structure and how each native source populates it. It is the working contract for `<BRICK_HOME>/metadata.sqlite`, `brick history sessions --format json`, and `brick history export --schema audit-v1|source-metadata-v1 --format json|csv`.

## Boundary

`metadata.sqlite` stores indexed session metadata. It does not store full transcript content. Full transcript records are formatted lazily from native storage by source-specific chunk providers, or copied into Brick blobs only through explicit evidence actions.

## Core table shape: `source_sessions`

| Column | Type | Meaning | JSON export |
| --- | --- | --- | --- |
| `source_id` | text | Brick source ID, such as `claude_code` or `codex_app`. | `source_id` |
| `external_session_id` | text | Native session ID, file stem, composer ID, or DB session ID. | `external_session_id`, `session_id` during migration |
| `title` | text nullable | Human-readable title. | `title` |
| `name` | text nullable | Provider name/title alias. | future `name` |
| `source_path` | text nullable | Native file/DB path. | `path`, `source_path` later |
| `source_uri` | text nullable | Native source URI if not file-backed. | `source_uri` later |
| `source_mtime` | RFC3339 text nullable | Native file/DB modified time. | `modified_at` |
| `source_size` | integer nullable | Native file/DB byte size. | `size_bytes` |
| `source_fingerprint` | text nullable | Provider-specific invalidation fingerprint. | `source_fingerprint` later |
| `parser_version` | text nullable | Parser version used for metadata extraction. | `parser_version` later |
| `session_created_at` | RFC3339 text nullable | Native session creation/first event time. | `created_at` |
| `session_updated_at` | RFC3339 text nullable | Native session update/last event time. | `updated_at` |
| `model` | text nullable | Model name when available. | `model` |
| `input_tokens` | integer nullable | Input token total when available. | `input_tokens` |
| `output_tokens` | integer nullable | Output token total when available. | `output_tokens` |
| derived | integer nullable | `input_tokens + output_tokens` when either side is present. | `total_tokens` |
| `repo_path` | text nullable | Workspace/repo path inferred from native metadata. | `repo_path` |
| `branch` | text nullable | Git branch when available. | `branch` |
| `files_changed` | integer nullable | Changed file count. | `files_changed` |
| `lines_added` | integer nullable | Added line count. | `lines_added` |
| `lines_removed` | integer nullable | Removed line count. | `lines_removed` |
| `touched_files_json` | JSON nullable | Touched file path array. | `touched_files` |
| `listable` | boolean/integer | Whether the row should appear in default lists. | future filter/export field |
| `discovered_at` | RFC3339 text | Brick discovery time. | internal |
| `last_seen_at` | RFC3339 text | Last time Brick saw the source row. | internal/sort |
| `created_at` | RFC3339 text | Metadata row creation time. | internal |
| `updated_at` | RFC3339 text | Metadata row update time. | internal |
| `metadata_json` | JSON nullable | Provider extras that are not first-class yet. | future `source_metadata` |

## Core table shape: `source_plans`

`source_plans` is the minimal durable index for native planning artifacts. It is keyed by `(source_id, external_plan_id)` and stores the plan evidence pointer plus parser metadata.

| Column | Type | Meaning |
| --- | --- | --- |
| `source_id` | text | Brick source ID, currently populated by `cursor_ide`. |
| `external_plan_id` | text | Native Cursor plan ID. |
| `title` | text nullable | Plan title/name when present; falls back to plan ID. |
| `source_path` | text nullable | Plan Markdown path such as `.cursor/plans/<plan>.plan.md` when exposed by Cursor. |
| `source_uri` | text nullable | Plan URI when available or derived from `source_path`. |
| `source_mtime` | RFC3339 text nullable | Backing Cursor state DB mtime for this first slice. |
| `parser_version` | text nullable | Plan parser version, currently `cursor-ide-plan-registry-v1`. |
| `discovered_at`, `last_seen_at`, `created_at`, `updated_at` | RFC3339 text | Metadata lifecycle timestamps. |
| `metadata_json` | JSON nullable | Provider extras, including the raw plan registry entry for now. |

## Core table shape: `source_plan_session_edges`

`source_plan_session_edges` stores recovered plan-to-session relationships. It deliberately does **not** require a matching `source_sessions` row so unresolved Cursor session IDs survive partial or damaged session header recovery.

| Column | Type | Meaning |
| --- | --- | --- |
| `source_plan_id` | integer FK | Local source plan row. |
| `external_session_id` | text | Native session/composer ID, preserved even when no session header row exists. |
| `role` | text enum | One of `created_by`, `edited_by`, `referenced_by`, or `built_by`. |
| `todo_ids_json` | JSON nullable | For `built_by`, Cursor todo IDs executed by that session. |
| `discovered_at`, `last_seen_at`, `created_at`, `updated_at` | RFC3339 text | Edge lifecycle timestamps. |
| `metadata_json` | JSON nullable | Provider-specific edge extras. |

## Raw plan history JSON

`brick history plans --source <source> --limit <n> --offset <n> --format json` is the audit-oriented read surface for indexed plans. The command refreshes the selected source first, then returns:

- `source_id`, `limit`, `offset`, `total`, and `has_more` for pagination.
- `plans[]` rows with the raw source-plan metadata fields: `plan_id` / `external_plan_id`, title, source path or URI, source mtime, parser version, lifecycle timestamps, and provider `metadata_json`.
- `edges[]` rows for the returned plan page, keyed by `external_plan_id` and preserving native `external_session_id` even when no matching `source_sessions` row exists. Edge rows include role, optional `todo_ids_json`, lifecycle timestamps, and provider `metadata_json`.

This slice intentionally exposes only raw JSON. It does not assign UI labels, task status, renderer hints, or app-specific plan semantics.

## Token accounting

Token fields are optional because not every source exposes them.

| Field | Rule |
| --- | --- |
| `input_tokens` | Store provider-reported input tokens. Include cache-read/cache-created input tokens when the provider accounts for them separately and the UI expects total prompt-side spend. |
| `output_tokens` | Store provider-reported output/completion tokens. Include reasoning tokens when the provider reports them as output-side spend and no separate first-class field exists yet. |
| `total_tokens` | Derived at JSON export time as `input_tokens + output_tokens` when either side exists. |
| Unknown | Use `null`, not `0`, when the provider does not expose token usage. |

## Source extraction inventory

### Claude Code

| Metadata | How Brick extracts it |
| --- | --- |
| Native storage | JSONL transcript files under configured `session_log_path` / `evidence_root`; discovery paths later expand to `~/.claude/projects/**/*.jsonl` and platform-specific roots. |
| `external_session_id` | File stem. |
| `source_path`, `source_mtime`, `source_size` | Filesystem metadata. |
| `parser_version` | `claude-code-jsonl-v1`. |
| `title` | First user message content, truncated to 200 chars; fallback file stem. |
| `session_created_at` | Minimum parsed JSONL `timestamp`. |
| `session_updated_at` | Maximum parsed JSONL `timestamp`. |
| `repo_path` | First non-empty top-level `cwd`. |
| `branch` | First non-empty top-level `gitBranch`. |
| `model` | First non-empty `message.model`. |
| `input_tokens` | Sum of `message.usage.input_tokens`, `cache_read_input_tokens`, and `cache_creation_input_tokens`. |
| `output_tokens` | Sum of `message.usage.output_tokens`. |
| `files_changed`, `lines_added`, `lines_removed`, `touched_files` | Planned: parse Claude edit/write tool calls. Current first slice leaves null/empty. |
| Full chunks | Planned lazy JSONL-to-chunk JSON formatting. |

### Codex App

| Metadata | How Brick extracts it |
| --- | --- |
| Native storage | JSONL session files under configured `session_log_path` / `evidence_root`; discovery paths later expand to `~/.codex/sessions/**/**/*.jsonl` and platform app support roots. |
| `external_session_id` | File stem. |
| `source_path`, `source_mtime`, `source_size` | Filesystem metadata. |
| `parser_version` | `codex-app-jsonl-v1`. |
| `title` | First `payload.type == "user_message"` message, truncated to 200 chars; fallback file stem. |
| `session_created_at` | Minimum top-level JSONL `timestamp`. |
| `session_updated_at` | Maximum top-level JSONL `timestamp`. |
| `repo_path` | First non-empty `payload.cwd`. |
| `branch` | Not currently exposed by Codex JSONL parser. |
| `model` | First non-empty `payload.model`. |
| `input_tokens` | Latest/observed `payload.total_token_usage.input_tokens` from `payload.type == "token_count"`. |
| `output_tokens` | Latest/observed `payload.total_token_usage.output_tokens` from `payload.type == "token_count"`. |
| `files_changed`, `lines_added`, `lines_removed`, `touched_files` | Parse `apply_patch` payloads from `function_call` and `custom_tool_call`. |
| Full chunks | Planned lazy JSONL-to-chunk JSON formatting. |

### Cursor IDE

| Metadata | How Brick should extract it |
| --- | --- |
| Native storage | Cursor `state.vscdb` SQLite `cursorDiskKV`. |
| Primary resilient session metadata path | `composer.composerHeaders.allComposers` for `name`, `createdAt`, `lastUpdatedAt`, `workspaceIdentifier`, `trackedGitRepos`, `subtitle`, `mode`, `isArchived`. |
| Full composer/chunk source path | `composerData:{composerId}`, `bubbleId:{composerId}:{bubbleId}`, and content blob keys for full chunk JSON formatting. |
| Plan/session edges | `composer.planRegistry` or `composer.planRegistry.{planId}` gives `uri.fsPath`, `createdBy`, `editedBy[]`, `referencedBy[]`, and `builtBy{sessionId: todoIds[]}`. Brick persists rows in `source_plans` and `source_plan_session_edges`, preserving unresolved session IDs even when `composer.composerHeaders.allComposers` has no matching header. |
| Token metadata | `contextTokensUsed` when available; Cursor does not always expose input/output split. Store split only when available; otherwise keep provider-specific values in `metadata_json`. |
| Impact metadata | Composer fields such as `totalLinesAdded`, `totalLinesRemoved`, and `filesChangedCount` when available. |
| Full chunks | Lazy DB-to-chunk JSON formatting resolves `composer.content.{hash}`-style text/JSON blobs in message/tool payloads for raw audit chunks. Window modes remain pending. |

### Windsurf

| Metadata | How Brick should extract it |
| --- | --- |
| Native storage | Windsurf `state.vscdb` SQLite `cursorDiskKV`. |
| Query method | Cursor-family composer/bubble key grammar. |
| Core metadata | `composerId`, `name`, `createdAt`, `lastUpdatedAt`, `modelConfig.modelName`, `contextTokensUsed`, `trackedGitRepos`, `workspaceIdentifier`. |
| Token metadata | `contextTokensUsed` when available; keep input/output split null unless source exposes split. |
| Full chunks | Lazy Cursor-family DB-to-chunk JSON formatting is implemented for composer/bubble rows and uses the shared Cursor-family content-blob resolver; validate Windsurf-specific content ID patterns with fixtures. |

### OpenCode

| Metadata | How Brick should extract it |
| --- | --- |
| Native storage | `opencode.db` SQLite. |
| Query method | `session` table for metadata via schema introspection; `message` and `part` tables for chunks when the DB exposes `message_id` and `session_id` on either `part` or `message`. |
| Core metadata | Requires `session.id`; optionally maps `session.title`, `session.directory`, `session.model`, `time_created`, `time_updated`, and archive flags (`time_archived`, `archived`, `is_archived`, `isArchived`). |
| Token metadata | Maps `tokens_input + tokens_cache_read + tokens_cache_write` as input and `tokens_output + tokens_reasoning` as output from `session` when present; otherwise aggregates matching columns from `part` joined to `message`. |
| Full chunks | First-pass lazy DB-to-chunk JSON formatting from `part` joined to `message`; chunk pointers include DB path plus native message/part IDs. Broader schema validation remains a follow-up. |

## Shared session export formats

Brick keeps the public export surface intentionally small. Source-specific providers may expose rich internal details, but user-facing auditing should converge on one of these schemas.

| Schema | Command | Purpose | Content boundary |
| --- | --- | --- | --- |
| `audit-v1` | `brick history export --source <source> --session-id <id> --schema audit-v1 --format json` | Stable cross-provider audit packet for humans, reviewers, and ORGII ingestion. | Normalized source, session, token, impact, evidence, and chunk sections. |
| `source-metadata-v1` | `brick history export --source <source> --session-id <id> --schema source-metadata-v1 --format json` | Loss-minimized export of the current metadata index row for debugging and provider parity checks. | Mirrors first-class `source_sessions` metadata plus provider extras. |
| CSV formatting | `brick history export --source <source> --session-id <id> --schema audit-v1 --format csv` | Spreadsheet/audit-table export for a specific session. | One row per chunk, with repeated session metadata/token/impact columns; metadata-only sources emit one row with empty chunk columns. |

Both schemas include a `chunks` array. Chunk objects may include optional raw source pointers (`source_id`, `source_path`, `source_record_key`, `source_line_number`, `source_message_id`, `source_part_id`) when the provider can identify the native record without expensive reconstruction. For Claude Code and Codex App, Brick lazily formats JSONL transcript records into chunk JSON and records path/line pointers. DB-backed providers attach DB paths and native row/key IDs where available. This preserves the final audit shape while keeping full transcript content out of `metadata.sqlite`.

## Current implementation status

| Source | Metadata status | Token status | Chunk status |
| --- | --- | --- | --- |
| Claude Code | First JSONL metadata parser in Brick. | Input/output extracted from `message.usage`. | JSONL-to-chunk JSON formatting. |
| Codex App | First JSONL metadata parser in Brick. | Input/output extracted from `token_count`. | JSONL-to-chunk JSON formatting. |
| Cursor IDE | Provider reads `composer.composerHeaders.allComposers` for sessions and `composer.planRegistry` / `composer.planRegistry.{planId}` for durable plan and plan-session edge rows. | Split not available in first provider; context token handling remains pending. | Full-session raw formatter reads `composerData:{composerId}` and `bubbleId:{composerId}:{bubbleId}` and dereferences `composer.content.{hash}`-style blobs for message/tool text and JSON; window modes remain pending. |
| Windsurf | First provider reads `composerData:%` rows from `cursorDiskKV` and extracts composer metadata. | `contextTokensUsed` is mapped to input tokens when present; output split remains null. | Shared Cursor-family composer/bubble formatter implemented with content-blob resolver; Windsurf fixture validation pending. |
| OpenCode | First DB metadata provider ported and registered. | Input/output extracted from `session` tokens or aggregated from `part` tokens. | First-pass lazy DB-to-chunk JSON formatting; source pointer metadata and additional schema variants pending. |
