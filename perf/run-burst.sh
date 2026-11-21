#!/usr/bin/env bash
# Bounded behavior under a burst, end to end: a fresh database, the release
# collector with deliberately modest limits (so the spike provably crosses
# them), and k6's staged burst. The collector's answers are counted per class
# and the database is reconciled afterwards: no 5xx, no unaccounted event, the
# post-spike window fully recovered.
#
#   perf/run-burst.sh <name>
#   perf/run-burst.sh smoke          # development check
#   perf/run-burst.sh million-ci     # the CI dispatch
#
# Configured limits for the run (printed in the result):
#   AIOPS_INGEST_EVENTS_PER_SEC=4000   (400 batches/s of 10 events)
#   AIOPS_INGEST_BURST_EVENTS=12000    (a ~3 s burst allowance)
#   AIOPS_INGEST_MAX_IN_FLIGHT=16      (a spike also trips the shed bound)
# The scenario defaults to a 1500 batches/s spike — several times the bucket.
#
# Writes perf/results/${AIOPS_RUN_NAME:-<date>-burst-<name>}/ with result.md,
# the k6 summary and the collector log.
#
# PostgreSQL: same modes as run-ingest.sh — AIOPS_PG_URL for an external server
# (CI), otherwise the local docker stack.
set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:?name}"
date="$(date -u +%Y-%m-%d)"
out_dir="perf/results/${AIOPS_RUN_NAME:-${date}-burst-${name}}"
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
  for f in server.log k6.txt; do
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

# Modest configured limits: the spike must cross them on purpose.
AIOPS_LISTEN="127.0.0.1:$port" \
AIOPS_INGEST_EVENTS_PER_SEC=4000 AIOPS_INGEST_BURST_EVENTS=12000 \
AIOPS_INGEST_MAX_IN_FLIGHT=16 \
AIOPS_RETENTION_SWEEP_SECS=86400 AIOPS_LOG=warn \
  "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do
  probe_code "http://127.0.0.1:$port/readyz" | grep -qx 200 && break
  sleep 0.2
done
code="$(probe_code "http://127.0.0.1:$port/healthz")"
[ "$code" = 200 ] || { echo "PREFLIGHT FAILED: /healthz answered $code" >&2; tail -30 "$work/server.log" >&2; exit 2; }

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
BASELINE_RATE="${BASELINE_RATE:-100}" SPIKE_RATE="${SPIKE_RATE:-1500}" \
WARM_S="${WARM_S:-30}" HOLD_S="${HOLD_S:-60}" SPIKE_HOLD_S="${SPIKE_HOLD_S:-30}" \
RECOVER_S="${RECOVER_S:-60}" \
k6 run --quiet --summary-export "$work/k6.json" \
  --include-system-env-vars \
  perf/k6/burst.js >"$work/k6.txt" 2>&1
k6_status=$?
set -e
load_after="$(uptime | sed 's/.*load averages*: //' || echo unavailable)"

stored_events="$(psql_db -c 'SELECT count(*) FROM events')"

mkdir -p "$out_dir"
cp "$work/k6.json" "$out_dir/k6.json"

metric() {
  python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); v=0 if v is None and sys.argv[3]=='count' else v; print('n/a' if v is None else (round(v,2) if isinstance(v,float) else v))" "$@"
}
k6f="$work/k6.json"
# The accounting identity, reconciled against the database's own count:
# stored(per answers) must equal the rows the store actually holds.
reconcile="$(python3 - "$(metric "$k6f" events_stored count)" "$stored_events" <<'PY'
import sys
ack, db = sys.argv[1], sys.argv[2]
try:
    print("PASS" if int(float(ack)) == int(db) else f"FAIL (answers counted {ack} stored, the database holds {db})")
except ValueError:
    print("FAIL (counters unreadable)")
PY
)"
cat >"$out_dir/result.md" <<EOF
# Burst run: ${name} (${date})

${venue}

| | |
|---|---|
| Commit | \`${commit}\` |
${machine_rows}
| Load average (1/5/15 min) before / after | ${load_before} / ${load_after} |
| Load generator | k6 ${k6v} on the same machine (ramping arrival rate) |
| Offered load | ${BASELINE_RATE:-100} batches/s baseline, ${SPIKE_RATE:-1500}/s spike (${SPIKE_HOLD_S:-30}s), ${RECOVER_S:-60}s recovery |
| Configured limits | 4,000 events/s + 12,000 burst per key; 16 in-flight |

## The answers (\`perf/k6/burst.js\`, exit ${k6_status})

| Metric | Value |
|---|---|
| Events sent | $(metric "$k6f" events_sent count) |
| Events stored (200 answers) | $(metric "$k6f" events_stored count) |
| Events refused (inside 200s) | $(metric "$k6f" events_refused count) |
| Events rate-limited (429) | $(metric "$k6f" events_limited count) |
| Events shed (503) | $(metric "$k6f" events_shed count) |
| Events unreachable (status 0 after retry) | $(metric "$k6f" events_unreachable count) |
| Events in non-503 5xx | $(metric "$k6f" events_server_error count) |
| **Events unaccounted** | **$(metric "$k6f" events_unaccounted count)** |
| Batches bounded (429+503 — proof the spike crossed the limit) | $(metric "$k6f" batches_bounded count) |
| Recovery-window non-200 answers | $(metric "$k6f" recovered_failed count) |
| Ingest p50 / p95 / p99 (ms) | $(metric "$k6f" http_req_duration med) / $(metric "$k6f" http_req_duration 'p(95)') / $(metric "$k6f" http_req_duration 'p(99)') |

## Reconciliation (counted, not inferred)

| | |
|---|---|
| Events the database holds | ${stored_events} |
| Stored-per-answers == rows held | **${reconcile}** |

## First failures (verbatim)

\`\`\`
$(grep 'AIOPS_FIRST_FAILURE ' "$work/k6.txt" | head -10 | sed 's/^AIOPS_FIRST_FAILURE //' || true)
\`\`\`

## Threshold verdicts

\`\`\`
$(grep -E '✓|✗' "$work/k6.txt" | sed 's/^ *//' || true)
\`\`\`
EOF
echo "wrote $out_dir/result.md (k6 exit $k6_status; reconcile ${reconcile})"
if [ "$k6_status" -ne 0 ] || [ "${reconcile%% *}" != PASS ]; then exit 1; fi
