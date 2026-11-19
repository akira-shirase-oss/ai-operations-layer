#!/usr/bin/env bash
# One recorded ingestion run (and a query run over what it stored), end to end:
# a fresh database in the local stack, the release collector, k6 at a fixed offered load,
# then the database is counted so loss cannot hide behind a latency chart.
#
#   perf/run-ingest.sh <name> <rate batches/s> <duration> [ops per batch]
#   perf/run-ingest.sh smoke 100 30s 1          # the guide's reference shape, short
#   perf/run-ingest.sh million 500 200s 1       # 1,000,000 events at 5x the reference rate
#
# Writes perf/results/<date>-ingest-<name>.md (+ the k6 summaries as JSON). The result
# carries the machine, the commit, the load average before and after, and every miss.
#
# PostgreSQL: by default the local docker stack (scripts/stack.sh up; psql through
# docker compose exec, AIOPS_PG_PORT selects its port). Set AIOPS_PG_URL to an
# external server's maintenance database instead — postgres://user:pass@host:port/db —
# and psql(1) talks to it directly; nothing docker is needed (CI's postgres service).
# Where psql is out of reach the store crate's perf-psql example stands in.
set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:?name}"
rate="${2:?rate (batches per second)}"
duration="${3:?duration (k6 format, e.g. 60s)}"
ops_per_batch="${4:-1}"
port="${AIOPS_PERF_PORT:-57410}"
date="$(date -u +%Y-%m-%d)"
out="perf/results/${date}-ingest-${name}"
work="$(mktemp -d)"
db="aiops_perf_$(date -u +%Y%m%d%H%M%S)"
run_id="$(openssl rand -hex 4)"

# Prefer rustup's cargo (the pin) when brew can name it; a brew that resolves but
# cannot run (a sandbox, a broken install) must not abort the run.
if brew_prefix="$(brew --prefix rustup 2>/dev/null)" && [ -n "$brew_prefix" ]; then
  export PATH="$brew_prefix/bin:$PATH"
fi
unset CARGO_TARGET_DIR
scripts/check-toolchain.sh >/dev/null
if [ -n "${AIOPS_PG_URL:-}" ]; then
  # External PostgreSQL (CI): no docker; the URL names the maintenance database and the
  # run's databases live beside it.
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
    # No client on this machine (a sandbox where the stack answers on TCP but docker
    # is out of reach): the store crate's perf-psql example does the same one-statement job.
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
  [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null && wait "$server_pid" 2>/dev/null
  # The logs outlive the work dir — a failed run must not delete its own evidence.
  mkdir -p perf/results
  [ -f "$work/server.log" ] && cp "$work/server.log" "${out}.server.log" || true
  [ -f "$work/ingest.txt" ] && cp "$work/ingest.txt" "${out}.k6.txt" || true
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

# One GET returning just the HTTP status (000 on transport failure): the
# pre-flight needs the verdict's code, not the body. python3 stand-in where
# curl is out of reach — the same pattern as perf-psql.
if curl --version >/dev/null 2>&1; then
  probe_code() { curl -sS -m 5 -o /dev/null -w '%{http_code}' ${2:+-H "$2"} "$1" 2>/dev/null || echo 000; }
else
  probe_code() {
    python3 - "$1" "$2" <<'PY'
import sys, urllib.request, urllib.error
req = urllib.request.Request(sys.argv[1])
if sys.argv[2]:
    req.add_header("authorization", sys.argv[2])
try:
    print(urllib.request.urlopen(req, timeout=5).status)
except urllib.error.HTTPError as e:
    print(e.code)
except Exception:
    print("000")
PY
  }
fi

cargo build --release --locked -p aiops-server >/dev/null 2>&1
bin=target/release/aiops-server
export AIOPS_DATABASE_URL="$pg_base/$db"
"$bin" admin create-project perf >/dev/null
key="$("$bin" admin create-ingest-key --project perf --name k6)"
token="$("$bin" admin create-token --actor perf --project perf 2>/dev/null)"
# The collector-wide counters need the admin scope (67bc0de); the query run keeps a read token.
admin_token="$("$bin" admin create-token --actor perf --project perf --scope admin 2>/dev/null)"

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

# Pre-flight (the control run's CI 44544883621 burned an hour against a dead
# listener): the loop above only waits — verify. /healthz answers, then one
# authenticated call proves auth+pool+project before a million events move.
fail_preflight() {
  echo "PREFLIGHT FAILED: $1" >&2
  [ -f "$work/server.log" ] && { echo '--- collector log (tail) ---' >&2; tail -30 "$work/server.log" >&2; }
  exit 2
}
code="$(probe_code "http://127.0.0.1:$port/healthz")"
[ "$code" = 200 ] || fail_preflight "GET /healthz answered HTTP $code — the collector is not up; load would only see connection-refused."
code="$(probe_code "http://127.0.0.1:$port/v1/collector/stats" "authorization: Bearer $admin_token")"
[ "$code" = 200 ] || fail_preflight "GET /v1/collector/stats with the minted admin token answered HTTP $code (expected 200)."

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
load_before="$(uptime | sed 's/.*load averages*: //')"
k6v="$(k6 version | head -1 | awk '{print $2}')"

set +e
k6 run --quiet --summary-export "$work/ingest.json" \
  -e BASE_URL="http://127.0.0.1:$port" -e INGEST_KEY="$key" -e RUN_ID="$run_id" \
  -e RATE="$rate" -e DURATION="$duration" -e OPS_PER_BATCH="$ops_per_batch" \
  perf/k6/ingest.js >"$work/ingest.txt" 2>&1
ingest_status=$?
set -e
load_after_ingest="$(uptime | sed 's/.*load averages*: //')"

stored_events="$(psql_db -c 'SELECT count(*) FROM events')"
stored_ops="$(psql_db -c 'SELECT count(*) FROM operations')"
incomplete_ops="$(psql_db -c 'SELECT count(*) FROM operations WHERE host_events <> 10 OR finish_outcome IS NULL')"
events_bytes="$(psql_db -c "SELECT pg_total_relation_size('events')")"
ops_bytes="$(psql_db -c "SELECT pg_total_relation_size('operations')")"
db_bytes="$(psql_db -c "SELECT pg_database_size('$db')")"

set +e
k6 run --quiet --summary-export "$work/query.json" \
  -e BASE_URL="http://127.0.0.1:$port" -e TOKEN="$token" -e RUN_ID="$run_id" -e PROJECT=perf \
  -e OPERATIONS="$stored_ops" -e RATE=50 -e DURATION=30s \
  perf/k6/query.js >"$work/query.txt" 2>&1
query_status=$?
set -e
load_after="$(uptime | sed 's/.*load averages*: //')"
# Post-run probes degrade, never abort: a denied `ps` or `curl` (a sandbox) must
# not kill the script through `set -o pipefail` the way it did on the first smoke.
rss_kb="$(ps -o rss= -p "$server_pid" 2>/dev/null | tr -d ' ' || true)"
collector_stats="$(curl -fsS -H "authorization: Bearer $admin_token" "http://127.0.0.1:$port/v1/collector/stats" 2>/dev/null || echo unavailable)"

mkdir -p perf/results
cp "$work/ingest.json" "$out.ingest.json"
cp "$work/query.json" "$out.query.json"

metric() { # file metric field
  # A counter k6 never incremented is absent from the summary: that is a count of 0.
  python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); v=0 if v is None and sys.argv[3]=='count' else v; print('n/a' if v is None else (round(v,2) if isinstance(v,float) else v))" "$@"
}
ing="$work/ingest.json"; qry="$work/query.json"
cat >"$out.md" <<EOF
# Ingestion run: ${name} (${date})

