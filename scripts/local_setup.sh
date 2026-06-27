#!/usr/bin/env bash
#
# local_setup.sh — build the current source and install it as your local `brick`.
#
# Run this after changing the code to refresh ~/.cargo/bin/brick to the latest
# build. By default it also re-registers the Brick MCP server with every detected
# AI tool so your editor/agent picks up the new binary.
#
# Usage:
#   scripts/local_setup.sh              # build + install + register all agents + refresh ORGII + optional share login
#   scripts/local_setup.sh --no-agents  # build + install only, skip all agent registration
#   scripts/local_setup.sh --no-orgii-refresh
#                                      # skip ORGII live MCP reconnect after global registration
#   scripts/local_setup.sh --no-login   # skip the optional Supabase sharing prompt
#   scripts/local_setup.sh --debug      # faster debug build via symlink (no cargo install)
#   scripts/local_setup.sh --check      # just report installed vs source, do nothing
#
# Env:
#   BRICK_INSTALL_BIN   override the install dir (default: cargo's bin dir)
#   BRICK_NO_LOGIN=1    skip the optional Supabase sharing prompt
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

REGISTER_AGENTS=1
REFRESH_ORGII=1
MODE="release"
CHECK_ONLY=0
PROMPT_LOGIN=1

for arg in "$@"; do
  case "$arg" in
    --no-agents) REGISTER_AGENTS=0 ; REFRESH_ORGII=0 ;;
    --no-orgii-refresh) REFRESH_ORGII=0 ;;
    --no-login)  PROMPT_LOGIN=0 ;;
    --debug)     MODE="debug" ;;
    --check)     CHECK_ONLY=1 ;;
    -h|--help)
      sed -n '2,18p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    *)
      printf 'unknown flag: %s (try --help)\n' "$arg" >&2
      exit 2
      ;;
  esac
done

say() { printf '\n\033[1;34m==>\033[0m %s\n' "$*"; }
ok()  { printf '\033[1;32m  ✓\033[0m %s\n' "$*"; }

require_agent_status() {
  local status="$1"
  local target="$2"
  if grep -q "^${target} present " <<< "$status"; then
    ok "$target present"
  else
    printf '\033[1;31m  missing required agent install target: %s\033[0m\n' "$target" >&2
    return 1
  fi
}

# Resolve where `brick` is / will be installed.
INSTALLED_BIN="$(command -v brick || true)"

if [[ "$CHECK_ONLY" == 1 ]]; then
  say "Installed vs source"
  if [[ -n "$INSTALLED_BIN" ]]; then
    printf '  installed: %s\n' "$INSTALLED_BIN"
    ls -la "$INSTALLED_BIN" | awk '{print "  mtime:    ", $6, $7, $8}'
  else
    printf '  installed: (none on PATH)\n'
  fi
  HEAD_REV="$(git rev-parse --short HEAD 2>/dev/null || echo '?')"
  HEAD_WHEN="$(git log -1 --format='%cd' --date=format:'%b %d %H:%M' 2>/dev/null || echo '?')"
  printf '  HEAD:      %s (%s)\n' "$HEAD_REV" "$HEAD_WHEN"
  if [[ -n "$INSTALLED_BIN" ]] && [[ -n "$(find crates -name '*.rs' -newer "$INSTALLED_BIN" 2>/dev/null | head -1)" ]]; then
    printf '\n  \033[1;33msource is NEWER than the installed binary — run scripts/local_setup.sh to update\033[0m\n'
  else
    ok "installed binary is up to date with source"
  fi
  exit 0
fi

if [[ "$MODE" == "debug" ]]; then
  # Fast path for iterating: debug build + symlink, no full release `cargo install`.
  say "Building brick (debug)"
  cargo build -p brick --features sync
  BIN_SRC="$ROOT_DIR/target/debug/brick"
  DEST_DIR="${BRICK_INSTALL_BIN:-$HOME/.cargo/bin}"
  mkdir -p "$DEST_DIR"
  ln -sf "$BIN_SRC" "$DEST_DIR/brick"
  ok "symlinked $DEST_DIR/brick -> $BIN_SRC"
else
  say "Building + installing brick (release)"
  if [[ -n "${BRICK_INSTALL_BIN:-}" ]]; then
    cargo install --path crates/cli --features sync --force --root "$(dirname "$BRICK_INSTALL_BIN")"
  else
    cargo install --path crates/cli --features sync --force
  fi
  ok "installed to cargo bin dir"
fi

# Confirm what we ended up with.
NEW_BIN="$(command -v brick || true)"
if [[ -z "$NEW_BIN" ]]; then
  printf '\n\033[1;31m  brick is not on PATH — add your cargo bin dir to PATH\033[0m\n' >&2
  exit 1
fi
say "Active brick"
printf '  %s\n' "$NEW_BIN"
brick --version 2>/dev/null || true

