# MCP capability kit

`brick mcp-serve` runs Brick as a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio, turning one install into a shared work surface that any
MCP-capable agent (Claude Code, Cursor, ORGII, …) can call. `brick agent install`
registers it automatically.

This surface is fully open-source and independent of the proprietary sync layer:
none of these tools require `--features sync` or a running `brick-server`.

## Transport

MCP is JSON-RPC 2.0, one message per line over stdin/stdout. The server
implements `initialize`, `tools/list`, `tools/call`, `ping`, and the
`notifications/initialized` ack. Every tool result is returned as a single MCP
text content block whose body is a JSON string — clients parse that JSON for the
structured payload documented below.

All tool logic reuses the same `brick-core` primitives the CLI uses
(`TraceEvent::*` constructors, `store.append_event`, index rebuilds), so MCP and
CLI never drift.

## The three capabilities

The thirteen tools group into three jobs an agent needs across a task:

| Capability | Tools | Question it answers |
| --- | --- | --- |
| **Memory** | `explore_memory`, `recall_file`, `search_sessions`, `read_session` | "What happened before?" |
| **Planning & work-item management** | `current_context`, `list_missions`, `show_mission`, `manage_mission`, `record_artifact`, `attach_evidence` | "What am I doing, and what did I produce?" |
| **Coordination & awareness** | `live_sessions`, `announce_work`, `list_announcements` | "Who else is working, and on what?" |

## Recommended flow

A natural end-to-end flow chains all three capabilities:

```text
current_context        # where am I (org / project / mission / counts)
  → list_missions      # what work is already in flight
  → manage_mission     # action="create": turn the request into a tracked goal
  → announce_work      # claim the files/area before editing
  → (do the work)
  → record_artifact    # log the deliverable, linked to the mission
  → attach_evidence    # point the artifact at the files that back it
  → manage_mission     # action="update", status="completed": close the loop
```

Memory tools (`recall_file`, `explore_memory`, …) are called opportunistically —
typically `recall_file` right before editing a file, and `explore_memory` /
`search_sessions` when you need prior context.

---

## Memory

Read-only queries over cross-tool session history on this machine. They never
write events.

### `explore_memory`

Answer an open question about past AI coding work. Searches cross-tool session
history and returns a synthesized summary of the most relevant prior sessions
(intent, tool, when, transcript pointer). Use it first when you want context but
have no specific file path.

- **Input**: `question` (string, required) — natural language, e.g. `"how did we fix the auth token race"`.
- **Output**: a synthesized summary plus the matched sessions, each with a transcript pointer for `read_session`.

### `recall_file`

Recall who previously changed a file and why, across every coding tool on this
machine. Call before editing a file.

- **Input**: `path` (string, required) — repo-relative or absolute file path.
- **Output**: a one-line summary plus per-session intent and change size. The payload may also include:
  - `live_broadcast` — a session running *right now* that touches this path.
  - `active_claims` — `announce_work` heads-up notes from other sessions covering this path (`{ count, message, claims[] }`).

### `search_sessions`

Free-text search over session metadata (title, intent, touched files, repo,
branch) to find past sessions by topic.

- **Input**: `query` (string, required) — keywords to match against session metadata.
- **Output**: matches newest-first, each with a transcript pointer. Result cap defaults to 10.

### `read_session`

Page through one session's full transcript chunks. Supports pagination and
per-field truncation so large tool outputs don't overflow context.

- **Input**:
  - `source` (string, required) — source id, e.g. `claude_code`, `codex_app`, `cursor_ide`, `orgii`.
  - `session_id` (string, required) — external session id from a search/recall hit.
  - `offset` (integer, default `0`) — chunk offset.
  - `limit` (integer, default `50`) — max chunks.
  - `max_field_bytes` (integer, default `2000`) — truncate string values over this many bytes; `0` disables truncation.
- **Output**: the requested transcript chunks for that session.

---

## Planning & work-item management

These turn a request into a tracked goal and record the proof of work against
it. The write tools append immutable `TraceEvent`s to the local store; the read
tools rebuild the derived index so they always reflect just-written events.

> **Identity for MCP writes.** MCP has no logged-in human, so the author of a
> write is the calling tool: the `actor_id`/`source` argument if supplied, else
> `"mcp"`. This mirrors how `announce_work` attributes claims.

### `current_context`

Report the active org, project, mission, and session Brick has on record. Call
at the start of a task to know what you're working on and where new work should
be filed.

- **Input**: none.
- **Output**:
  - `current` — the persisted current context (or `null`).
  - `current_mission` — the full mission object the context points at, if any.
  - `counts` — `{ orgs, projects, missions, sessions, artifacts }`.
  - `note` — a one-line pointer to `list_missions` / `manage_mission` / `record_artifact`.

### `list_missions`

List missions (work items / goals), newest activity first. Use it to see what's
in flight, find an existing mission to attach output to, or pick up unfinished
work.

- **Input**:
  - `status` (string, optional) — `planned | active | blocked | completed | archived`.
  - `project` (string, optional) — filter to one project id.
  - `limit` (integer, default `50`).
- **Output**: `{ count, missions[] }`, missions sorted by most recent activity.

### `show_mission`

Show one mission in detail: status, description, and the sessions and artifacts
linked to it.

