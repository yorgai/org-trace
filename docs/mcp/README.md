# MCP capability kit

`brick mcp-serve` runs Brick as a [Model Context Protocol](https://modelcontextprotocol.io)
server over stdio. `brick agent install` registers it automatically.

This surface is fully open-source and independent of the proprietary sync layer:
nothing here requires `--features sync` or a running `brick-server`. Local
`explain` is free — the membership wall is on cross-machine sync.

## Transport

MCP is JSON-RPC 2.0, one message per line over stdin/stdout. The server
implements `initialize`, `tools/list`, `tools/call`, `ping`, and the
`notifications/initialized` ack. Every tool result is returned as a single MCP
text content block whose body is a JSON string — clients parse that JSON for the
structured payload documented below.

All tool logic reuses the same `brick-core` primitives the CLI uses
(`explain_from_events`, `TraceEvent::*` constructors, `store.append_event`), so
MCP and CLI never drift.

## Two surfaces

`brick mcp-serve` exposes a deliberately tiny surface, because every extra tool
dilutes the model's attention and eats context. Anything an agent will not
reliably reach for on its own is pushed via hooks or kept off the agent surface.

| Surface | How to start | Tools |
| --- | --- | --- |
| **Main (coding agent)** | `brick mcp-serve` | `explain`, `link` |
| **Planning agent** | `brick mcp-serve --planning` | `mission`, `mission_list`, `show_mission`, `artifact_add`, `artifact_attach` |

The planning surface is for a *dedicated planning custom agent* (a Claude
subagent, a Codex/Cursor mode, or an ORGII custom agent). When a user asks to
plan, the main agent spawns the planning agent; the coding agent's own tool list
stays at two.

## Main surface

### `explain`

Your single entry point into existing code. When you locate a file or code you
are about to change or reason about, call `explain` **before** drawing
conclusions from the code alone — prefer it over `grep` and `git log`, which are
a fallback used only when Brick has no record.

Input:

```json
{ "anchor": "crates/core/src/auth.rs:42", "depth": 3 }
```

`anchor` is a `path:line` (resolved through line-level blame, drift-aware), an
`artifact_*` id, a `mission_*` id, or an event id. The path may be repo-relative
or **absolute**; an absolute path is the most robust because it lets the server
locate the repo regardless of its own working directory (see
[Working directory](#working-directory-and-anchors)). The tool schema steers
agents toward absolute anchors for exactly this reason, so the default agent
behavior succeeds even when the client spawned the server with `cwd=/`. `depth`
is the causal hops to walk back (default 3, max 8).

Output (abridged):

```json
{
  "anchor": { "kind": "file_line", "input": "…:42",
              "resolved_events": ["<event-id>"], "blame_confidence": "commit" },
  "causal_chain": [
    { "event_id": "…", "event_type": "diff.captured",
      "what": "changed auth.rs", "actor_id": "claude",
      "session_id": "session_…", "mission_id": "mission_…",
      "occurred_at": "…", "relation": null,
      "note": "token refresh had a concurrency race; serialized it",
      "confidence": "observed", "depth": 0,
      "transcript": { "source": null, "session_id": "session_…" } }
  ],
  "forward": [
    { "event_id": "…", "what": "added test_auth.rs",
      "relation_to_anchor": "derived_from" }
  ],
  "truncated": false,
  "live": { "…": "another session is editing this file" }
}
```

- `causal_chain` walks **backward** from the anchor (newest first). Each step
  carries WHO (`actor_id` / `session_id` / `mission_id`), WHY (`note` +
  `relation`), and a `confidence` of `explicit` > `observed` > `inferred`.
- `forward` is what was derived from / triggered by the anchor.
- `live` appears only when another running session is touching the anchored file
  (Tier 1) or working in the same project (Tier 2) — this replaces the old
  standalone `sessions` / `claims` tools. **Liveness is never stored** — a
  persisted "active" flag is wrong the instant it lands. It is recomputed on
  every scan from two signals, so an abandoned or crashed session cannot show
  "active" forever:
  1. **A 120s activity window** — a transcript untouched for longer is `Idle`
     without even being opened, so a session whose process died simply stops
     appearing once it goes quiet.
  2. **Turn boundaries** — within the window, a finished turn is still `Idle`
     (Codex `task_complete` after the last `task_started`; Claude an `assistant`
     record with `stop_reason` set). Only an open turn counts as live.
- When the anchor has no causal record, `resolved_events` is empty, `causal_chain`
  is empty (never guessed), and a `note` says so — fall back to git there.

`explain` subsumes line-level **blame** (WHO) into the WHY answer.

### `link`

Record WHY after a non-trivial change so the next agent can recover your
reasoning with `explain`. Two forms:

```json
{ "note": "token refresh had a concurrency race; serialized it" }
```

```json
{ "effect": "src/test_auth.rs:2", "cause": "src/auth.rs:2",
  "relation": "derived_from", "note": "covers the race fix" }
```

- `effect` is the change you just made (`path:line` or event id); omit it to bind
  to your most recent captured diff.
- `cause` is the anchor that prompted the change; omit it for a standalone
  rationale.
- `relation` is one of `triggered_by`, `derived_from`, `supersedes`,
  `responds_to`, `rationale` (defaults to `derived_from` with a cause, else
  `rationale`).
- **Invariant:** at least one of `cause` or `note` must be present.

Edges recorded via `link` carry `confidence: explicit`.

## Planning surface (`--planning`)

- `mission` — `action="create"` opens a tracked goal under a project;
  `action="update"` changes its title / description / status.
- `mission_list` — in-flight missions, newest first, optional status/project filter.
- `show_mission` — one mission's detail, with linked sessions and artifacts.
- `artifact_add` — record a deliverable (PR, design doc, decision, test result).
- `artifact_attach` — attach the files that back an artifact.

## Retired tools

The previous query/coordination tools are gone from the agent surface; their
capability is folded into `explain` (WHO + WHY + `live`) or moved to the planning
surface. A `tools/call` for any retired name returns an actionable migration hint
for one release rather than a bare error:

| Retired | Replacement |
| --- | --- |
| `log_file`, `recall_file`, `blame`, `blame_file`, `log_line`, `blame_history`, `search`, `explore_memory`, `search_sessions`, `show_session`, `read_session` | `explain` (WHO + WHY + transcript pointer) |
| `sessions`, `live_sessions`, `claim`, `announce_work`, `claims`, `list_announcements`, `status`, `current_context` | the `live` field of an `explain` response |
| `mission`, `mission_list`, `show_mission`, `artifact_add`, `artifact_attach` (and aliases `manage_mission`, `list_missions`, `record_artifact`, `attach_evidence`) | the planning surface (`brick mcp-serve --planning`) |

## Storage

Writes (`link`, and the planning tools) append `TraceEvent`s to the local JSONL
log under `.brick/provenance/`; reads project that log into the rebuildable
index. Deleting the derived index or the SQLite `causal_edges` table and
rebuilding from JSONL reproduces every causal edge exactly — JSONL is the source
of truth.

## Working directory and anchors

A Brick repo is located by walking up from a path to its git root. The server
resolves that path in one of two ways, in order:

1. **From the anchor**, when it is an absolute path (`/abs/workspace/src/x.rs`
   or `/abs/workspace/src/x.rs:42`). The repo is discovered from the anchor
   itself, so the server reads the right repo no matter where it was started.
2. **From the server's working directory**, when the anchor is repo-relative
   (`src/x.rs:42`).

This matters because **MCP clients routinely spawn the stdio server with
`cwd=/`** — the agent's workspace is *not* inherited as the server's working
directory. Two ways to make `explain`/`link` resolve the right repo:

- **Pass absolute path anchors** (recommended for agents): always works,
  independent of how the client launched the server. The `explain`/`link` tool
  schemas already steer agents to do this, so it is the default path — no client
  config needed.
- **Set the server's `cwd` to the workspace** in the MCP client config, e.g.

  ```json
  { "mcpServers": { "brick": {
      "command": "brick", "args": ["mcp-serve"],
      "cwd": "/abs/path/to/workspace"
  } } }
  ```

When a repo-relative anchor is used and the working directory is not inside a
git repo (the `cwd=/` case), `explain` does **not** fail — it returns an empty
chain with an actionable `note` telling the caller to pass an absolute anchor or
set the working directory, and the agent falls back to git there.

The **planning surface** (missions / artifacts) has no path anchor to resolve a
repo from, and its records are cross-repo by nature. So when the server is
started outside a git repo, the planning tools fall back to a store rooted at
`BRICK_HOME` (default `~/.brick`, overridable via the env var) instead of
failing — writes and reads land in that same store, so a mission created over
MCP lists straight back.

The **`live` field** reads source profiles from `<repo>/.brick/sources`. Because
the server is spawned with `cwd=/`, those profiles are resolved from the repo
the *anchor* points at — not the process cwd — so cross-session awareness fires
on the default absolute-anchor path. With a relative anchor and no repo from
cwd, there is nothing to resolve and `live` is simply absent (never a crash).

## Verifying

`crates/cli/tests/mcp_smoke.rs` spawns the real `brick mcp-serve` binary and
drives it over stdio: it asserts the main surface is exactly `explain` + `link`,
the planning surface exposes the five planning tools, retired names return a
migration hint, `explain` resolves a `path:line` through blame and walks the
causal chain (including across commit + line drift), `link` records both a
standalone rationale and a cross-event edge, `explain` surfaces a live session
on the anchor file while excluding a finished (Idle) session from the `live`
field (and still firing for a live session via an absolute anchor when the
server's cwd is unrelated), and — spawning the server with an unrelated working
directory — an absolute anchor still recovers the WHO/WHY while a relative
anchor degrades to the actionable note, and the planning surface still creates
and lists a mission via its `BRICK_HOME` fallback.
