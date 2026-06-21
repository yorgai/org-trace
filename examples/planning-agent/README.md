# Planning agent example

Brick's planning tools (`mission`, `mission_list`, `show_mission`, `artifact_add`,
`artifact_attach`) are **not** on the main coding-agent MCP surface — that surface
is deliberately just `explain` + `link`. Planning lives behind a flag:

```bash
brick mcp-serve --planning
```

Wire that as a **dedicated planning custom agent** so the main coding agent stays
minimal and only delegates to it when the user actually asks to plan. The
mechanism differs per platform; the MCP server command is the same.

## Claude Code (subagent)

Define a subagent whose MCP config points at the planning server:

```json
{
  "mcpServers": {
    "brick-planning": {
      "type": "stdio",
      "command": "brick",
      "args": ["mcp-serve", "--planning"]
    }
  }
}
```

The main agent (with the default `brick mcp-serve` / `explain` + `link`) spawns
this subagent when the user wants a plan; the subagent has the mission/artifact
tools, the main agent does not.

## Codex (profile)

Add the planning server under a Codex profile / `config.toml`:

```toml
[mcp_servers.brick-planning]
command = "brick"
args = ["mcp-serve", "--planning"]
```

Switch to that profile for planning work, or expose it as a named mode.

## Cursor (custom mode)

Add an MCP server in Cursor settings with command `brick` and args
`mcp-serve --planning`, and bind it to a custom mode used for planning. The
default coding mode keeps only the `explain` + `link` server.

## ORGII (custom agent)

Create a custom agent whose tool set is the planning MCP server
(`brick mcp-serve --planning`). The main agent delegates planning to it.

## Why split it out

In a general coding agent, `mission` / `artifact_add` are noise — the model
rarely reaches for them unprompted, and every extra tool dilutes its attention.
In a *planning* agent they are the whole job, so it uses them reliably. Same
tools, different agent identity — placement, not capability, drives whether a
tool gets used.
