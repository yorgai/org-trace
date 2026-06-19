# @yorgai/brick

**Brick** is a local metadata index of your past AI coding sessions across every
tool on your machine — Claude Code, Cursor, Codex, Gemini, and more. It lets an
agent recall *who changed a file and why* instead of rediscovering it.

## Install

```sh
npm install -g @yorgai/brick
```

This downloads the prebuilt `brick` binary for your platform from the GitHub
Release and puts `brick` on your PATH. To try it without a global install:

```sh
npx @yorgai/brick version
```

Supported platforms: macOS (Apple Silicon & Intel), Linux (x64 & arm64),
Windows (x64). On unsupported platforms, build from source with
`cargo install --path crates/cli`.

## Make your agents use it

```sh
brick init               # discover your tools' local session history
brick agent install --global
```

`agent install` does two things, both idempotent and non-destructive:

1. **Push** — injects a short instruction block into `CLAUDE.md` / `AGENTS.md` /
   `GEMINI.md` and registers a Claude Code `PreToolUse` hook so recall fires
   automatically before edits.
2. **Pull** — registers the Brick **MCP server** (`brick mcp-serve`) into Claude
   Code (`~/.claude.json`) and Cursor (`~/.cursor/mcp.json`), so any MCP-capable
   agent can call Brick's tools on demand: `explore_memory`, `recall_file`,
   `search_sessions`, `read_session`.

## Use it directly

```sh
brick metadata recall --path src/main.rs   # who changed this file & why
brick metadata query --query "auth race"   # find past sessions by topic
```

## Environment variables

- `BRICK_SKIP_DOWNLOAD=1` — skip the binary download on `npm install` (falls
  back to a `brick` already on your PATH).

## License

AGPL-3.0-or-later
