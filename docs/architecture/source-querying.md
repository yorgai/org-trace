---
status: active
---

# Source Querying Inventory

This document tracks how Brick should query native coding-agent history sources, extract metadata, persist source metadata rows, and export JSON/CSV for consumers such as ORGII. The architecture boundary is defined in `architecture.md`.

Terminology:

- `metadata.sqlite` is the **source metadata index**.
- Native source storage remains the raw source of truth for external transcripts and app history.
- Full transcript bytes are not copied into Brick unless the user explicitly requests evidence copy/upload.
- Source-specific chunk providers should lazy-read native storage and return normalized history DTOs.
- Brick now has its own native source providers for all supported platforms, with real metadata extraction, real lazy chunk loading (`format_chunks`), and incremental refresh shipped. ORGII's `orgtrack-core` readers remain a reference, but Brick is no longer waiting to absorb them.

## Shared export contract

### Session metadata JSON

`brick history sessions --source <source> --format json` should return stable session rows. Provider-specific fields can live under `sourceMetadata` until promoted to first-class columns.

| Field | Meaning | Source metadata index mapping |
| --- | --- | --- |
| `source` | Brick source ID, such as `cursor_ide`, `claude_code`, `codex_app`, `opencode`, or `windsurf`. | `source_id` |
| `sourceSessionId` | Native source session/composer/file ID. | `external_session_id` or provider-specific `source_session_id` |
| `sessionId` | Brick/consumer-visible session ID. May keep ORGII-compatible prefixes during migration. | Derived from source + `sourceSessionId` or stored in metadata JSON |
| `name` / `title` | Human label. | `title` / `name` |
| `createdAt` | Session creation time. | provider metadata or first event timestamp |
| `updatedAt` | Last activity/update time. | `source_mtime`, provider updated timestamp, or max event timestamp |
| `status` | Normalized read-only status. | provider metadata; usually `completed` for imported history |
| `readOnly` | Whether consumer should block send/mutate actions. | Always `true` for external/native history |
| `model` | Model name when known. | provider metadata |
| `inputTokens` | Input/cache token total when known. | provider metadata |
| `outputTokens` | Output/reasoning token total when known. | provider metadata |
| `totalTokens` | `inputTokens + outputTokens`. | derived |
| `repoPath` / `workspacePath` | Workspace or repo path inferred from source. | source metadata index workspace/repo columns |
| `repoName` | Basename of `repoPath`. | derived |
| `branch` | Git branch when known. | provider metadata |
| `filesChanged` | Changed file count. | source impact stats |
| `linesAdded` | Added line count. | source impact stats |
| `linesRemoved` | Removed line count. | source impact stats |
| `touchedFiles` | Files touched by the session. | source impact stats JSON |
| `sourcePath` | Native file/DB path. | `source_path` |
| `sourceRecordKey` | Native row/key/file stem pointer. | `source_record_key` in metadata JSON or dedicated column |
| `sourceMtimeMs` | Native file/DB modified time. | `source_mtime` |
| `sourceSizeBytes` | Native file/DB size. | `source_size` |
| `sourceFingerprint` | Provider-specific invalidation fingerprint. | `source_fingerprint` |
| `parserVersion` | Provider parser version used for metadata extraction. | `parser_version` |
| `sourceMetadata` | Provider-specific raw or semi-normalized metadata. | `metadata_json` |

### Chunk JSON

`brick history chunks --source <source> --session-id <id> --format json` should return a stable `ActivityChunk`-compatible shape during ORGII migration. This is raw source-record formatting for audit/consumer APIs; it is not app rendering logic:

| Field | Meaning |
| --- | --- |
| `chunkId` / `chunk_id` | Stable chunk ID within the session. |
| `sessionId` / `session_id` | Consumer-visible session ID. |
| `actionType` / `action_type` | `raw`, `assistant`, `thinking`, or `tool_call`. |
| `function` | Canonical function, such as `user_message`, `assistant`, `thinking`, `run_command_line`, or `edit_file_by_replace`. |
| `args` | Tool args or normalized message args. |
| `result` | Message/tool result payload. |
| `createdAt` / `created_at` | RFC3339 timestamp. |
| `source_id` / `sourceId` | Optional Brick/source-provider ID. |
| `source_path` / `sourcePath` | Optional native file or DB path evidence pointer. |
| `source_record_key` / `sourceRecordKey` | Optional native row/key pointer, such as Cursor-family `bubbleId:{composerId}:{bubbleId}`. |
| `source_line_number` / `sourceLineNumber` | Optional JSONL line number for file-backed sources. |
| `source_message_id` / `sourceMessageId` | Optional native message/tool call ID where the provider exposes one. |
| `source_part_id` / `sourcePartId` | Optional native part/bubble ID where the provider exposes one. |

### Source diagnostics JSON

