# Repository Rules

## Modularity

Keep files focused and modular. Prefer splitting code by responsibility instead of growing large catch-all files.

Recommended structure for Rust crates:

- `lib.rs` should re-export public modules and hold only small glue code.
- Put domain types in focused modules such as `ids.rs`, `events.rs`, `payloads.rs`, and `identity.rs`.
- Put IO-heavy or platform-specific logic in separate modules such as `store.rs`, `repo_context.rs`, and `git.rs`.
- Keep CLI parsing separate from command execution when command files become large.

## File size limits

Aim for these limits:

- Rust source files: 600 lines maximum.
- CLI command files: 600 lines maximum.
- Markdown docs: 500 lines maximum.
- Config files: 300 lines maximum.

If a file approaches the limit, split it before adding more features.

## Code style

- Avoid single-letter variable names unless used in a very small local scope with obvious meaning.
- Prefer typed domain values over raw strings for event types, IDs, statuses, roles, and source kinds.
- Do not silently swallow errors. Return errors with context so callers can decide how to handle them.
- Avoid dead code, compatibility shims, and unused abstractions.
- Keep local-first behavior working before adding server-specific features.

## Notes and comments

- Every Rust source file should have a module-level `//!` note explaining the module's purpose and boundary.
- Public structs, enums, constants, and functions should have useful `///` doc comments when they form part of the crate or CLI behavior.
- Comments should explain intent, constraints, trade-offs, and provenance semantics. Do not add comments that merely narrate obvious code steps.
- Keep docs and comments in English.

## Testing and compilation

- Every behavior change should include or update tests at the closest useful layer: protocol serialization tests, core unit tests, CLI smoke tests, or server handler tests.
- Before considering work complete, run `cargo fmt --all`, `cargo check --workspace`, and `cargo test --workspace` from the repository root.
- Run `cargo doc --workspace --no-deps` when public APIs or doc comments change.
- `cargo check --workspace` is the minimum compile signal. `cargo test --workspace` is the stronger signal because it compiles test targets and runs the suite.
- For CLI behavior, run at least one local smoke command with `cargo run -p brick -- <command>` when the change touches command parsing or output.
- Do not rely only on editor diagnostics; stale IDE warnings can happen. Trust Cargo verification from the workspace root.

## Phase discipline

Phase 1 is local trace recording only:

- typed events
- identity resolution
- Git repo context capture
- JSONL local store
- local CLI commands
- local tests

Phase 2 may add rebuildable local derived indexes and richer local inspection commands. The JSONL event log must remain the source of truth.

Phase 3 may add a minimal self-hosted append-only event endpoint and non-destructive CLI push dry-runs. Do not drain local queues, add auth, or implement conflict resolution until retry and authorization semantics are designed.

Phase 4 may add configurable local storage roots and source profiles. Storage selection must keep the JSONL event log as the source of truth, preserve repo-local `.brick/provenance` defaults, and keep source profile bootstrap config readable before a profile-selected store root exists.

Phase 6 may add a rebuildable SQLite cache under the effective local store cache directory. The SQLite database must remain derived data, expose typed read-only query commands by default, and avoid arbitrary mutating SQL from the CLI.

Do not add full server sync, auth, or importers until local indexing, inspection, configurable source profiles, SQLite query cache, and the minimal append-only server surface are stable.
