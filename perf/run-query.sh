#!/usr/bin/env bash
# Query latency over >= 1,000,000 stored events, end to end: a fresh database,
# the release collector, an ingest seeding run, then k6/query.js in STRICT mode
# (p50/p95/p99 gates per shape). The gate is the scenario's own thresholds plus
# the million-event floor — a run that stored less than the claim says so and
# fails the job.
#
#   perf/run-query.sh <name> <seed batches/s> <seed duration> [query req/s] [query duration]
#   perf/run-query.sh smoke 100 30s 20 30s          # development check
#   perf/run-query.sh million-ci 500 200s 50 300s   # ~1,000,000 events, 5 min of reads
#
# Writes perf/results/${AIOPS_RUN_NAME:-<date>-query-<name>}/ with result.md, the
# k6 summaries and the collector log.
#
# PostgreSQL: same modes as run-ingest.sh — AIOPS_PG_URL for an external server
# (CI), otherwise the local docker stack.
set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:?name}"
seed_rate="${2:?seed rate (batches per second)}"
seed_duration="${3:?seed duration (k6 format)}"
query_rate="${4:-50}"
query_duration="${5:-300s}"
date="$(date -u +%Y-%m-%d)"
out_dir="perf/results/${AIOPS_RUN_NAME:-${date}-query-${name}}"
work="$(mktemp -d)"
db="aiops_perf_$(date -u +%Y%m%d%H%M%S)"
run_id="$(openssl rand -hex 4)"

if brew_prefix="$(brew --prefix rustup 2>/dev/null)" && [ -n "$brew_prefix" ]; then
  export PATH="$brew_prefix/bin:$PATH"
fi
unset CARGO_TARGET_DIR
scripts/check-toolchain.sh >/dev/null
if [ -n "${AIOPS_PG_URL:-}" ]; then
  case "$AIOPS_PG_URL" in
    *://*/*) ;;
    *) echo "AIOPS_PG_URL must look like postgres://user:pass@host:port/db" >&2; exit 2 ;;
  esac
  pg_base="${AIOPS_PG_URL%/}"; pg_base="${pg_base%/*}"
  store_desc="PostgreSQL at ${pg_base##*@} (external, AIOPS_PG_URL)"
  if command -v psql >/dev/null 2>&1; then
    psql_admin() { psql "$AIOPS_PG_URL" -v ON_ERROR_STOP=1 -qtA "$@"; }
    psql_db() { psql "$pg_base/$db" -v ON_ERROR_STOP=1 -qtA "$@"; }
  else
    cargo build --release --locked -p aiops-store --example perf-psql >/dev/null 2>&1
    psql_admin() { target/release/examples/perf-psql "$AIOPS_PG_URL" "${@: -1}"; }
    psql_db() { target/release/examples/perf-psql "$pg_base/$db" "${@: -1}"; }
  fi
else
  scripts/stack.sh up >/dev/null
  pg_base="postgres://aiops:aiops-dev@127.0.0.1:${AIOPS_PG_PORT:-57432}"
  store_desc="PostgreSQL 18.6 (docker compose, local stack)"
  psql_admin() { docker compose -f deploy/compose.yaml exec -T postgres psql -U aiops -d aiops -v ON_ERROR_STOP=1 -qtA "$@"; }
  psql_db() { docker compose -f deploy/compose.yaml exec -T postgres psql -U aiops -d "$db" -v ON_ERROR_STOP=1 -qtA "$@"; }