`brick history doctor --source <source|all> --format json` returns raw diagnostic rows for configured source providers. It is intentionally JSON-only and does not apply app rendering semantics. Each row reports profile presence/selection, provider and parser kind, configured path fields with existence/readability checks, provider metadata listing status or structured error string, indexed source-session/source-plan counts from `metadata.sqlite` when available, provider notes, and accumulated errors.

### File-level session attribution

`brick history file-session-blame --path <repo-relative-path> --format json` returns raw file/session attribution rows. The command name intentionally avoids `git blame`: rows are file-level, session-level evidence, not exact line authorship.

The first projection merges two auditable sources:

- Runtime provenance from the rebuildable SQLite cache: `diff.captured` events provide per-file additions/removals plus diff pointers, and `artifact.file_ref_recorded` events provide explicit file/session references.
- Backfilled source metadata from `metadata.sqlite`: `source_sessions.touched_files_json` identifies files touched by external/native sessions, with `files_changed`, `lines_added`, `lines_removed`, source/app metadata, actor metadata where present, and pointers back to source storage.

The JSON response includes `status` (`ok`, `empty`, or `error`), `errors`, and raw rows with `file_path`, `session_id` or `external_session_id`, `source_id` / `app_id`, actor fields, `evidence_kind`, `last_seen_at`, impact counts, `confidence`, and `source_pointer`. Empty results are explicit (`status = "empty"`), while runtime index rebuild/query failures or source metadata refresh/query failures are surfaced in `errors` rather than silently returning empty data.

True line-level blame remains a separate future projection requiring commit/range mapping, patch hunk parsing, and correlation from session-level evidence to final file lines.

### Session export formatting

`brick history export` is the specific-session export surface. JSON is canonical and loss-minimized. CSV is a convenience formatting for audit tables and spreadsheets.

| Format | Command shape | Treatment |
| --- | --- | --- |
| JSON | `brick history export --source <source> --session-id <id> --schema audit-v1 --format json` | Full structured audit packet. |
| JSON metadata | `brick history export --source <source> --session-id <id> --schema source-metadata-v1 --format json` | Current source metadata index row plus chunks when supported. |
| CSV | `brick history export --source <source> --session-id <id> --schema audit-v1 --format csv` | One row per formatted chunk with repeated session metadata/token/impact columns and chunk source-pointer columns. If no chunks are available, emit one metadata-only row with empty chunk/source-pointer cells. |

CSV chunk source-pointer columns are named `chunk_source_id`, `chunk_source_path`, `source_record_key`, `source_line_number`, `source_message_id`, and `source_part_id`. They are raw audit pointers back into native storage, not stable UI anchors: JSONL providers generally populate path plus line number, Cursor-family providers populate DB path plus KV row keys, and DB-backed providers populate DB path plus native message/part IDs where available.

Future formats should be added as formatting adapters over the same source metadata and chunk DTOs, not as provider-specific CLI paths.

## Cursor IDE

### Native query and export plan

| Column | Details |
| --- | --- |
| Source ID | `cursor_ide` |
| Native storage | Cursor `state.vscdb` under Cursor global storage. macOS: `~/Library/Application Support/Cursor/User/globalStorage/state.vscdb`; Linux: `~/.config/Cursor/User/globalStorage/state.vscdb`; Windows: `~/AppData/Roaming/Cursor/User/globalStorage/state.vscdb`. |
| Native query method | Open SQLite read-only. Query metadata with `SELECT key, value FROM cursorDiskKV WHERE key LIKE 'composerData:%'`. Query one composer with `composerData:{composerId}`. Query bubbles with `bubbleId:{composerId}:{bubbleId}` keys, usually batched by `IN (...)`. Resolve content blobs from `composer.content.{hash}` keys when referenced. |
| Native raw format | `cursorDiskKV.value` JSON strings. Composer rows contain composer metadata and ordered bubble headers. Bubble rows contain user/assistant/tool data. `toolFormerData.params` and `toolFormerData.result` are JSON-encoded strings. |
| ORGII implementation | `orgtrack-core/src/sources/cursor_ide/db.rs`, `history.rs`, `io.rs`, `helpers.rs`, `models.rs`, `summaries.rs`. Tauri bridge: `src-tauri/src/orgtrack/history_commands.rs`. Frontend wrapper: `src/api/tauri/cursorIde/index.ts`. |
| Current ORGII metadata store | `imported_history_session_cache` for session metadata and `cursor_ide_turn_summaries` for derived turn summaries. |
| Brick target | Cursor source provider with metadata rows in `<BRICK_HOME>/metadata.sqlite`; lazy chunk provider reads Cursor DB on demand; turn summaries become a derived index keyed by source fingerprint. |
| JSON export | `history sessions` returns session rows; `history chunks` supports real full-session raw formatting from `composerData:{composerId}` and `bubbleId:{composerId}:{bubbleId}`. Cursor-specific chunk modes `initial-window`, `full-refresh`, and `turn-window` remain pending. |

### Cursor session metadata fields observed