if [[ "$REGISTER_AGENTS" == 1 ]]; then
  say "Registering Brick MCP server with detected AI tools (global)"
  # Re-point every tool's MCP config at this binary. The default --target is
  # `all`, so this registers every detected tool. Idempotent; --force rewrites
  # the managed block so a moved/renamed binary path is corrected. This step also
  # installs the Brick Agent Skill (SKILL.md) into skill-capable clients
  # (Claude Code, Codex, Cursor, Gemini, ORGII, and Windsurf) so the skill
  # description routes "why/how did this happen" investigations to `brick
  # explain` more reliably than the markdown block alone.
  if brick agent install --global --force; then
    ok "MCP registration refreshed"
  else
    printf '\033[1;33m  agent install reported an issue (often: a tool not installed) — safe to ignore\033[0m\n'
  fi

  say "Verifying global AI tool install"
  AGENT_STATUS="$(brick agent status --global)"
  for target in \
    claude_mcp codex_mcp cursor_mcp gemini_mcp orgii_mcp windsurf_mcp vscode_mcp \
    claude_skill codex_skill cursor_skill gemini_skill orgii_skill windsurf_skill; do
    require_agent_status "$AGENT_STATUS" "$target"
  done

  # The global MCP + Skill install is the normal path: one install makes Brick
  # available across repos for skill-capable clients. Per-repo memory blocks are
  # optional overrides for clients/workspaces that explicitly read a project file.
  # BRICK_AGENT_REPOS is kept for that advanced case only.
  if [[ -n "${BRICK_AGENT_REPOS:-}" ]]; then
    say "Installing optional per-repo agent memory blocks"
    read -r -a _repos <<< "$BRICK_AGENT_REPOS"
    for entry in "${_repos[@]}"; do
      [[ -z "$entry" ]] && continue
      _target="all"
      repo="$entry"
      if [[ "$entry" == *"@"* ]]; then
        repo="${entry%@*}"
        _target="${entry##*@}"
      fi
      if [[ ! -d "$repo" ]]; then
        printf '\033[1;33m  skip (not a dir): %s\033[0m\n' "$repo"
        continue
      fi
      if (cd "$repo" && brick agent install --target "$_target" --force >/dev/null); then
        if (cd "$repo" && brick agent status --target "$_target" 2>/dev/null | grep -q "present"); then
          ok "optional memory block ($_target) installed + verified in $repo"
        else
          printf '\033[1;31m  installed but NOT verified present for target=%s in %s\033[0m\n' "$_target" "$repo"
        fi
      else
        printf '\033[1;33m  optional agent install failed for target=%s in %s\033[0m\n' "$_target" "$repo"
      fi
    done
  else
    ok "global MCP + Skill install is ready across repos"
    printf '  Optional: set BRICK_AGENT_REPOS if you also want project-local memory blocks.\n'
  fi
else
  ok "skipped MCP registration (--no-agents)"
fi

if [[ "$REFRESH_ORGII" == 1 ]]; then
  say "Refreshing live ORGII MCP process"
  if curl -fsS --max-time 2 http://127.0.0.1:13847/agent/health >/dev/null 2>&1; then
    curl -fsS --max-time 10 -X POST http://127.0.0.1:13847/agent/test/mcp/disconnect-server \
      -H 'content-type: application/json' \
      -d '{"server_name":"brick"}' >/dev/null || true
    if curl -fsS --max-time 20 -X POST http://127.0.0.1:13847/agent/test/mcp/reconnect-server \
      -H 'content-type: application/json' \
      -d '{"server_name":"brick"}' >/dev/null; then
      ok "ORGII MCP server reconnected"
    else
      printf '\033[1;33m  ORGII is running, but MCP reconnect failed — restart ORGII or reconnect brick manually\033[0m\n'
    fi
  else
    ok "ORGII test server not running; skip MCP reconnect"
  fi
else
  ok "skipped ORGII MCP reconnect"
fi

if [[ "$PROMPT_LOGIN" == 1 && "${BRICK_NO_LOGIN:-0}" != "1" ]]; then
  say "Optional Supabase sharing login"
  if brick sync whoami 2>/dev/null | grep -q '^logged_in=true'; then
    ok "already logged in; sharing sync is enabled"
  elif [[ -t 0 ]]; then
    printf '  Brick works local-only without an account. Log in now to enable sharing sync? [y/N] '
    read -r answer
    case "$answer" in
      y|Y|yes|YES)
        printf '  Email: '
        read -r email
        if [[ -n "$email" ]]; then
          brick setup --agents false --email "$email"
          printf '  Code: '
          read -r code
          if [[ -n "$code" ]]; then
            brick setup --agents false --email "$email" --code "$code"
            ok "sharing sync enabled"
          else
            printf '  \033[1;33msharing login pending; finish later with:\033[0m\n'
            printf '    brick setup --email %s --code <code>\n' "$email"
          fi
        fi
        ;;
      *)
        ok "staying local-only; run 'brick setup --email <you@example.com>' later to enable sharing"
        ;;
    esac
  else
    ok "non-interactive shell; run 'brick setup --email <you@example.com>' later to enable sharing"
  fi
else
  ok "skipped optional Supabase sharing login"
fi

say "Done"