fi
psql_admin -c "CREATE DATABASE $db"
cleanup() {
  { [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null && wait "$server_pid" 2>/dev/null; } || true
  mkdir -p "$out_dir"
  for f in server.log ingest.txt query.txt; do
    [ -f "$work/$f" ] && cp "$work/$f" "$out_dir/$f" || true
  done
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

if curl --version >/dev/null 2>&1; then
  probe_code() {
    local c
    if [ $# -ge 2 ]; then
      c="$(curl -sS -m 5 -o /dev/null -w '%{http_code}' -K - "$1" 2>/dev/null <<<"header = \"$2\"" || true)"
    else
      c="$(curl -sS -m 5 -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || true)"
    fi
    echo "${c:-000}"
  }
  http_get() { curl -fsS -m 5 -K - "$1" <<<"header = \"$2\"" 2>/dev/null || echo unavailable; }
else
  probe_code() {
    AIOPS_HDR="${2-}" python3 - "$1" <<'PY'
import os, sys, urllib.request, urllib.error
req = urllib.request.Request(sys.argv[1])
hdr = os.environ.get("AIOPS_HDR", "")
if hdr:
    name, _, value = hdr.partition(":")
    req.add_header(name.strip(), value.strip())
try:
    print(urllib.request.urlopen(req, timeout=5).status)
except urllib.error.HTTPError as e:
    print(e.code)
except Exception:
    print("000")
PY
  }
  http_get() {
    AIOPS_HDR="${2-}" python3 - "$1" <<'PY' 2>/dev/null || echo unavailable
import os, sys, urllib.request
req = urllib.request.Request(sys.argv[1])
hdr = os.environ.get("AIOPS_HDR", "")
if hdr:
    name, _, value = hdr.partition(":")
    req.add_header(name.strip(), value.strip())
sys.stdout.write(urllib.request.urlopen(req, timeout=5).read().decode())
PY
  }
fi

cargo build --release --locked -p aiops-server >/dev/null 2>&1
bin=target/release/aiops-server
export AIOPS_DATABASE_URL="$pg_base/$db"
"$bin" admin create-project perf >/dev/null
key="$("$bin" admin create-ingest-key --project perf --name k6)"
token="$("$bin" admin create-token --actor perf --project perf 2>/dev/null)"
admin_token="$("$bin" admin create-token --actor perf --scope admin 2>/dev/null)"

port_free() { python3 -c 'import socket,sys; s=socket.socket(); s.bind(("127.0.0.1", int(sys.argv[1])))' "$1" 2>/dev/null; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
choose_port() {
  if [ -n "${1:-}" ]; then
    for _ in $(seq 1 300); do
      port_free "$1" && { echo "$1"; return 0; }
      sleep 0.3
    done
    echo "port $1 never came free" >&2
    return 1
  fi
  free_port
}
port="$(choose_port "${AIOPS_PERF_PORT:-}")"
echo "collector on 127.0.0.1:$port" >&2

AIOPS_LISTEN="127.0.0.1:$port" \
AIOPS_INGEST_EVENTS_PER_SEC=10000000 AIOPS_INGEST_BURST_EVENTS=10000000 \
AIOPS_QUERY_REQUESTS_PER_SEC=100000 AIOPS_QUERY_BURST=100000 \
AIOPS_RETENTION_SWEEP_SECS=86400 AIOPS_LOG=warn \
  "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do
  probe_code "http://127.0.0.1:$port/readyz" | grep -qx 200 && break
  sleep 0.2
done

fail_preflight() {
  echo "PREFLIGHT FAILED: $1" >&2
  [ -f "$work/server.log" ] && { echo '--- collector log (tail) ---' >&2; tail -30 "$work/server.log" >&2; }
  exit 2
}
code="$(probe_code "http://127.0.0.1:$port/healthz")"
[ "$code" = 200 ] || fail_preflight "GET /healthz answered HTTP $code"
code="$(probe_code "http://127.0.0.1:$port/v1/collector/stats" "authorization: Bearer $admin_token")"
[ "$code" = 200 ] || fail_preflight "GET /v1/collector/stats with the admin token answered HTTP $code"

commit="$(git rev-parse --short HEAD)$(git diff --quiet || echo '-dirty')"
if [ "$(uname -s)" = "Darwin" ]; then
  if cpu_brand="$(sysctl -n machdep.cpu.brand_string 2>/dev/null)" && [ -n "$cpu_brand" ]; then
    machine="$cpu_brand, $(sysctl -n hw.ncpu) cores, $(( $(sysctl -n hw.memsize) / 1073741824 )) GB, macOS $(sw_vers -productVersion)"
  else
    machine="$(uname -sm) — sysctl unavailable here (a sandbox); CPU/RAM not recorded"
  fi
else
  cpu="$(sed -n 's/^model name[[:space:]]*:[[:space:]]*//p' /proc/cpuinfo | head -1)"
  machine="${cpu:-$(uname -m)}, $(nproc) cores, $(( $(awk '/^MemTotal/{print $2}' /proc/meminfo) / 1048576 )) GB, $(uname -srm)"
fi
machine_rows="| Machine | ${machine} |
| Collector | release build, one process, ${store_desc}, 16 ingest connections |"
venue="Development observation on a shared laptop, **not** production capacity."
if [ -n "${GITHUB_RUN_ID:-}" ]; then
  machine_rows="${machine_rows}
| CI run | ${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID} |"
  venue="Run on a GitHub-hosted Actions runner (dedicated VM, **not** production capacity)."
fi
load_before="$(uptime | sed 's/.*load averages*: //' || echo unavailable)"
k6v="$(k6 version | head -1 | awk '{print $2}')"

set +e
BASE_URL="http://127.0.0.1:$port" INGEST_KEY="$key" RUN_ID="$run_id" \
RATE="$seed_rate" DURATION="$seed_duration" OPS_PER_BATCH=1 \
k6 run --quiet --summary-export "$work/ingest.json" \
  --include-system-env-vars \
  perf/k6/ingest.js >"$work/ingest.txt" 2>&1
ingest_status=$?
set -e

stored_events="$(psql_db -c 'SELECT count(*) FROM events')"
stored_ops="$(psql_db -c 'SELECT count(*) FROM operations')"
floor_ok=PASS
[ "${stored_events:-0}" -ge 1000000 ] || floor_ok="FAIL (${stored_events} < 1,000,000)"

set +e
BASE_URL="http://127.0.0.1:$port" TOKEN="$token" RUN_ID="$run_id" PROJECT=perf \
OPERATIONS="$stored_ops" RATE="$query_rate" DURATION="$query_duration" STRICT=1 \
k6 run --quiet --summary-export "$work/query.json" \
  --include-system-env-vars \
  perf/k6/query.js >"$work/query.txt" 2>&1
query_status=$?
set -e
load_after="$(uptime | sed 's/.*load averages*: //' || echo unavailable)"
collector_stats="$(http_get "http://127.0.0.1:$port/v1/collector/stats" "authorization: Bearer $admin_token")"

mkdir -p "$out_dir"
cp "$work/ingest.json" "$out_dir/ingest.json"
cp "$work/query.json" "$out_dir/query.json"

metric() {
  python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); v=0 if v is None and sys.argv[3]=='count' else v; print('n/a' if v is None else (round(v,2) if isinstance(v,float) else v))" "$@"
}
qry="$work/query.json"
cat >"$out_dir/result.md" <<EOF
# Query latency over ${stored_events} stored events: ${name} (${date})

${venue}

| | |
|---|---|
| Commit | \`${commit}\` |
${machine_rows}
| Load average (1/5/15 min) before / after | ${load_before} / ${load_after} |
| Load generator | k6 ${k6v} on the same machine (open model: constant arrival rate) |
| Seeded | ${seed_rate} batches/s × 10 events for ${seed_duration} -> ${stored_events} events, ${stored_ops} operations |
| Query load | ${query_rate} iterations/s for ${query_duration} (STRICT mode) |
| Million-event floor | **${floor_ok}** |

## The seed (\`perf/k6/ingest.js\`, exit ${ingest_status})

| Metric | Value |
|---|---|
| Events sent | $(metric "$work/ingest.json" events_sent count) |
| Events lost (sent − stored, per answer) | $(metric "$work/ingest.json" events_lost count) |
| Batches rate-limited / shed | $(metric "$work/ingest.json" batches_rate_limited count) / $(metric "$work/ingest.json" batches_shed count) |
| Ingest p50 / p95 / p99 (ms) | $(metric "$work/ingest.json" http_req_duration med) / $(metric "$work/ingest.json" http_req_duration 'p(95)') / $(metric "$work/ingest.json" http_req_duration 'p(99)') |

## The reads (\`perf/k6/query.js\` STRICT, exit ${query_status})

| Shape | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| timeline | $(metric "$qry" 'http_req_duration{name:timeline}' med) | $(metric "$qry" 'http_req_duration{name:timeline}' 'p(95)') | $(metric "$qry" 'http_req_duration{name:timeline}' 'p(99)') | $(metric "$qry" 'http_req_duration{name:timeline}' max) |
| list-by-request | $(metric "$qry" 'http_req_duration{name:list}' med) | $(metric "$qry" 'http_req_duration{name:list}' 'p(95)') | $(metric "$qry" 'http_req_duration{name:list}' 'p(99)') | $(metric "$qry" 'http_req_duration{name:list}' max) |
| filtered list (status+release) | $(metric "$qry" 'http_req_duration{name:list_filtered}' med) | $(metric "$qry" 'http_req_duration{name:list_filtered}' 'p(95)') | $(metric "$qry" 'http_req_duration{name:list_filtered}' 'p(99)') | $(metric "$qry" 'http_req_duration{name:list_filtered}' max) |
| release comparison | $(metric "$qry" 'http_req_duration{name:compare}' med) | $(metric "$qry" 'http_req_duration{name:compare}' 'p(95)') | $(metric "$qry" 'http_req_duration{name:compare}' 'p(99)') | $(metric "$qry" 'http_req_duration{name:compare}' max) |

| Metric | Value |
|---|---|
| Iterations | $(metric "$qry" iterations count) |
| Checks passed (rate) | $(metric "$qry" checks value) |
| HTTP failures (rate) | $(metric "$qry" http_req_failed value) |
| Collector counters | \`${collector_stats}\` |

## First failures (verbatim — status 0 means the request never left k6)

\`\`\`
$(grep 'AIOPS_FIRST_FAILURE ' "$work/ingest.txt" "$work/query.txt" | head -10 | sed 's/^[^:]*:AIOPS_FIRST_FAILURE //' || true)
\`\`\`

## Threshold verdicts

\`\`\`
$(grep -E '✓|✗' "$work/ingest.txt" | sed 's/^ *//' || true)
$(grep -E '✓|✗' "$work/query.txt" | sed 's/^ *//' || true)
\`\`\`
EOF
echo "wrote $out_dir/result.md (ingest $ingest_status, query $query_status, floor ${floor_ok})"
if [ "$ingest_status" -ne 0 ] || [ "$query_status" -ne 0 ] || [ "$floor_ok" != PASS ]; then
  exit 1
fi