| Native field | Meaning | Brick mapping / treatment |
| --- | --- | --- |
| `composerId` | Session/composer ID. | `sourceSessionId`, `external_session_id`, `sourceRecordKey`. |
| `name` | Session title. | `title` / `name`. |
| `createdAt` | Created time in epoch ms. | `createdAt`, `created_at`. |
| `lastUpdatedAt` | Last composer update time in epoch ms. | `updatedAt`, `last_seen_at`, source fingerprint input. |
| `conversationCheckpointLastUpdatedAt` | Checkpoint update time. | Store in `sourceMetadata`; may contribute to `updatedAt` after validation. |
| `unifiedMode` | Mode, for example `agent`. | `sourceMetadata.mode`; optionally first-class `mode`. |
| `forceMode` | Forced mode, for example `edit`. | `sourceMetadata.forceMode`. |
| `hasUnreadMessages` | Whether session has unread messages. | `sourceMetadata.hasUnreadMessages`. |
| `contextUsagePercent` | Context usage percentage. | `sourceMetadata.contextUsagePercent`; not equivalent to token count. |
| `contextTokensUsed` | Context token usage when available. | `inputTokens` or `sourceMetadata.contextTokensUsed`. |
| `totalLinesAdded` | Added lines attributed by Cursor. | `linesAdded`. |
| `totalLinesRemoved` | Removed lines attributed by Cursor. | `linesRemoved`. |
| `filesChangedCount` | Number of changed files. | `filesChanged`. |
| `subtitle` | Subtitle, often read/edited file summary. | `sourceMetadata.subtitle`; potential display subtitle. |
| `hasBlockingPendingActions` | Blocking pending action state. | `sourceMetadata.hasBlockingPendingActions`; may affect status. |
| `hasPendingPlan` | Whether pending plan exists. | `sourceMetadata.hasPendingPlan`. |
| `isArchived` | Archived flag. | `sourceMetadata.isArchived`; can exclude or expose via filters. |
| `isDraft` | Draft flag. | `sourceMetadata.isDraft`; can exclude or expose via filters. |
| `isWorktree` | Whether session uses a worktree. | `sourceMetadata.isWorktree`; may link to workspace roots. |
| `worktreeStartedReadOnly` | Whether worktree began read-only. | `sourceMetadata.worktreeStartedReadOnly`. |
| `isSpec` | Spec session flag. | `sourceMetadata.isSpec`. |
| `isProject` | Project session flag. | `sourceMetadata.isProject`. |
| `isBestOfNSubcomposer` | Best-of-N subcomposer flag. | `sourceMetadata.isBestOfNSubcomposer`; likely listability filter. |
| `numSubComposers` | Number of subcomposers. | `sourceMetadata.numSubComposers`. |
| `referencedPlans` | Referenced plan IDs/data. | `sourceMetadata.referencedPlans`; optional linked planning artifacts later. |
| `trackedGitRepos` | Associated repo paths, branches, and repo metadata. | `repoPath`, `branch`, `workspace_roots`, `git_repositories`, plus `sourceMetadata.trackedGitRepos`. |
| `workspaceIdentifier` | Workspace URI/path metadata. | Fallback `workspacePath` / `repoPath`. |
| `fullConversationHeadersOnly` | Canonical bubble order. | Lazy chunk order source; store header count and fingerprint only by default. |
| `subagentInfo` | Subagent composer metadata. | Listability filter; preserve in `sourceMetadata` when included. |
| `modelConfig.modelName` | Cursor model. | `model`. |

### Cursor plan/session relationship recovery

Cursor plan-to-session relationships are recoverable through `composer.planRegistry` plus `composer.composerHeaders.allComposers`. This path is important because it does not primarily depend on the large composer data table that can be damaged or partially unreadable.

| Trace path | Fields | Brick mapping / treatment |
| --- | --- | --- |
| `composer.planRegistry.{planId}` | Plan registry entry keyed by `planId`. | Create or update a Brick planning artifact keyed by Cursor plan ID. |
| `composer.planRegistry.{planId}.uri.fsPath` | Plan file path, for example `/Users/laptop-h/.cursor/plans/<plan>.plan.md`. | Evidence pointer for the plan Markdown file; optional content-addressed copy when explicitly imported. |
| `composer.planRegistry.{planId}.createdBy` | Session/composer ID that created the plan. | Edge: `session created plan`. |
| `composer.planRegistry.{planId}.editedBy[]` | Session/composer IDs that edited the plan. | Edges: `session edited plan`. |
| `composer.planRegistry.{planId}.referencedBy[]` | Session/composer IDs that referenced the plan. | Edges: `session referenced plan`. |
| `composer.planRegistry.{planId}.builtBy{sessionId: todoIds[]}` | Sessions that executed specific plan todo IDs. | Edges: `session built plan todo`; preserve todo IDs in `sourceMetadata` and later promote to task/proof links. |
| `composer.composerHeaders.allComposers.{sessionId}.name` | Session name/title. | Session display `name` / `title`. |
| `composer.composerHeaders.allComposers.{sessionId}.createdAt` | Session created time. | `createdAt`. |
| `composer.composerHeaders.allComposers.{sessionId}.lastUpdatedAt` | Session updated time. | `updatedAt`, `last_seen_at`. |
| `composer.composerHeaders.allComposers.{sessionId}.workspaceIdentifier` | Workspace identifier. | `workspacePath` / `repoPath` fallback. |
| `composer.composerHeaders.allComposers.{sessionId}.trackedGitRepos` | Tracked repos and branches. | `repoPath`, `branch`, repo context metadata. |
| `composer.composerHeaders.allComposers.{sessionId}.subtitle` | Session subtitle. | `sourceMetadata.subtitle`; optional display subtitle. |
| `composer.composerHeaders.allComposers.{sessionId}.mode` | Session mode. | `sourceMetadata.mode`; optionally first-class Cursor mode. |
| `composer.composerHeaders.allComposers.{sessionId}.isArchived` | Archive flag. | Listability/filter field. |

