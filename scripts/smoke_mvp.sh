#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
SCRATCH_ROOT="${ORGII_SCRATCHPAD:-/private/var/folders/sj/tbzp59657v35lt95bs16l_040000gn/T/orgii-501/Users_vinceorz_Projects_Brick-Vault/sdeagent-3b425f71-3901-49e9-9818-324a6029eb9e/scratchpad}"
mkdir -p "${SCRATCH_ROOT}"
TMP_ROOT="$(mktemp -d "${SCRATCH_ROOT}/brick-mvp-smoke.XXXXXX")"
SERVER_PID=""
SERVER_LOG="${TMP_ROOT}/server.log"
PORT="${BRICK_SMOKE_PORT:-17821}"
REMOTE="http://127.0.0.1:${PORT}"
REPO_ID="smoke-repo-$(date +%s)-$$"
ORG_ID="org_smoke"

cleanup() {
  local exit_code=$?
  if [[ -n "${SERVER_PID}" ]] && kill -0 "${SERVER_PID}" 2>/dev/null; then
    kill "${SERVER_PID}" 2>/dev/null || true
    wait "${SERVER_PID}" 2>/dev/null || true
  fi
  rm -rf "${TMP_ROOT}"
  exit "${exit_code}"
}
trap cleanup EXIT INT TERM

run() {
  printf '\n==> %s\n' "$*"
  "$@"
}

capture() {
  printf '\n==> %s\n' "$*" >&2
  "$@"
}

require_output() {
  local output="$1"
  local needle="$2"
  if ! grep -Fq "${needle}" <<<"${output}"; then
    printf 'Expected output to contain %q, got:\n%s\n' "${needle}" "${output}" >&2
    exit 1
  fi
}

json_value() {
  local output="$1"
  local key="$2"
  python3 -c 'import json, sys; print(json.load(sys.stdin)[sys.argv[1]])' "${key}" <<<"${output}"
}

brick() {
  cargo run --quiet --manifest-path "${ROOT_DIR}/Cargo.toml" -p brick --features sync -- "$@"
}

brick_server() {
  cargo run --quiet --manifest-path "${ROOT_DIR}/Cargo.toml" -p brick-server -- "$@"
}

wait_for_server() {
  local attempts=60
  for _ in $(seq 1 "${attempts}"); do
    if curl -fsS "${REMOTE}/health" >/dev/null 2>&1; then
      return 0
    fi
    if [[ -n "${SERVER_PID}" ]] && ! kill -0 "${SERVER_PID}" 2>/dev/null; then
      printf 'brick-server exited early. Log:\n' >&2
      sed -n '1,160p' "${SERVER_LOG}" >&2 || true
      exit 1
    fi
    sleep 0.25
  done
  printf 'Timed out waiting for %s. Log:\n' "${REMOTE}" >&2
  sed -n '1,160p' "${SERVER_LOG}" >&2 || true
  exit 1
}

printf 'Brick MVP smoke temp root: %s\n' "${TMP_ROOT}"

run cargo build --quiet --manifest-path "${ROOT_DIR}/Cargo.toml" -p brick -p brick-server --features sync

REPO_ONE="${TMP_ROOT}/repo-one"
STORE_ONE="${TMP_ROOT}/store-one"
SERVER_DATA="${TMP_ROOT}/server-data"
mkdir -p "${REPO_ONE}" "${STORE_ONE}" "${SERVER_DATA}"
cd "${REPO_ONE}"
run git init --quiet
run git config user.email smoke@example.invalid
run git config user.name "Brick Smoke"
printf 'initial\n' > tracked.txt
run git add tracked.txt
run git commit --quiet -m "initial smoke commit"
printf 'working change\n' >> tracked.txt

link_output="$(capture brick --store-root "${STORE_ONE}" --actor-id smoke-agent --actor-type agent --session 11111111-1111-4111-8111-111111111111 link --note "Smoke captured working diff")"
printf '%s\n' "${link_output}"
require_output "${link_output}" '"linked":true'
require_output "${link_output}" 'tracked.txt'
effect_event="$(json_value "${link_output}" effect_event)"

explain_output="$(capture brick --store-root "${STORE_ONE}" explain "${effect_event}")"
printf '%s\n' "${explain_output}"
require_output "${explain_output}" 'Smoke captured working diff'

cd "${ROOT_DIR}"
brick_server add-org-member --data-dir "${SERVER_DATA}" --org-id "${ORG_ID}" --user-id smoke-user >/dev/null
members_output="$(capture brick_server list-org-members --data-dir "${SERVER_DATA}" --org-id "${ORG_ID}")"
printf '%s\n' "${members_output}"
require_output "${members_output}" 'member_count=1'
require_output "${members_output}" 'user_id=smoke-user'
brick_server serve --bind "127.0.0.1:${PORT}" --data-dir "${SERVER_DATA}" >"${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
wait_for_server
run curl -fsS "${REMOTE}/health"

cd "${REPO_ONE}"
push_output="$(capture brick --store-root "${STORE_ONE}" sync push --remote "${REMOTE}" --repo-id "${REPO_ID}" --org-id "${ORG_ID}")"
printf '%s\n' "${push_output}"
require_output "${push_output}" 'accepted_count='
run curl -fsS "${REMOTE}/v1/repos/${REPO_ID}/index/status"
run curl -fsS "${REMOTE}/v1/repos/${REPO_ID}/sessions?limit=20"

REPO_TWO="${TMP_ROOT}/repo-two"
STORE_TWO="${TMP_ROOT}/store-two"
mkdir -p "${REPO_TWO}" "${STORE_TWO}"
cd "${REPO_TWO}"
run git init --quiet
run git config user.email smoke@example.invalid
run git config user.name "Brick Smoke"
printf 'second repo\n' > README.md
run git add README.md
run git commit --quiet -m "initial second repo"
pull_output="$(capture brick --store-root "${STORE_TWO}" sync pull --remote "${REMOTE}" --repo-id "${REPO_ID}")"
printf '%s\n' "${pull_output}"
require_output "${pull_output}" 'pulled_event_count='
pulled_explain_output="$(capture brick --store-root "${STORE_TWO}" explain "${effect_event}")"
printf '%s\n' "${pulled_explain_output}"
require_output "${pulled_explain_output}" 'Smoke captured working diff'

cd "${ROOT_DIR}"
printf '\nBrick MVP smoke completed successfully for repo_id=%s\n' "${REPO_ID}"
