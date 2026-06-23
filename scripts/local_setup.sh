#!/usr/bin/env bash
#
# local_setup.sh — build the current source and install it as your local `brick`.
#
# Run this after changing the code to refresh ~/.cargo/bin/brick to the latest
# build. By default it also re-registers the Brick MCP server with every detected
# AI tool so your editor/agent picks up the new binary.
#
# Usage:
#   scripts/local_setup.sh              # build + install + register MCP (default)
#   scripts/local_setup.sh --no-agents  # build + install only, skip MCP registration
#   scripts/local_setup.sh --debug      # faster debug build via symlink (no cargo install)
#   scripts/local_setup.sh --check      # just report installed vs source, do nothing
#
# Env:
#   BRICK_INSTALL_BIN   override the install dir (default: cargo's bin dir)
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

REGISTER_AGENTS=1
MODE="release"
CHECK_ONLY=0

for arg in "$@"; do
  case "$arg" in
    --no-agents) REGISTER_AGENTS=0 ;;
    --debug)     MODE="debug" ;;
    --check)     CHECK_ONLY=1 ;;
    -h|--help)
      sed -n '2,17p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
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
  cargo build -p brick
  BIN_SRC="$ROOT_DIR/target/debug/brick"
  DEST_DIR="${BRICK_INSTALL_BIN:-$HOME/.cargo/bin}"
  mkdir -p "$DEST_DIR"
  ln -sf "$BIN_SRC" "$DEST_DIR/brick"
  ok "symlinked $DEST_DIR/brick -> $BIN_SRC"
else
  say "Building + installing brick (release)"
  if [[ -n "${BRICK_INSTALL_BIN:-}" ]]; then
    cargo install --path crates/cli --force --root "$(dirname "$BRICK_INSTALL_BIN")"
  else
    cargo install --path crates/cli --force
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

  # `--global` only touches per-user MCP configs; it does NOT write the
  # per-project memory block that tells the agent WHEN to reach for `explain`.
  # Without that block an agent never learns the workflow and silently falls back
  # to grep/read_file. Each client reads a DIFFERENT file: Claude→CLAUDE.md,
  # Codex→AGENTS.md, ORGII→<repo>/.orgii/agent-rules.md.
  #
  # BRICK_AGENT_REPOS is a space-separated list of repos. Each entry is either:
  #   /path/to/repo            → install ALL detected clients' blocks (mixed repo)
  #   /path/to/repo@orgii      → install ONLY that client (e.g. an ORGII workspace,
  #                              so we don't pollute it with CLAUDE.md/AGENTS.md it
  #                              never reads). @target is any `brick agent` target.
  # We verify the EXACT target we installed reports `present` (not a blanket
  # `--target all | grep present`, which goes green if any one client landed and
  # would hide the very "ORGII file missing" failure this is meant to catch).
  if [[ -n "${BRICK_AGENT_REPOS:-}" ]]; then
    say "Installing the agent memory block into target repos"
    # Space-separated only, so absolute paths containing ':' stay intact.
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
      # --force rolls a stale block forward to the current TEMPLATE_VERSION.
      if (cd "$repo" && brick agent install --target "$_target" --force >/dev/null); then
        # Verify the SAME target we just installed is actually present on disk —
        # a silent "installed" with no readable file is exactly the failure that
        # makes an agent never learn the workflow (it then falls back to grep).
        if (cd "$repo" && brick agent status --target "$_target" 2>/dev/null | grep -q "present"); then
          ok "memory block ($_target) installed + verified in $repo"
        else
          printf '\033[1;31m  installed but NOT verified present for target=%s in %s — agent will not see Brick\033[0m\n' "$_target" "$repo"
        fi
      else
        printf '\033[1;33m  agent install failed for target=%s in %s — install it manually there\033[0m\n' "$_target" "$repo"
      fi
    done
  else
    printf '  \033[1;33mnote:\033[0m --global updated MCP configs only. To teach an agent the\n'
    printf '        explain workflow, the per-project memory block must be installed IN that\n'
    printf '        repo (each client reads a different file — ORGII uses .orgii/agent-rules.md).\n'
    printf '        Run there:\n'
    printf '          cd /path/to/your/repo && brick agent install --target orgii   # ORGII workspace\n'
    printf '          cd /path/to/your/repo && brick agent install --target all     # mixed repo\n'
    printf '        or re-run this script with BRICK_AGENT_REPOS set, e.g.:\n'
    printf '          BRICK_AGENT_REPOS="%s@orgii" scripts/local_setup.sh\n' "$HOME/Projects/ORGII"
  fi
else
  ok "skipped MCP registration (--no-agents)"
fi

say "Done"