Recovery flow:

1. Read `composer.planRegistry` from Cursor storage.
2. For each `planId`, collect all related session IDs from `createdBy`, `editedBy[]`, `referencedBy[]`, and `builtBy` keys.
3. Resolve those session IDs through `composer.composerHeaders.allComposers` to recover session metadata such as `name`, `createdAt`, `lastUpdatedAt`, `workspaceIdentifier`, `trackedGitRepos`, `subtitle`, `mode`, and `isArchived`.
4. Persist plan/session edges into Brick source metadata or a dedicated planning-edge index.
5. Use the larger composer/bubble rows only for full transcript-to-chunk JSON formatting, not as the only source for plan relationship recovery.

First slice implemented in Brick:

- `metadata.sqlite` schema version `2` adds `source_plans` and `source_plan_session_edges`.
- Cursor plan registry refresh accepts both compact `composer.planRegistry` objects and per-plan `composer.planRegistry.{planId}` keys.
- Plan-session edge roles are typed as `created_by`, `edited_by`, `referenced_by`, and `built_by`; `built_by` preserves Cursor todo IDs in `todo_ids_json`.
- Edges store native `external_session_id` directly and intentionally do not require a matching `source_sessions` row, so relationships survive missing or partially unreadable session headers.

### Cursor chunk formatting

| Step | Details |
| --- | --- |
| Composer lookup | Load `composerData:{composerId}` and parse composer metadata. |
| Bubble order | Use `fullConversationHeadersOnly` as canonical order. Do not sort by timestamp. |
| Bubble lookup | Load `bubbleId:{composerId}:{bubbleId}` values from `cursorDiskKV`. |
| User chunks | Bubble type `1` becomes `actionType = raw`, `function = user_message`. |
| Assistant chunks | Bubble type `2` text becomes `actionType = assistant`, `function = assistant`. |
| Tool chunks | Assistant bubbles with `toolFormerData` become `actionType = tool_call`; tool args/results are parsed from JSON strings and canonicalized. |
| Content blobs | During raw chunk formatting, string references to `composer.content.{hash}` and embedded hash-like content IDs in content/key/hash fields are resolved from `cursorDiskKV`. Resolved blob values are parsed as JSON when possible, then reduced to text for message content/tool output or parsed as JSON for tool args. This remains an audit formatter, not a Cursor UI replay/reconstruction layer. |
| Windowed export | Cursor still needs initial-window, full-refresh, and turn-window APIs because ORGII UI does not always load all bubbles at once. |

## Claude Code

### Native query and export plan

| Column | Details |
| --- | --- |
| Source ID | `claude_code` |
| Native storage | JSONL transcripts under Claude projects roots, especially `~/.claude/projects/**/*.jsonl`. ORGII also checks platform-specific Claude Code application support paths. |
| Native query method | Recursive filesystem scan for `*.jsonl`; use file stem as source session ID; use file mtime and size as record signature; parse changed files line-by-line. |
| Native raw format | JSONL. Lines include `type`, `timestamp`, `cwd`, `gitBranch`, and optional `message`. Message includes `model`, `content`, and `usage`. |
| ORGII implementation | `orgtrack-core/src/sources/claude_code/history.rs`. Older stats scanner: `orgtrack-core/src/sources/claude_code/db.rs`. |
| Current ORGII metadata store | `imported_history_session_cache`; older stats path also writes `claude_session_cache`. |
| Brick target | One Claude provider that combines modern JSONL-to-JSON formatting with optional `sessions-index.json` scan optimization. |
| JSON export | Sessions, recent paths, full chunks. Chunk source pointers include JSONL path and line number where available. |

### Claude metadata fields

