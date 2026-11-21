#!/usr/bin/env bash
# Collector loss at million scale, end to end: a fresh database, the release
# collector, k6 at the reference shape — then the collector is SIGKILLed
# mid-run and restarted on the same database. Every sent event must land in
# exactly one bucket (stored / refused / limited / shed / unreachable /
# server_error); the database's own row count is reconciled against the
# answers exactly. Loss inside the outage window is real — it is counted as
# `unreachable`, never hidden.
#
#   perf/run-loss.sh <name> <batches/s> <duration>
#   perf/run-loss.sh smoke 100 60s          # development check
#   perf/run-loss.sh million-ci 500 200s    # ~1,000,000 events, killed at ~40%
#
# Writes perf/results/${AIOPS_RUN_NAME:-<date>-loss-<name>}/ with result.md,
# the k6 summary, both collector logs and the restart timing.
#
# PostgreSQL: same modes as run-ingest.sh — AIOPS_PG_URL for an external server
# (CI), otherwise the local docker stack.
set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:?name}"
rate="${2:?rate (batches per second)}"
duration="${3:?duration (k6 format, e.g. 200s)}"
date="$(date -u +%Y-%m-%d)"
out_dir="perf/results/${AIOPS_RUN_NAME:-${date}-loss-${name}}"
work="$(mktemp -d)"
db="aiops_perf_$(date -u +%Y%m%d%H%M%S)"
run_id="$(openssl rand -hex 4)"

# Duration to seconds (k6 'Ns' or 'Nm'); the kill fires at 40% of the run.
dur_s="$(python3 -c "import re,sys; m=re.fullmatch(r'(\d+)([sm])', sys.argv[1]); v=int(m.group(1)); print(v*60 if m.group(2)=='m' else v)" "$duration")"
kill_after=$(( dur_s * 40 / 100 ))

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
  for f in server-1.log server-2.log k6.txt; do
    [ -f "$work/$f" ] && cp "$work/$f" "$out_dir/$f" || true
  done
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

if curl --version >/dev/null 2>&1; then
  probe_code() {
    curl -sS -m 5 -o /dev/null -w '%{http_code}' "$1" 2>/dev/null || echo 000
  }
else
  probe_code() {
    python3 - "$1" <<'PY'
import sys, urllib.request, urllib.error
try:
    print(urllib.request.urlopen(sys.argv[1], timeout=5).status)
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

port_free() { python3 -c 'import socket,sys; s=socket.socket(); s.bind(("127.0.0.1", int(sys.argv[1])))' "$1" 2>/dev/null; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
port="$(free_port)"
echo "collector on 127.0.0.1:$port (SIGKILL after ${kill_after}s of ${dur_s}s)" >&2

serve_env() {
  AIOPS_LISTEN="127.0.0.1:$port" \
  AIOPS_INGEST_EVENTS_PER_SEC=10000000 AIOPS_INGEST_BURST_EVENTS=10000000 \
  AIOPS_QUERY_REQUESTS_PER_SEC=100000 AIOPS_QUERY_BURST=100000 \
  AIOPS_RETENTION_SWEEP_SECS=86400 AIOPS_LOG=warn \
    "$bin" serve
}
serve_env >"$work/server-1.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 150); do
  probe_code "http://127.0.0.1:$port/readyz" | grep -qx 200 && break
  sleep 0.2
done
code="$(probe_code "http://127.0.0.1:$port/healthz")"
[ "$code" = 200 ] || { echo "PREFLIGHT FAILED: /healthz answered $code" >&2; tail -30 "$work/server-1.log" >&2; exit 2; }

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
| Collector | release build, two lifetimes (SIGKILL + restart), ${store_desc}, 16 ingest connections |"
venue="Development observation on a shared laptop, **not** production capacity."
if [ -n "${GITHUB_RUN_ID:-}" ]; then
  machine_rows="${machine_rows}
| CI run | ${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID} |"
  venue="Run on a GitHub-hosted Actions runner (dedicated VM, **not** production capacity)."
fi
load_before="$(uptime | sed 's/.*load averages*: //' || echo unavailable)"
k6v="$(k6 version | head -1 | awk '{print $2}')"

# The run and the kill overlap: k6 in the background, the orchestrator sleeps to
# the kill point, SIGKILLs, waits a beat, restarts, and waits for the run.
BASE_URL="http://127.0.0.1:$port" INGEST_KEY="$key" RUN_ID="$run_id" \
RATE="$rate" DURATION="$duration" OPS_PER_BATCH=1 \
k6 run --quiet --summary-export "$work/k6.json" \
  --include-system-env-vars \
  perf/k6/loss.js >"$work/k6.txt" 2>&1 &
k6_pid=$!

sleep "$kill_after"
kill -9 "$server_pid" 2>/dev/null || true
wait "$server_pid" 2>/dev/null || true
killed_at="$(python3 -c 'import time; print(time.time())')"
echo "collector killed at ${kill_after}s" >&2
sleep 3

serve_env >"$work/server-2.log" 2>&1 &
server_pid=$!
restart_s="never"
for i in $(seq 1 300); do
  if probe_code "http://127.0.0.1:$port/readyz" | grep -qx 200; then
    restart_s="$(python3 -c "import time; print(round(time.time() - float('$killed_at'), 1))")"
    break
  fi
  sleep 0.2
done
echo "collector back after ${restart_s}s" >&2

wait "$k6_pid" || k6_status=$?
k6_status="${k6_status:-0}"
load_after="$(uptime | sed 's/.*load averages*: //' || echo unavailable)"

stored_events="$(psql_db -c 'SELECT count(*) FROM events')"
stored_ops="$(psql_db -c 'SELECT count(*) FROM operations')"

mkdir -p "$out_dir"
cp "$work/k6.json" "$out_dir/k6.json"

metric() {
  python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); v=0 if v is None and sys.argv[3]=='count' else v; print('n/a' if v is None else (round(v,2) if isinstance(v,float) else v))" "$@"
}
k6f="$work/k6.json"
# Exact reconciliation: events the answers counted as stored must equal the rows
# the database holds. (A status-0 batch is all-or-nothing: the insert is one
# transaction, so a batch that died after commit would show db > answers — that
# delta is printed, not assumed zero.)
reconcile="$(python3 - "$(metric "$k6f" events_stored count)" "$stored_events" <<'PY'
import sys
ack, db = sys.argv[1], sys.argv[2]
try:
    a, d = int(float(ack)), int(db)
except ValueError:
    print("FAIL (counters unreadable)"); sys.exit()
if a == d:
    print("PASS")
elif d > a:
    print(f"NOTE ({d - a} events stored but unacknowledged — commits that outlived their replies)")
else:
    print(f"FAIL (answers counted {a} stored but the database holds {d})")
PY
)"
cat >"$out_dir/result.md" <<EOF
# Collector-loss run: ${name} (${date})

