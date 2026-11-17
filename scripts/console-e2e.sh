#!/usr/bin/env bash
# The console in a real browser (Playwright, Chromium) against the real stack: the release
# collector serving the built console under its CSP, the reference host instrumented with
# the SDK, and data that exercises the honest states (a lost publish response, a complete
# operation, an operation known only from gap reports, hostile event text, a denial).
set -euo pipefail
cd "$(dirname "$0")/.."

port=57420
host_port=57421
db="aiops_e2e_$(date -u +%Y%m%d%H%M%S)"
work="$(mktemp -d)"
command -v brew >/dev/null && export PATH="$(brew --prefix rustup)/bin:$PATH"
unset CARGO_TARGET_DIR
# CI provides a PostgreSQL service through DATABASE_URL; locally the compose stack is used.
if [ -n "${DATABASE_URL:-}" ] && command -v psql >/dev/null; then
  psql_admin() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -qtA "$@"; }
  db_base="${DATABASE_URL%/*}"
else
  scripts/stack.sh up >/dev/null
  psql_admin() { docker compose -f deploy/compose.yaml exec -T postgres psql -U aiops -d aiops -v ON_ERROR_STOP=1 -qtA "$@"; }
  db_base="postgres://aiops:aiops-dev@127.0.0.1:${AIOPS_PG_PORT:-57432}"
fi
psql_admin -c "CREATE DATABASE $db" >/dev/null
cleanup() {
  [ -n "${host_pid:-}" ] && kill "$host_pid" 2>/dev/null
  [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null
  wait 2>/dev/null || true
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

cargo build --release --locked -p aiops-server -p aiops-reference-host --quiet
npm run build --workspace @aiops/console >/dev/null
export AIOPS_DATABASE_URL="$db_base/$db"
bin=target/release/aiops-server
"$bin" admin create-project pilot >/dev/null
"$bin" admin create-project other >/dev/null
key="$("$bin" admin create-ingest-key --project pilot --name reference-host)"
probe_key="$("$bin" admin create-ingest-key --project pilot --name e2e-probe)"
token="$("$bin" admin create-token --actor e2e --project pilot 2>/dev/null)"
other_token="$("$bin" admin create-token --actor outsider --project other 2>/dev/null)"

AIOPS_LISTEN="127.0.0.1:$port" AIOPS_CONSOLE_DIR="apps/console/build" AIOPS_LOG=warn \
  AIOPS_RETENTION_SWEEP_SECS=86400 "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
REFHOST_LISTEN="127.0.0.1:$host_port" AIOPS_ENDPOINT="http://127.0.0.1:$port" AIOPS_INGEST_KEY="$key" \
  REFHOST_ALLOW_FAULTS=1 REFHOST_LOG_FILE=/dev/null target/release/reference-host >/dev/null 2>&1 &
host_pid=$!
for _ in $(seq 1 100); do
  curl -fsS "http://127.0.0.1:$port/readyz" >/dev/null 2>&1 && curl -fsS "http://127.0.0.1:$host_port/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done

article() { curl -s -o /dev/null -X POST "http://127.0.0.1:$host_port/articles" -H 'content-type: application/json' -H "x-request-id: $1" -d "{\"topic\":\"e2e\",\"fault\":$2}"; }
article e2e-ok null
article e2e-lost '"publish_lost_response"'

now="$(python3 -c 'import datetime; print(datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ"))')"
xss_op="e2e0000000000000000000000000a551"
e3_op="e2e00000000000000000000000000e30"
gap_op="e2e0000000000000000000000000d0e5"
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/v1/ingest" -H "authorization: Bearer $probe_key" -H 'content-type: application/json' -d @- <<JSON
{"schema_version": 1,
 "source": {"instance_id": "e2e000000000000000000000000000aa", "service": "e2e-probe", "sdk": "curl"},
 "events": [
  {"event_id": "e2e00000000000000000000000000001", "type": "operation.started", "operation_id": "$xss_op", "seq": 1, "op_seq": 1,
   "event_time": "$now", "name": "publish_article", "request_id": "e2e-xss",
   "attributes": {"retry.reason": "<img src=x onerror=alert(1)>"}},
  {"event_id": "e2e00000000000000000000000000002", "type": "operation.finished", "operation_id": "$xss_op", "seq": 2, "op_seq": 2,
   "event_time": "$now", "outcome": "succeeded", "events_emitted": 2},
  {"event_id": "e2e00000000000000000000000000003", "type": "gap.reported", "operation_id": "$gap_op", "seq": 3, "op_seq": 0,
   "event_time": "$now", "dropped": 10, "reason": "send_failed", "first_dropped_at": "$now", "last_dropped_at": "$now"},
  {"event_id": "e2e00000000000000000000000000004", "type": "operation.started", "operation_id": "$e3_op", "seq": 4, "op_seq": 1,
   "event_time": "$now", "name": "publish_article", "request_id": "e2e-e3"},
  {"event_id": "e2e00000000000000000000000000006", "type": "step.finished", "operation_id": "$e3_op", "seq": 6, "op_seq": 3,
   "event_time": "$now", "step_id": "e300000000000003", "attempt": 1, "kind": "tool", "name": "publish",
   "outcome": "uncertain", "uncertain_reason": "timeout_after_send", "error_code": "publish_timeout"},
  {"event_id": "e2e00000000000000000000000000007", "type": "operation.finished", "operation_id": "$e3_op", "seq": 7, "op_seq": 4,
   "event_time": "$now", "outcome": "failed", "failure_class": "internal", "events_emitted": 4}
 ]}
JSON
sleep 1 # the SDK flushes every 200 ms

AIOPS_E2E_BASE="http://127.0.0.1:$port" AIOPS_E2E_TOKEN="$token" AIOPS_E2E_OTHER_TOKEN="$other_token" \
AIOPS_E2E_OK=e2e-ok AIOPS_E2E_LOST=e2e-lost AIOPS_E2E_E3=e2e-e3 AIOPS_E2E_XSS=e2e-xss AIOPS_E2E_GAP="$gap_op" \
  npm run test:e2e --workspace @aiops/console