| Native field/source | Meaning | Brick mapping / treatment |
| --- | --- | --- |
| File stem | Native session ID. | `sourceSessionId`, `sourceRecordKey`. |
| File path | JSONL path. | `sourcePath`, evidence pointer. |
| File mtime/size | Source signature. | `sourceMtimeMs`, `sourceSizeBytes`, `sourceFingerprint`. |
| `timestamp` | Event time. | min -> `createdAt`; max -> `updatedAt`. |
| `cwd` | Working directory. | `repoPath` / `workspacePath`. |
| `gitBranch` | Git branch. | `branch`. |
| `message.model` | Model name. | `model`. |
| `message.usage.input_tokens` | Input tokens. | `inputTokens`. |
| `message.usage.cache_read_input_tokens` | Cache read tokens. | add to `inputTokens`. |
| `message.usage.cache_creation_input_tokens` | Cache creation tokens. | add to `inputTokens`. |
| `message.usage.output_tokens` | Output tokens. | `outputTokens`. |
| First user message content | Human title. | `name` / `title`, truncated. |
| Assistant `tool_use` items | Tool actions. | impact stats and lazy chunks. |
| Tool names `Edit`, `MultiEdit`, `Write` | File edits. | `touchedFiles`, `filesChanged`, `linesAdded`, `linesRemoved`. |

### Claude chunk formatting

| Step | Details |
| --- | --- |
| Path resolution | Look up source path from metadata index; fallback to rescanning source roots by file stem. |
| Parse | Read JSONL line-by-line. |
| User chunks | User messages become `raw/user_message`. |
| Assistant chunks | Assistant text becomes `assistant/assistant`; thinking content becomes `thinking/thinking`. |
| Tool chunks | Tool use/result pairs become `tool_call`; edit/write tool args are normalized. |
| Export additions | Include `sourceLineNumber`, `sourceTimestamp`, `sourceType`, `toolUseId` when available. |

## Codex App

### Native query and export plan

| Column | Details |
| --- | --- |
| Source ID | `codex_app` |
| Native storage | Codex JSONL session files under `~/.codex/sessions/**/**/*.jsonl` and platform application support variants. |
| Native query method | Recursive filesystem scan for `.jsonl`; file stem is source session ID; mtime/size are source signature; parse changed JSONL files. |
| Native raw format | JSONL lines with top-level `timestamp` and `payload`. Payload types include `user_message`, `agent_message`, `message`, `reasoning`, `agent_reasoning`, `function_call`, `custom_tool_call`, `function_call_output`, `custom_tool_call_output`, and `token_count`. |
| ORGII implementation | `orgtrack-core/src/sources/codex/app.rs`. Older generic CLI scanner also scans Codex in `orgtrack-core/src/sources/cli_session_db.rs`. |
| Current ORGII metadata store | `imported_history_session_cache`; older stats path writes `cli_session_cache`. |
| Brick target | Canonical Codex provider should use the modern `codex_app` parser; deprecate duplicate generic stats path after parity. |
| JSON export | Sessions, recent paths, full chunks with JSONL path and line pointers. |

### Codex metadata fields

| Native field/source | Meaning | Brick mapping / treatment |
| --- | --- | --- |
| File stem | Native session ID. | `sourceSessionId`, `sourceRecordKey`. |
| File path | JSONL path. | `sourcePath`, evidence pointer. |
| File mtime/size | Source signature. | `sourceMtimeMs`, `sourceSizeBytes`, `sourceFingerprint`. |
| Outer `timestamp` | Event timestamp. | min -> `createdAt`; max -> `updatedAt`. |
| First `user_message.message` | Human title. | `name` / `title`, truncated. |
| Turn context `cwd` | Workspace. | `repoPath` / `workspacePath`. |
| Turn context `model` | Model. | `model`. |
| `token_count.total_token_usage.input_tokens` | Input tokens. | `inputTokens`. |
| `token_count.total_token_usage.output_tokens` | Output tokens. | `outputTokens`. |
| `apply_patch` payloads | Patch impact. | `touchedFiles`, `filesChanged`, `linesAdded`, `linesRemoved`. |

### Codex chunk formatting

| Step | Details |
| --- | --- |
| Path resolution | Metadata index source path first; fallback scan by file stem. |
| User messages | `payload.type = user_message` becomes `raw/user_message`. |
| Assistant messages | `agent_message` or assistant `message` becomes `assistant/assistant`. |
| Reasoning | `reasoning` or `agent_reasoning` becomes `thinking/thinking`. |
| Tool calls | `function_call` / `custom_tool_call` are paired with output rows by `call_id`. |
| Canonical tools | `shell` -> `run_command_line`; `apply_patch` -> `edit_file_by_replace`. |

## OpenCode

### Native query and export plan

