---
status: active
---

# Brick Architecture

This document defines the current Brick storage and history architecture. The naming rule is intentional: `metadata.sqlite` is the **source metadata index**, not a second cache layer and not a transcript store.

## Naming glossary

| Term | Meaning | Durable? | Rebuildable? |
| --- | --- | --- | --- |
| Native source storage | The original storage owned by Cursor, Claude Code, Codex, OpenCode, ORGII, or another source app. | Yes, outside Brick | No |
| Source metadata index | Brick's normalized metadata rows for external sessions, roots, paths, fingerprints, parser versions, and links. Stored in `<BRICK_HOME>/metadata.sqlite`. | Yes as Brick local metadata | Yes from native sources where available |
| Provenance ledger | Brick's append-only accountability events. Stored as JSONL events. | Yes | No |
| Derived query views | `index.json`, `brick.sqlite`, and Markdown `views/` rebuilt from the provenance ledger. | No | Yes |
| Evidence blobs | Optional copied full transcript/log/attachment bytes in content-addressed storage. | Yes when explicitly copied | No |

Avoid using “cache” for `metadata.sqlite` in product architecture. Use “metadata index” or “source metadata index”. “Cache” is still acceptable for repo-local derived query files such as `index.json` and `brick.sqlite`, because those are disposable projections of the provenance ledger.

## Source-of-truth boundaries

```mermaid
flowchart TD
    Native[Native source storage<br/>Cursor state.vscdb<br/>Claude/Codex JSONL<br/>OpenCode DB<br/>ORGII state] -->|scan / parse metadata| Metadata[(BRICK_HOME/metadata.sqlite<br/>Source metadata index)]
    Native -->|lazy chunk load<br/>no default copy| History[brick history JSON API]
    Metadata -->|list sessions<br/>recent paths<br/>source filters| History

    CLI[brick write commands] -->|append TraceEvent JSONL| Ledger[Provenance ledger<br/>events/queue + events/inbound]
    Ledger -->|rebuild| Derived[Derived query views<br/>index.json<br/>brick.sqlite<br/>views/*.md]

    Native -. explicit copy only .-> Blobs[Content-addressed evidence blobs]
    CLI -. explicit attach/copy .-> Blobs
```

Key points:

- Native source storage remains the raw transcript/source-of-truth for external app history.
- `metadata.sqlite` stores normalized source metadata, not full transcripts by default.
- Brick JSONL events are the durable provenance claims.
- Derived query views can be deleted and rebuilt from Brick JSONL events.
- Evidence blobs exist only when the user explicitly copies/uploads full content.

## Local storage layout

```mermaid
flowchart LR
    subgraph Global[Global Brick home]
        Home[BRICK_HOME<br/>default ~/.brick]
        Metadata[(metadata.sqlite<br/>source metadata index)]
        OptionalBlobs[blobs/<sha256><br/>optional copied evidence]
        Home --> Metadata
        Home --> OptionalBlobs
    end

    subgraph Repo[Repo-local bootstrap / provenance]
        DotBrick[.brick/]
        Config[config.toml<br/>sources/*.toml]
        Events[provenance/events/*.jsonl<br/>append-only ledger]
        Query[provenance/cache/index.json<br/>provenance/cache/brick.sqlite<br/>provenance/views/*.md]
        DotBrick --> Config
        DotBrick --> Events
        Events --> Query
    end

    Config -->|source roots and defaults| Metadata
    Events -->|rebuild projections| Query
```

Current implementation still keeps source profile TOML files in repo-local `.brick/`. The direction is to move source profile rows and scan state into the global metadata index while preserving repo-local bootstrap/config where useful.

## History read path

```mermaid
sequenceDiagram
    participant ORGII as ORGII / Consumer
    participant Brick as brick history
    participant Native as Native source storage
    participant Meta as metadata.sqlite

    ORGII->>Brick: history sessions --source claude_code --format json
    Brick->>Native: scan configured source roots for metadata
    Native-->>Brick: session id, path, mtime, size
    Brick->>Meta: upsert source_session rows
    Brick->>Meta: query page ordered by last_seen_at
    Meta-->>Brick: normalized source metadata rows
    Brick-->>ORGII: JSON DTOs
```

For now `history sessions` and `history recent-paths` are read-through operations: they refresh metadata from configured native roots, then query `metadata.sqlite`. Future source-specific providers should make refresh incremental with fingerprints, parser versions, and source-specific cursors.

## Provenance write path

```mermaid
sequenceDiagram
    participant User as User / Agent
    participant CLI as brick CLI
    participant Git as Git repo
    participant Ledger as JSONL provenance ledger
    participant Views as derived query views

    User->>CLI: brick artifact/evidence/session command
    CLI->>Git: capture repo context when relevant
    Git-->>CLI: branch, commit, dirty state, diff stats
    CLI->>Ledger: append TraceEvent JSONL
    CLI-->>User: IDs and status
    User->>CLI: brick maintenance index/db rebuild
    CLI->>Ledger: read local + inbound events
    CLI->>Views: rebuild index.json, brick.sqlite, Markdown views
```

The provenance ledger is not a metadata index and should not be mutated during history scans. It records accountability claims: who did what, in which session, attached to which mission/artifact, with which evidence.

## ORGII offload boundary

```mermaid
flowchart TD
    subgraph BrickOwns[Brick owns]
        Discovery[Source discovery]
        Scan[Index refresh and delta scan]
        Parsers[Source parsers]
        MetaIndex[Source metadata index]
        Chunks[Lazy chunk providers]
        HistoryAPI[JSON history API]
    end

    subgraph OrgiiOwns[ORGII owns]
        UI[History UI rendering]
        Runtime[ORGII runtime sessions]
        LiveCursor[Cursor live automation]
        AppState[App-private state]
    end

    Discovery --> Scan --> MetaIndex --> HistoryAPI --> UI
    Parsers --> MetaIndex
    Parsers --> Chunks --> HistoryAPI
    Runtime --> UI
    LiveCursor --> Runtime
    AppState --> UI
```

Migration target:

- Brick owns portable external-history discovery, indexing, parsing, metadata, and lazy chunk APIs.
- ORGII consumes Brick's history APIs and keeps UI/runtime orchestration.
- ORGII should not keep a parallel external-history metadata store once Brick reaches source-by-source parity.

## Platform-specific source querying

Platform-specific querying methods, raw formats, JSON contracts, and Brick treatment rules live in `source-querying.md`. Keep this architecture document focused on ownership boundaries and storage semantics.

## Current implementation snapshot

Implemented:

- `BRICK_HOME` resolution with default `~/.brick`.
- `metadata.sqlite` schema skeleton and source-session upsert/list/count APIs.
- Native file source listing for configured source profiles.
- `brick history sources`, `sessions`, `recent-paths`, and placeholder `chunks` JSON commands.
- Read-through source-session metadata indexing for `history sessions`, `history recent-paths`, and native import list/ingest.

Still pending:

- Source profile rows and scan rows persisted into `metadata.sqlite`.
- Incremental delta scanning with parser version, fingerprint, pruning, and source-specific cursors.
- Real chunk loading for Claude Code, Codex, Cursor, OpenCode, Windsurf, and ORGII sources.
- Workspace-root and Git-repo relationship population.
- ORGII feature-flag bridge that shells out to Brick history APIs.
