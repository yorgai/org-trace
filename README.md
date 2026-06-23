# Brick

Brick is the **causal memory of a codebase**: a local-first provenance layer that
answers *why* code looks the way it does across AI coding sessions.

The current public surface is intentionally small:

- `brick explain <anchor>` — read the causal context for a file, line range,
  mission, artifact, or event.
- `brick link ...` — record WHY for a real change.
- `brick mcp-serve` — expose the coding-agent MCP surface (`explain` + `link`).
- `brick mcp-serve --planning` — expose the separate planning MCP surface.
- `brick agent install/status/uninstall` — install Brick awareness, MCP config,
  skills, and hooks for supported coding agents.

Team sync is still the next product direction, but it should serve this same
surface: `link` writes provenance, automatic sync shares it, and `explain` reads
team context. The old broad CLI browsing surface has been removed.

## Quick start

```bash
cargo run -p brick -- agent install --global
cargo run -p brick -- explain src/main.rs:42
cargo run -p brick -- link --note "Why this change was made"
cargo run -p brick -- mcp-serve
```

For planning agents:

```bash
cargo run -p brick -- mcp-serve --planning
```

## Agent workflow

When an agent investigates existing code, it should call `explain` before
reconstructing history from grep or git. After a non-trivial change, it should use
`link` to bind the reason to the current diff or to an explicit effect anchor.

`agent install` writes the managed awareness block and configures supported MCP /
skill / hook integrations so agents can use this automatically.

## Build

```bash
cargo fmt --all
cargo check -p brick
cargo test -p brick
```

Sync internals are kept for the upcoming automatic team-sharing path, but users
should not need separate history/blame/log/search commands in the normal flow.