| Column | Details |
| --- | --- |
| Source ID | `opencode` |
| Native storage | OpenCode SQLite DB `opencode.db`. Candidate roots include fallback roots (`~/.opencode/opencode.db`, `~/.local/share/opencode/opencode.db`), macOS application support paths, Windows roaming/local paths, and Linux config/data paths. Discovered defaults write the DB file to `session_db_path`. |
| Native query method | Open DB read-only from `session_db_path` or configured path containing `opencode.db`. Query `session` for metadata with conservative schema introspection. Query `part` joined to `message` for chunks when `message_id` plus a session ID column are available. |
| Native raw format | SQLite tables `session`, `message`, and `part`; first slice assumes `session.id` and optionally reads `session.title`, `session.directory`, `session.model`, `time_created`, `time_updated`, archive flags, and token columns. JSON is expected in `message.data`, `part.data`, and sometimes `session.model`, but provider falls back to plain strings where feasible. |
| ORGII implementation | `orgtrack-core/src/sources/opencode/history.rs`. |
| Current ORGII metadata store | `imported_history_session_cache`. |
| Brick target | DB-backed OpenCode provider with per-session metadata rows and lazy DB chunk loading. |
| JSON export | Sessions, recent paths, and real full chunks. Chunk source pointers include DB path plus message and part IDs when exposed by the schema. |

### OpenCode metadata fields

| Native field/source | Meaning | Brick mapping / treatment |
| --- | --- | --- |
| `session.id` | Native session ID. | `sourceSessionId`, `sourceRecordKey`. |
| DB path | Native source DB. | `sourcePath`. |
| DB mtime/size | Source signature. | `sourceMtimeMs`, `sourceSizeBytes`; stronger per-session fingerprint later. |
| `session.title` | Title. | `name` / `title`, truncated. |
| `session.directory` | Workspace/repo path. | `repoPath` / `workspacePath`. |
| `session.model` | Raw or JSON model descriptor. | parse `id`, then `modelId`, then `providerId`, else raw string. |
| `tokens_input` | Input tokens. | part of `inputTokens`. |
| `tokens_cache_read` | Cache read tokens. | add to `inputTokens`. |
| `tokens_cache_write` | Cache write tokens. | add to `inputTokens`. |
| `tokens_output` | Output tokens. | part of `outputTokens`. |
| `tokens_reasoning` | Reasoning tokens. | add to `outputTokens`. |
| `time_created` | Created time. | `createdAt`. |
| `time_updated` | Updated time. | `updatedAt`; fallback to `time_created`. |
| `time_archived` | Archive marker. | Filter archived sessions by default when present; also recognizes boolean-ish `archived`, `is_archived`, and `isArchived` columns. |

### OpenCode chunk formatting

| Step | Details |
| --- | --- |
| Session ID | ORGII prefix is `opencodeapp-{session.id}`; Brick can preserve or expose native ID with source. |
| Native query | Fixture-supported first slice introspects columns and effectively queries `part p JOIN message m ON m.id = p.message_id`, filtering on `p.session_id` or `m.session_id`, ordering by `p.time_created` when present and `p.id` as a tie-breaker. |
| Text chunks | `part.type = text` and role `user` -> `raw/user_message`; otherwise -> `assistant/assistant`. |
| Reasoning chunks | `part.type = reasoning` -> `thinking/thinking`. |
| Tool chunks | `part.type = tool` -> `tool_call`. |
| Canonical tools | `bash`, `shell`, `execute` -> `run_command_line`; `write`, `edit`, `patch`, `apply_patch` -> `edit_file_by_replace`. |
| Source pointers | Include `sourcePartId`, `sourceMessageId`, `timeCreatedMs`, `timeStartMs`, `timeEndMs` in chunk metadata. |

## Windsurf

### Native query and export plan

| Column | Details |
| --- | --- |
| Source ID | `windsurf` |
| Native storage | Windsurf `state.vscdb`. Candidate roots include macOS `~/Library/Application Support/Windsurf/User/globalStorage/state.vscdb`, Linux config/data paths, Windows roaming/local paths, and fallback `~/.windsurf/User/globalStorage/state.vscdb`. Discovered defaults write the DB file to `cursor_state_db_path`. |
| Native query method | Open SQLite read-only. Query metadata with `SELECT value FROM cursorDiskKV WHERE key LIKE 'composerData:%'`. Load one composer with `composerData:{composerId}`. Load bubbles with batched `bubbleId:{composerId}:{bubbleId}` keys. |
| Native raw format | Cursor-like `cursorDiskKV` JSON strings. Composer rows include `composerId`, `name`, timestamps, model config, headers, tracked repos, workspace identifier, subagent info. Bubble rows include `type`, `bubbleId`, `createdAt`, `text`, and optional `toolFormerData`. |
| ORGII implementation | `orgtrack-core/src/sources/windsurf/history.rs`. |
| Current ORGII metadata store | `imported_history_session_cache`. |
| Brick target | Cursor-family DB provider variant; share key grammar with Cursor where possible, with Windsurf-specific path discovery and tool mapping. |
| JSON export | Sessions and full chunks. Brick first slice reads `composerData:%` for metadata and shares the Cursor-family composer/bubble formatter for raw chunk JSON. Initial/turn windows can be added if UI needs parity with Cursor. |