${venue}

| | |
|---|---|
| Commit | \`${commit}\` |
${machine_rows}
| Load average (1/5/15 min) before / after | ${load_before} / ${load_after} |
| Load generator | k6 ${k6v} on the same machine (open model: constant arrival rate) |
| Offered load | ${rate} batches/s × 10 events for ${duration} |
| The fault | SIGKILL at ${kill_after}s (${dur_s}s run); restarted on the same database, ready again after ${restart_s}s |

## Every event in exactly one bucket (\`perf/k6/loss.js\`, exit ${k6_status})

| Bucket | Events |
|---|---|
| sent | $(metric "$k6f" events_sent count) |
| stored (200 answers) | $(metric "$k6f" events_stored count) |
| refused (inside 200s) | $(metric "$k6f" events_refused count) |
| limited (429) | $(metric "$k6f" events_limited count) |
| shed (503) | $(metric "$k6f" events_shed count) |
| unreachable (status 0 after retry — the outage's share) | $(metric "$k6f" events_unreachable count) |
| in non-503 5xx | $(metric "$k6f" events_server_error count) |
| **unaccounted** | **$(metric "$k6f" events_unaccounted count)** |
| Batches retried after no reply | $(metric "$k6f" batches_retried count) |
| HTTP failures (rate — includes the outage's connection refusals, disclosed) | $(metric "$k6f" http_req_failed value) |
| Ingest p50 / p95 / p99 (ms) | $(metric "$k6f" http_req_duration med) / $(metric "$k6f" http_req_duration 'p(95)') / $(metric "$k6f" http_req_duration 'p(99)') |

## Reconciliation (counted, not inferred)

| | |
|---|---|
| Events the database holds | ${stored_events} |
| Operations the database holds | ${stored_ops} |
| stored-per-answers == rows held | **${reconcile}** |

## First failures (verbatim)

\`\`\`
$(grep 'AIOPS_FIRST_FAILURE ' "$work/k6.txt" | head -10 | sed 's/^AIOPS_FIRST_FAILURE //' || true)
\`\`\`

## Threshold verdicts

\`\`\`
$(grep -E '✓|✗' "$work/k6.txt" | sed 's/^ *//' || true)
\`\`\`
EOF
echo "wrote $out_dir/result.md (k6 exit $k6_status; restart ${restart_s}s; reconcile ${reconcile})"
if [ "$k6_status" -ne 0 ] || [ "${reconcile%% *}" = FAIL ] || [ "$restart_s" = never ]; then exit 1; fi