${venue}

| | |
|---|---|
| Commit | \`${commit}\` |
${machine_rows}
| Load average (1/5/15 min) before | ${load_before} |
| … after ingest / after query | ${load_after_ingest} / ${load_after} |
| Load generator | k6 ${k6v} on the same machine (open model: constant arrival rate) |
| Offered load | ${rate} batches/s × ${ops_per_batch} operation(s) × 10 events for ${duration} |

## Ingest (\`perf/k6/ingest.js\`, exit ${ingest_status})

| Metric | Value |
|---|---|
| Batches (iterations) | $(metric "$ing" iterations count) |
| Events sent | $(metric "$ing" events_sent count) |
| Events accepted (k6 count) | $(metric "$ing" events_accepted count) |
| **Events lost** (sent − stored, per answer) | $(metric "$ing" events_lost count) |
| Batches rate-limited / shed | $(metric "$ing" batches_rate_limited count) / $(metric "$ing" batches_shed count) |
| Dropped iterations (k6 could not offer the load) | $(metric "$ing" dropped_iterations count) |
| HTTP failures (rate) | $(metric "$ing" http_req_failed value) |
| Latency p50 / p95 / p99 / max (ms) | $(metric "$ing" http_req_duration med) / $(metric "$ing" http_req_duration 'p(95)') / $(metric "$ing" http_req_duration 'p(99)') / $(metric "$ing" http_req_duration max) |
| Achieved request rate (/s) | $(metric "$ing" http_reqs rate) |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | ${stored_events} |
| Operations stored | ${stored_ops} |
| Operations not complete (≠ 10 events or no finish) | ${incomplete_ops} |
| \`events\` table + indexes | ${events_bytes} bytes ($(python3 -c "print(round(${events_bytes}/max(${stored_events},1)))") bytes/event) |
| \`operations\` table + indexes | ${ops_bytes} bytes |
| Database total | ${db_bytes} bytes |
| Collector RSS after the runs | ${rss_kb:-unavailable (process tools denied here)}${rss_kb:+ KiB} |
| Collector counters | \`${collector_stats}\` |

## Query over what was stored (\`perf/k6/query.js\`, exit ${query_status}, 50 iterations/s for 30 s)

| Metric | Value |
|---|---|
| Timeline p50 / p95 / p99 (ms) | $(metric "$qry" 'http_req_duration{name:timeline}' med) / $(metric "$qry" 'http_req_duration{name:timeline}' 'p(95)') / $(metric "$qry" 'http_req_duration{name:timeline}' 'p(99)') |
| List-by-request p50 / p95 / p99 (ms) | $(metric "$qry" 'http_req_duration{name:list}' med) / $(metric "$qry" 'http_req_duration{name:list}' 'p(95)') / $(metric "$qry" 'http_req_duration{name:list}' 'p(99)') |
| Checks passed (rate) | $(metric "$qry" checks value) |
| HTTP failures (rate) | $(metric "$qry" http_req_failed value) |

## First failures (verbatim — status 0 means the request never left k6)

\`\`\`
$(grep 'AIOPS_FIRST_FAILURE ' "$work/ingest.txt" | head -10 | sed 's/^AIOPS_FIRST_FAILURE //' || true)
\`\`\`

## Threshold verdicts

\`\`\`
$(grep -E '✓|✗' "$work/ingest.txt" | sed 's/^ *//' || true)
$(grep -E '✓|✗' "$work/query.txt" | sed 's/^ *//' || true)
\`\`\`
EOF
echo "wrote $out.md (ingest exit $ingest_status, query exit $query_status)"
# A threshold miss fails the workflow (k6 exits nonzero); the artifacts still upload.
if [ "$ingest_status" -ne 0 ]; then exit "$ingest_status"; fi
exit "$query_status"
