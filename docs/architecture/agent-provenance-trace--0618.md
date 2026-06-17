# ORGII Trace: Mission Provenance Architecture

## Goal

ORGII Trace records the execution trace behind software changes produced by humans and AI agents. It is intentionally not a line-level blame system in the first version. Git remains the source of truth for code; ORGII Trace becomes the source of truth for mission, session, artifact, review, and acceptance provenance.

## Naming

- Product: ORGII Trace
- Primary CLI binary: `orgii-trace`
- Short aliases: `otrace`, `ot`
- Optional wrapper: `orgii trace`
- Server binary: `orgii-trace-server`

## Core entities

### Mission

A Mission is the accountability container for an objective, feature, bug, investigation, or review. It replaces the less precise term "work item".

### Session

A Session is an execution container for a human, agent, CLI run, IDE chat, or importer-observed trace. Missions and Sessions are many-to-many.

### Artifact

An Artifact is a reviewable output of a session. Examples include patches, key decisions, content updates, test results, reviews, and acceptance records.

### Repo context

Repo context captures the Git state at the time an event is recorded:

- repository identity
- branch
- upstream branch
- branch base commit
- merge base commit
- session start HEAD commit
- current HEAD commit
- context mode, such as `created_worktree` or `attached_current_branch`

### External refs

External refs link Missions, Sessions, Artifacts, and repo contexts to GitHub, Jira, Linear, PRs, CI checks, and other external systems.

## Relationships

- Mission ↔ Session: many-to-many through `mission_session_links`
- Artifact ↔ Mission: many-to-many through `artifact_mission_links`
- Artifact ↔ Artifact: related through `artifact_relations`, such as `supersedes`, `reviews`, or `accepts`
- Artifact ↔ File: through `artifact_file_refs` for file-level lookup without line-level blame

## Confidence model

Every recorded event carries provenance confidence:

- `explicit`: recorded directly by CLI/API
- `observed`: captured from filesystem/process observation
- `imported`: parsed from another tool's trace
- `inferred`: derived from heuristics
- `unknown`: provenance source is not known

## Local storage

Local writes are event-sourced. The durable path is a Git-like filesystem log:

```text
.orgii/provenance/
  queue/
    YYYY-MM-DD.jsonl
  events/
  cache/
```

The JSONL event log is the durable source for local pending writes. SQLite can be added later as a derived cache/index for fast lookup.

## Sync model

The server is the source of truth for shared provenance. Sync has three flows:

- `push`: send locally queued events to the configured remote
- `pull`: fetch accepted remote events by repo cursor
- `sync`: pull, then push, then pull again to converge cursors

Events are idempotent by `event_id`. The server validates repo membership and permissions before accepting events.

## Authorization

Authorization is repo-scoped and self-host first. Initial roles:

- `viewer`: pull provenance
- `contributor`: push own explicit events
- `agent_writer`: push events for registered agents
- `maintainer`: manage repo settings and agent registrations
- `admin`: manage server-level configuration

## Self-host first

Teams should be able to host `orgii-trace-server` themselves. ORGII Cloud can be added later as a compatible hosted remote, but the protocol should not require it.

## Implementation phases

1. Rust workspace scaffold
2. Protocol crate with event schemas
3. Core crate with local event log and repo discovery
4. CLI commands for init, mission, session, artifact, push, pull, and sync
5. Server health endpoint and persistence skeleton
6. Importers for Cursor, Claude Code, Codex, and other agent traces
