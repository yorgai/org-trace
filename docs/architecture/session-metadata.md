---
status: active
---

# Session Metadata Structure

This document tracks Brick's source-session metadata structure and how each native source populates it. It is the working contract for `<BRICK_HOME>/metadata.sqlite` and `brick history sessions --format json`.

## Boundary

`metadata.sqlite` stores indexed session metadata. It does not store full transcript content. Full replay is loaded lazily from native storage by source-specific chunk providers, or copied into Brick blobs only through explicit evidence actions.

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
| Full chunks | Planned lazy JSONL replay into `ActivityChunk`-compatible JSON. |

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
| Full chunks | Planned lazy JSONL replay into `ActivityChunk`-compatible JSON. |

### Cursor IDE

| Metadata | How Brick should extract it |
| --- | --- |
| Native storage | Cursor `state.vscdb` SQLite `cursorDiskKV`. |
| Primary resilient session metadata path | `composer.composerHeaders.allComposers` for `name`, `createdAt`, `lastUpdatedAt`, `workspaceIdentifier`, `trackedGitRepos`, `subtitle`, `mode`, `isArchived`. |
| Full composer/chunk path | `composerData:{composerId}`, `bubbleId:{composerId}:{bubbleId}`, and content blob keys for full replay. |
| Plan/session edges | `composer.planRegistry.{planId}` gives `uri.fsPath`, `createdBy`, `editedBy[]`, `referencedBy[]`, and `builtBy{sessionId: todoIds[]}`. Resolve session IDs through `composer.composerHeaders.allComposers`. |
| Token metadata | `contextTokensUsed` when available; Cursor does not always expose input/output split. Store split only when available; otherwise keep provider-specific values in `metadata_json`. |
| Impact metadata | Composer fields such as `totalLinesAdded`, `totalLinesRemoved`, and `filesChangedCount` when available. |
| Full chunks | Lazy DB replay with window modes. |

### Windsurf

| Metadata | How Brick should extract it |
| --- | --- |
| Native storage | Windsurf `state.vscdb` SQLite `cursorDiskKV`. |
| Query method | Cursor-family composer/bubble key grammar. |
| Core metadata | `composerId`, `name`, `createdAt`, `lastUpdatedAt`, `modelConfig.modelName`, `contextTokensUsed`, `trackedGitRepos`, `workspaceIdentifier`. |
| Token metadata | `contextTokensUsed` when available; keep input/output split null unless source exposes split. |
| Full chunks | Lazy Cursor-family DB replay. |

### OpenCode

| Metadata | How Brick should extract it |
| --- | --- |
| Native storage | `opencode.db` SQLite. |
| Query method | `session` table for metadata; `message` and `part` tables for chunks. |
| Core metadata | `session.id`, `session.title`, `session.directory`, `session.model`, `time_created`, `time_updated`, archive flags. |
| Token metadata | `tokens_input + tokens_cache_read + tokens_cache_write` as input; `tokens_output + tokens_reasoning` as output. |
| Full chunks | Lazy DB replay from `part` joined to `message`. |

## Current implementation status

| Source | Metadata status | Token status | Chunk status |
| --- | --- | --- | --- |
| Claude Code | First JSONL metadata parser in Brick. | Input/output extracted from `message.usage`. | Planned. |
| Codex App | First JSONL metadata parser in Brick. | Input/output extracted from `token_count`. | Planned. |
| Cursor IDE | First metadata-only provider in Brick using `composer.composerHeaders.allComposers`. | Split not available in first provider; context token handling remains pending. | Planned. |
| Windsurf | Documented; not ported yet. | Context token metadata documented. | Planned. |
| OpenCode | Documented; not ported yet. | Input/output strategy documented. | Planned. |