- **Input**: `mission` (string, required) — the mission id.
- **Output**: the full mission object, including `artifact_ids`, `session_ids`, status, and timestamps. Errors if the id is unknown.

### `manage_mission`

Create or update a mission — Brick's planning primitive.

- **Input**:
  - `action` (string, required) — `"create"` or `"update"`.
  - **create** requires `project` (string) and `title` (string); accepts `description` (string) and `status` (default `planned`).
  - **update** requires `mission` (string); accepts any of `title`, `description`, `status`, `project` (at least one). `project` on update moves the mission to another project.
  - `status` enum: `planned | active | blocked | completed | archived`.
  - `session_id`, `source` (strings, optional) — author attribution.
- **Output**: `{ created: true, mission_id, note }` on create, or `{ updated: true, mission_id }` on update.

### `record_artifact`

Record a deliverable you produced (a PR, a design doc, a decision, a test
result) and link it to a mission. This closes the planning loop: a mission
states the goal, an artifact is the proof of work.

- **Input**:
  - `title` (string, required) — what the artifact is, e.g. `"PR #42: OAuth login"`.
  - `kind` (string, default `note`) — `decision | file_ref | patch | review | test_result | acceptance | note`.
  - `body` (string, optional) — details / link / summary.
  - `mission` (string, optional but recommended) — mission to link to, so the work item shows its outputs.
  - `session_id`, `source` (strings, optional) — author attribution.
- **Output**: `{ recorded: true, artifact_id, note }`.

### `attach_evidence`

Attach a file-path piece of evidence to an artifact — the concrete file(s) that
back a deliverable, forming an auditable trail. Call after `record_artifact`.

- **Input**:
  - `artifact` (string, required) — artifact id from `record_artifact`.
  - `path` (string, required) — file path the artifact represents or touched.
  - `session_id`, `source` (strings, optional) — author attribution.
- **Output**: `{ attached: true, artifact_id }`.

#### Why `record_artifact` + `attach_evidence`

Together they answer "what did this goal actually produce, and how do I prove
it?" Without them a mission only says it is `active`; with them, anyone (or
another agent) calling `show_mission` sees a full chain:

```text
mission "Add OAuth login" (completed)
  └── artifact "PR #42: OAuth login" (patch)
        └── evidence src/auth/oauth.rs
```

That goal → deliverable → file trail is the core of Brick's provenance value.

---

## Coordination & awareness

These help concurrent sessions avoid stepping on each other.

### `live_sessions`

List AI coding sessions that appear to be running right now across every tool on
this machine. Liveness is recomputed per call from each source's own signals; it
is never persisted.

- **Input**: `scope` (string, optional) — path prefix; only sessions whose work scope is at or under this path are returned.
- **Output**: `{ count, sessions[], note }`. Each row includes the session's work scope and recently touched files.

### `announce_work`

Post a heads-up on the cross-session bulletin board *before* you start editing:
"I'm changing X, hold off." Other sessions calling `recall_file` on a matching
path see your note (as `active_claims`).

- **Input**:
  - `scope` (string, required) — file path or glob you are claiming, e.g. `crates/core/src/auth.rs` or `crates/cli/src/**/*.rs`. A bare filename like `auth.rs` matches that file anywhere.
  - `message` (string, required) — one line: what you're doing and any warning.
  - `session_id` (string, optional) — your session id, so others know who to coordinate with.
  - `source` (string, optional) — your tool/app id.
  - `ttl_minutes` (integer, default `240`) — minutes until the claim auto-expires.
- **Output**: `{ published: <claim>, note }`.

> **Lifecycle note.** A claim is retired on whichever comes first:
> 1. **Session ended** — when the publishing session can be matched to a native
>    source session that is no longer active, the claim is dropped (and deleted)
>    on the next `recall_file` / `list_announcements` read. This is the common
>    case: the moment an agent exits, its claims stop misleading others.
> 2. **TTL expiry** — claims whose publisher cannot be probed (a CLI claim, a
>    bare `mcp` publisher with no matching native session) fall back to the TTL
>    (default 4h), swept on the next read or write.
>
> Liveness only ever retires a claim we can positively confirm is dead; an
> unidentifiable publisher is always kept until its TTL, so the check never
> over-deletes. Scope matching is generous: exact path, glob (`*`, `**`, `?`,
> `[…]`), bare basename, relative/absolute path-suffix equivalence, and
> directory-prefix all match.

### `list_announcements`

List active bulletin-board claims (other sessions' "I'm working on X" notes).
Call before editing to check nobody has claimed the area you're about to touch.

- **Input**: `path` (string, optional) — only claims whose scope covers this path. Omit to list every active claim.
- **Output**: `{ count, announcements[] }`, newest-first.

---

## Storage

Planning writes land in the same append-only event log as the CLI
(`.brick/provenance/queue/*.jsonl` by default; see the storage-root resolution
order in [`../architecture/README.md`](../architecture/README.md)). Missions,
artifacts, and evidence are events, not mutable rows — `mission.created`,
`mission.updated`, `artifact.created`, `artifact.file_ref_recorded`, and so on.
See [`../protocol/README.md`](../protocol/README.md) for the event families.

Announcements are deliberately **not** in the rebuildable event log. They are
authored intent with a TTL, stored in their own additive-only
`<BRICK_HOME>/announcements.sqlite` so a schema bump to the metadata cache never
wipes them.
