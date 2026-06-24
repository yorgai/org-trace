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

`npm install` already wires Brick into every MCP-capable coding agent on your
machine (it runs `brick agent install --global` for you). There is **no
`brick init`** and nothing is written into your repositories — Brick is
zero-config. To re-run or verify it by hand:

```sh
brick agent install --global   # idempotent; re-run after installing a new agent
brick agent status             # see which agents are wired up
```

`agent install` does two things, both idempotent and non-destructive:

1. **Push** — injects a short instruction block into `CLAUDE.md` / `AGENTS.md` /
   `GEMINI.md` and registers a Claude Code `PreToolUse` hook so recall fires
   automatically before edits.
2. **Pull** — registers the Brick **MCP server** (`brick mcp-serve`) into Claude
   Code (`~/.claude.json`) and Cursor (`~/.cursor/mcp.json`), so any MCP-capable
   agent can call Brick's two tools on demand: **`explain`** (who changed this
   code and why) and **`link`** (record why after a change).

```sh
brick setup                    # local-only; wires agents and does not require an account
brick setup --email you@example.com
brick setup --email you@example.com --code <otp-code>
```

Without `--email`, Brick stays local-only. With Supabase login, Brick enables best-effort sharing sync for the normal agent path: `explain` pulls before reading and `link` pushes after writes.

## Use it directly

```sh
brick explain src/main.rs:42                 # why this code looks this way
brick link --note "Reason for this change"  # record why after a change
```

## Environment variables

- `BRICK_SKIP_DOWNLOAD=1` — skip the binary download on `npm install` (falls
  back to a `brick` already on your PATH).
- `BRICK_SKIP_AGENT_INSTALL=1` — skip the automatic agent wiring on install.
- `BRICK_HOME` — override the global Brick home (default `~/.brick`).

## License

AGPL-3.0-or-later