### Windsurf metadata fields

| Native field/source | Meaning | Brick mapping / treatment |
| --- | --- | --- |
| `composerId` | Native session/composer ID. | `sourceSessionId`, `sourceRecordKey`. |
| DB path | Native source DB. | `sourcePath`. |
| DB mtime/size | Source signature. | `sourceMtimeMs`, `sourceSizeBytes`. |
| `name` | Session title. | `name` / `title`, truncated. |
| `createdAt` | Created time. | `createdAt`. |
| `lastUpdatedAt` | Updated time. | `updatedAt`; fallback to `createdAt`. |
| `status` | Native status. | `sourceMetadata.status`; imported row status can remain `completed`. |
| `modelConfig.modelName` | Model. | `model`. |
| `contextTokensUsed` | Context tokens. | `inputTokens`; preserve exact value in `sourceMetadata`. |
| `trackedGitRepos[0].repoPath` | Repo path. | `repoPath`. |
| `trackedGitRepos[0].branches[0].branchName` | Branch. | `branch`. |
| `workspaceIdentifier.uri.fsPath` | Workspace fallback. | fallback `repoPath` / `workspacePath`. |
| `workspaceIdentifier.uri.path` | Workspace fallback. | fallback `repoPath` / `workspacePath`. |
| `subagentInfo` | Subagent marker. | listability filter and `sourceMetadata`. |
| `fullConversationHeadersOnly` | Bubble order. | lazy chunk order source; store count/fingerprint only by default. |

### Windsurf chunk formatting

| Step | Details |
| --- | --- |
| Composer lookup | Load `composerData:{composerId}` from `cursorDiskKV`. |
| Bubble lookup | Build `bubbleId:{composerId}:{bubbleId}` keys from composer headers and query in chunks. |
| User chunks | Bubble type `1` -> `raw/user_message`. |
| Assistant chunks | Bubble type `2` text -> `assistant/assistant`. |
| Tool chunks | Bubble type `2` with `toolFormerData` -> `tool_call`. |
| Canonical tools | `shell`, `run_command`, `terminal`, `terminal_command` -> `run_command_line`; `edit_file`, `edit_file_v2`, `write_file`, `apply_patch` -> `edit_file_by_replace`. |
| Content IDs | Tool results may reference content IDs; provider should resolve them from native DB only during lazy chunk load. |

## ORGII runtime sessions

| Column | Details |
| --- | --- |
| Source IDs | ORGII-owned runtime sources such as `orgii_rust_agents` and `orgii_cli_sessions`. |
| Native storage | ORGII session persistence and app/runtime DBs. |
| Query method | ORGII internal APIs and session persistence, not external native app scraping. |
| Brick treatment | Do not make Brick scrape ORGII runtime state by default. ORGII should explicitly emit Brick provenance events or export evidence when needed. |
| JSON export | Brick can ingest ORGII-origin events, but ORGII remains runtime owner. |

## Incremental refresh model (shipped)

Brick auto-refreshes the anchor repo's sources on every `explain` call, but the refresh is both throttled and incremental, so it stays near-real-time without a manual CLI refresh.

- **Watermark table.** `source_index_watermark(source_id, last_indexed_updated_at, last_refreshed_at)` lives in `metadata.sqlite` (schema version 6) and tracks each source's high-water mark.
- **Incremental entry point.** `list_source_sessions_since(profile, limit, since)` is the incremental scan path; `since` is the source's last indexed updated-at.
- **Three filter strategies:**
  - File-mtime gate (JSONL/JSON file providers — `claude_code`, `codex_app`, `gemini`): the native file walker (`native_source.rs`) skips parsing any session file whose mtime is at or below the watermark. This is what eliminates the big-history full re-scan cost (codex went from 1413 parsed session files to ~1 on incremental runs).
  - SQL / column push-down (ORGII and OpenCode): `updated_at >= since` filters at the query / parsed-column level (`time_updated` for OpenCode).
  - KV-blob post-filter (cursor-family — `cursor_ide`, `windsurf`): `filter_sessions_since` shrinks the downstream upsert set after parsing, because the session's updated time lives inside a SQLite KV JSON blob that must be parsed anyway.
- **Persistent cross-process throttle.** Auto-refresh is keyed on `source_index_watermark.last_refreshed_at` with a 10s window, so back-to-back `explain` calls across separate CLI processes stay cheap. (The earlier in-process `OnceLock` throttle never fired for the CLI, where each invocation is a fresh process.)
- **Measured effect.** A throttle-expired `explain` over a multi-GB ORGII plus ~1400-file codex history dropped from ~30s (full re-scan of every source) to ~0.5s once the watermark is populated.

## Provider fixture validation

Provider parity tests use sanitized fixture scenarios under `crates/core/tests/fixtures/external_sources`. The fixture tree is intentionally text-first so Brick can validate real-ish source schemas without committing private native history or large binary databases.

