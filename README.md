# ORGII Trace

ORGII Trace is a self-host-first provenance CLI and server for tracking human and AI agent execution history around missions, sessions, artifacts, files, and commits.

## Packages

- `orgii-trace`: standalone CLI client
- `orgii-trace-server`: self-hosted provenance remote
- `org-trace-protocol`: shared event schema types
- `org-trace-core`: local storage and sync primitives
- `org-trace-importers`: external trace importers

## Development

```bash
cargo check --workspace
cargo run -p orgii-trace -- init
cargo run -p orgii-trace-server -- serve
```

## Local storage model

Local writes use an append-only filesystem event log under `.orgii/provenance/`. SQLite can be added as a derived cache/index later.

## License

AGPL-3.0-or-later.

## Status

This repository is an initial scaffold. See `docs/architecture/agent-provenance-trace--0618.md` for the current architecture plan.
