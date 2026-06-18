#!/usr/bin/env bash
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
TMP_ROOT="$(mktemp -d "${TMPDIR:-/tmp}/brick-mvp-smoke.XXXXXX")"
SERVER_PID=""
SERVER_LOG="$TMP_ROOT/server.log"
PORT="${BRICK_SMOKE_PORT:-17821}"
REMOTE="http://127.0.0.1:${PORT}"
REPO_ID="smoke-repo-$(date +%s)-$$"

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

extract_value() {
  local output="$1"
  local key="$2"
  awk -F= -v key="${key}" '$1 == key { print substr($0, length(key) + 2); exit }' <<<"${output}"
}

brick() {
  cargo run --quiet --manifest-path "${ROOT_DIR}/Cargo.toml" -p brick -- "$@"
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

run cargo build --quiet --manifest-path "${ROOT_DIR}/Cargo.toml" -p brick -p brick-server

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

run brick --store-root "${STORE_ONE}" init
run brick source configure --name cursor --app-id cursor --actor-id smoke-agent --actor-type agent --store-root "${STORE_ONE}" --notes "Smoke source"
run brick source use --name cursor
run brick --source cursor source show --name cursor

mission_output="$(capture brick --source cursor mission create "Smoke MVP mission" --description "End-to-end smoke")"
printf '%s\n' "${mission_output}"
mission_id="$(extract_value "${mission_output}" mission_id)"
[[ -n "${mission_id}" ]]

session_output="$(capture brick --source cursor session start --mission "${mission_id}" --name "Smoke session" --set-current --print-env)"
printf '%s\n' "${session_output}"
require_output "${session_output}" "BRICK_SESSION_ID"
session_id="$(extract_value "${session_output}" session_id)"
[[ -n "${session_id}" ]]
run brick --source cursor session current
run brick --source cursor context show

artifact_output="$(capture brick --source cursor artifact decision --mission "${mission_id}" --session "${session_id}" "Smoke decision" --body "Choose the MVP smoke path")"
printf '%s\n' "${artifact_output}"
artifact_id="$(extract_value "${artifact_output}" artifact_id)"
[[ -n "${artifact_id}" ]]
run brick --source cursor artifact update --artifact "${artifact_id}" --session "${session_id}" --title "Updated smoke decision" --body "Updated by smoke harness" --kind review

ATTACHMENT_FILE="${TMP_ROOT}/attachment.txt"
SESSION_LOG_FILE="${TMP_ROOT}/session.log"
printf 'attachment body\n' > "${ATTACHMENT_FILE}"
printf '{"role":"assistant","message":"smoke"}\n' > "${SESSION_LOG_FILE}"
run brick --source cursor artifact upload --artifact "${artifact_id}" --session "${session_id}" --path "${ATTACHMENT_FILE}" --name smoke.txt --content-type text/plain
run brick --source cursor session upload-log --session "${session_id}" --path "${SESSION_LOG_FILE}" --format jsonl --source cursor

run brick --source cursor diff capture --artifact "${artifact_id}" --session "${session_id}" --target working
run git add tracked.txt
run brick --source cursor diff capture --artifact "${artifact_id}" --session "${session_id}" --target staged
run brick --source cursor artifact file --artifact "${artifact_id}" --session "${session_id}" tracked.txt

run brick --source cursor index rebuild
run brick --source cursor index status
run brick --source cursor db rebuild
run brick --source cursor db status
run brick --source cursor db sessions --limit 10 --app-id cursor --actor-id smoke-agent
run brick --source cursor db artifacts --limit 10 --session "${session_id}" --mission "${mission_id}"
run brick --source cursor inspect mission "${mission_id}"
run brick --source cursor inspect session "${session_id}"
run brick --source cursor inspect artifact "${artifact_id}"
run brick --source cursor inspect file tracked.txt

CURSOR_FIXTURE="${TMP_ROOT}/cursor.jsonl"
CI_FIXTURE="${TMP_ROOT}/ci.json"
printf '{"role":"user","message":"imported smoke prompt","title":"Smoke import"}\n' > "${CURSOR_FIXTURE}"
cat > "${CI_FIXTURE}" <<'JSON'
{
  "job_name": "smoke-ci",
  "status": "success",
  "url": "https://ci.example.invalid/smoke",
  "commit": "0000000000000000000000000000000000000000"
}
JSON
run brick --source cursor import cursor --path "${CURSOR_FIXTURE}" --session "${session_id}" --mission "${mission_id}" --app-session-id cursor-smoke --app-session-name "Cursor Smoke"
run brick --source cursor import ci --path "${CI_FIXTURE}" --mission "${mission_id}" --session "${session_id}"
run brick --source cursor index rebuild
run brick --source cursor db rebuild

cd "${ROOT_DIR}"
brick_server serve --bind "127.0.0.1:${PORT}" --data-dir "${SERVER_DATA}" >"${SERVER_LOG}" 2>&1 &
SERVER_PID=$!
wait_for_server
run curl -fsS "${REMOTE}/health"

cd "${REPO_ONE}"
run brick --source cursor push --remote "${REMOTE}" --repo-id "${REPO_ID}"
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
run brick --store-root "${STORE_TWO}" init
run brick --store-root "${STORE_TWO}" pull --remote "${REMOTE}" --repo-id "${REPO_ID}"
run brick --store-root "${STORE_TWO}" index rebuild
run brick --store-root "${STORE_TWO}" db rebuild
run brick --store-root "${STORE_TWO}" db sessions --limit 20
run brick --store-root "${STORE_TWO}" db artifacts --limit 20 --mission "${mission_id}"

cd "${ROOT_DIR}"
run brick_server rebuild-index --data-dir "${SERVER_DATA}" --repo-id "${REPO_ID}"

printf '\nBrick MVP smoke completed successfully for repo_id=%s\n' "${REPO_ID}"