Convention:

```text
crates/core/tests/fixtures/external_sources/
  <source_id>/
    <scenario_name>/
      manifest.json
      logs/                  # JSONL/text fixtures
      db-spec/               # SQL/JSON specs for generated SQLite DBs
```

Each `manifest.json` declares the source, fixture format, relative profile path hints, and expected session/chunk fields. JSONL-backed providers can commit tiny sanitized transcripts directly. SQLite-backed providers should commit SQL or JSON specs and let tests generate temporary DBs, rather than committing `state.vscdb`, `opencode.db`, WAL, or SHM files.

Sanitization rules before adding fixtures:

1. Keep only the smallest records needed for the parser behavior under test.
2. Replace private prompts, file contents, command output, paths, repo names, users, hosts, URLs, tokens, and organization names with deterministic placeholders.
3. Normalize timestamps and IDs to stable synthetic values.
4. Use paths such as `/workspace/repo`, branches such as `feature/example`, and model names such as `example-model` unless the exact value is required to test parsing.
5. Review generated expected metadata/chunks before committing.

The current committed example is `claude_code/basic_session`, a tiny JSONL transcript that exercises metadata extraction and chunk formatting. Next fixtures should add one generated SQLite scenario each for Cursor IDE, Windsurf, and OpenCode, plus a tiny Codex App JSONL scenario that covers `token_count` and `apply_patch` impact parsing.

## Implementation status

| Source | Brick today | ORGII today | Target |
| --- | --- | --- | --- |
| Cursor IDE | Metadata provider reads `composer.composerHeaders.allComposers`; real chunk formatter reads composer/bubble KV rows and dereferences `composer.content.{hash}`-style blobs; plan registry refresh persists `source_plans` and typed plan-session edges; incremental refresh wired via KV-blob post-filter (`filter_sessions_since`). Cursor windowing modes (initial-window/full-refresh/turn-window) remain pending. | Mature metadata scan, DB parsing, lazy chunks, window APIs, turn summaries. | Add Cursor window modes and richer plan/task export next. |
| Claude Code | Full JSONL metadata extraction, impact stats (`touched_files`), real lazy chunk loading (`format_chunks`), and incremental refresh via a file-mtime watermark gate. | Mature JSONL metadata scan, impact stats, lazy chunks. | At parity; harden fixtures and edge cases. |
| Codex App | Full JSONL parsing, impact stats, real lazy chunks (`format_chunks`), and mtime-gated incremental refresh. This was the big-history case: incremental runs dropped from ~1413 parsed session files to ~1 once the watermark is populated. | Mature JSONL metadata scan, impact stats, lazy chunks. | At parity; harden fixtures and edge cases. |
| OpenCode | DB metadata provider registered as `opencode`; reads `session` metadata, aggregates token columns from `session` or `part`, filters archived sessions, provides real lazy chunks from `message` + `part`, and supports incremental refresh via a parsed `time_updated` column filter. | DB metadata scan and lazy chunks. | Add source pointer metadata and validate against more real-world schema variants. |
| Windsurf | Reads `composerData:%` rows from `cursorDiskKV`, extracts core metadata/context tokens/impact fields, shares Cursor-family real chunk formatting for composer/bubble rows (including shared content-blob resolution), and supports incremental refresh via the KV-blob post-filter (`filter_sessions_since`). | Cursor-family DB metadata scan and lazy chunks. | Validate content ID/blob patterns against real Windsurf fixtures and add native path discovery defaults next. |
| Gemini | `format_chunks` implemented: renders user/gemini messages plus `toolCalls` from the chat JSON; mtime-gated incremental refresh shared with the other native file walkers. | n/a | Document full field reference alongside the other providers. |
| ORGII source | Full ORGII provider reads `agent_sessions` + `events`; real chunks strip the literal `assistant ` prefix from assistant content; incremental refresh uses SQL `updated_at >= since` push-down. | ORGII owns runtime. | Keep in sync with ORGII session/event schema. |
| ORGII runtime | Not a native external scrape target. | ORGII owns runtime. | ORGII emits/exports to Brick explicitly. |

## Brick treatment rules

- Metadata rows should include source identity, external session ID, source path/URI, mtime, size, fingerprint when available, parser version, discovery time, last-seen time, and provider-specific metadata.
- Source providers should expose a metadata refresh operation separate from chunk loading.
- Lazy chunk providers should read native source storage on demand and return normalized DTOs.
- Source providers should never silently swallow parser errors for an explicitly requested source/session.
- Full transcript bytes should remain in native storage by default.
- Optional evidence copy should go through content-addressed blobs and provenance events, not through the source metadata index.
- Source profile and scan state should move into `metadata.sqlite` over time, but repo-local TOML profiles can remain as bootstrap/config during migration. Incremental-refresh watermark/scan state already lives in `metadata.sqlite` via the `source_index_watermark` table (see "Incremental refresh model").
