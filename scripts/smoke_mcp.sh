#!/usr/bin/env bash
# MCP capability-kit smoke test.
#
# Exercises all 13 `brick mcp-serve` tools over the real MCP stdio JSON-RPC
# protocol, against a real git repository, with two native source profiles
# (codex_app + claude_code) backed by real transcript files. Verifies cross-tool
# memory (a Codex session's work is recalled by a Claude session), FTS5 search,
# the planning loop, and liveness-aware bulletin-board claim retirement.
#
# The repo under test defaults to a fresh clone of THIS repository into a temp
# dir, so nothing touches your working tree. Override with SMOKE_SRC_REPO to
# point at any other local git repo (it is cloned, never modified in place).
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/brick-mcp-smoke.XXXXXX")"

cleanup() {
  local exit_code=$?
  rm -rf "${TMP_ROOT}"
  exit "${exit_code}"
}
trap cleanup EXIT INT TERM

printf 'Brick MCP smoke temp root: %s\n' "${TMP_ROOT}"

# --- pick a python3 ---
PYTHON="${PYTHON:-python3}"
if ! command -v "${PYTHON}" >/dev/null 2>&1; then
  printf 'python3 is required for the MCP smoke test (stdio JSON-RPC driver).\n' >&2
  exit 1
fi

# --- build the binary once, use the built path directly (no cargo-run noise on stdout) ---
printf '\n==> cargo build -p brick\n'
cargo build --quiet --manifest-path "${ROOT_DIR}/Cargo.toml" -p brick
BRICK_BIN="${ROOT_DIR}/target/debug/brick"
[[ -x "${BRICK_BIN}" ]] || { printf 'built brick binary not found at %s\n' "${BRICK_BIN}" >&2; exit 1; }

# --- clone the repo under test into the temp dir (never touch the original) ---
SRC_REPO="${SMOKE_SRC_REPO:-${ROOT_DIR}}"
SMOKE_REPO="${TMP_ROOT}/repo"
printf '\n==> git clone %s\n' "${SRC_REPO}"
git clone --quiet "${SRC_REPO}" "${SMOKE_REPO}"

# --- choose two real tracked source files for the two agents to reference ---
pick_file() {
  # first tracked file matching one of the given extensions
  git -C "${SMOKE_REPO}" ls-files | grep -E '\.(rs|ts|tsx|js|py|go|md)$' | sed -n "${1}p"
}
FILE_CODEX="$(pick_file 1)"
FILE_CLAUDE="$(pick_file 2)"
: "${FILE_CLAUDE:=${FILE_CODEX}}"
if [[ -z "${FILE_CODEX}" ]]; then
  printf 'No suitable source files found in %s\n' "${SMOKE_REPO}" >&2
  exit 1
fi
printf '   codex file:  %s\n   claude file: %s\n' "${FILE_CODEX}" "${FILE_CLAUDE}"

# --- isolated dirs ---
export BRICK_BIN SMOKE_REPO FILE_CODEX FILE_CLAUDE
export BRICK_HOME="${TMP_ROOT}/home"
export CODEX_DIR="${TMP_ROOT}/codex"
export CLAUDE_DIR="${TMP_ROOT}/claude"
mkdir -p "${BRICK_HOME}" "${CODEX_DIR}" "${CLAUDE_DIR}"

printf '\n==> MCP driver\n'
"${PYTHON}" "${ROOT_DIR}/scripts/smoke_mcp_driver.py"

printf '\nBrick MCP smoke completed successfully.\n'
