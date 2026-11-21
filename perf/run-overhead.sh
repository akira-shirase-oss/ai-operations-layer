#!/usr/bin/env bash
# Instrumentation-overhead run, end to end: a fresh database, the release
# collector, two reference hosts that differ only in their SDK client (one
# disabled, one live), and k6 driving the same POST /articles workload at both.
# The gate is the cross-leg median delta, under the evidence plan's proposed
# budget (docs/END_GOAL.md): (med_on - med_off) / med_off < OVERHEAD_BUDGET_PCT
# percent (default 5). SDK drop counters are disclosed from the host's
# /aiops/stats; any drop at this rate is itself a finding and fails the run.
#
#   perf/run-overhead.sh <name> <req/s per leg> <duration>
#   perf/run-overhead.sh smoke 20 60s        # development check
#   perf/run-overhead.sh million-ci 20 300s  # the CI dispatch
#
# Writes perf/results/${AIOPS_RUN_NAME:-<date>-overhead-<name>}/ with result.md,
# the k6 summary JSON, and the server/host logs.
#
# PostgreSQL: same modes as run-ingest.sh — AIOPS_PG_URL for an external server
# (CI), otherwise the local docker stack.
set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:?name}"
rate="${2:?rate (requests per second per leg)}"
duration="${3:?duration (k6 format, e.g. 300s)}"
budget="${OVERHEAD_BUDGET_PCT:-5}"
date="$(date -u +%Y-%m-%d)"
out_dir="perf/results/${AIOPS_RUN_NAME:-${date}-overhead-${name}}"
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
  for pid in ${server_pid:-} ${host_bare_pid:-} ${host_on_pid:-}; do
    { [ -n "$pid" ] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null; } || true
  done
  mkdir -p "$out_dir"
  [ -f "$work/server.log" ] && cp "$work/server.log" "$out_dir/server.log" || true
  [ -f "$work/host-bare.log" ] && cp "$work/host-bare.log" "$out_dir/host-bare.log" || true
  [ -f "$work/host-on.log" ] && cp "$work/host-on.log" "$out_dir/host-on.log" || true
  [ -f "$work/k6.txt" ] && cp "$work/k6.txt" "$out_dir/k6.txt" || true
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
  http_get() { curl -fsS -m 5 "$1" 2>/dev/null || echo unavailable; }
  post_code() {
    curl -sS -m 10 -o /dev/null -w '%{http_code}' -X POST "$1/articles" \
      -H 'content-type: application/json' -d '{"topic":"preflight"}' 2>/dev/null || echo 000
  }
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
    python3 - "$1" <<'PY' 2>/dev/null || echo unavailable
import sys, urllib.request
sys.stdout.write(urllib.request.urlopen(sys.argv[1], timeout=5).read().decode())
PY
  }
  post_code() {
    python3 - "$1" <<'PY'
import sys, urllib.request, urllib.error
req = urllib.request.Request(sys.argv[1] + "/articles", data=b'{"topic":"preflight"}',
                             headers={"content-type": "application/json"}, method="POST")
try:
    print(urllib.request.urlopen(req, timeout=10).status)
except urllib.error.HTTPError as e:
    print(e.code)
except Exception:
    print("000")
PY
  }
fi

cargo build --release --locked -p aiops-server -p aiops-reference-host >/dev/null 2>&1
bin=target/release/aiops-server
refhost=target/release/reference-host
export AIOPS_DATABASE_URL="$pg_base/$db"
"$bin" admin create-project perf >/dev/null
key="$("$bin" admin create-ingest-key --project perf --name k6)"
admin_token="$("$bin" admin create-token --actor perf --scope admin 2>/dev/null)"

port_free() { python3 -c 'import socket,sys; s=socket.socket(); s.bind(("127.0.0.1", int(sys.argv[1])))' "$1" 2>/dev/null; }
free_port() { python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1", 0)); print(s.getsockname()[1])'; }
cport="$(free_port)"; hport_bare="$(free_port)"; hport_on="$(free_port)"
echo "collector 127.0.0.1:$cport, hosts :$hport_bare (bare) :$hport_on (instrumented)" >&2

AIOPS_LISTEN="127.0.0.1:$cport" \
AIOPS_INGEST_EVENTS_PER_SEC=10000000 AIOPS_INGEST_BURST_EVENTS=10000000 \
AIOPS_QUERY_REQUESTS_PER_SEC=100000 AIOPS_QUERY_BURST=100000 \
AIOPS_RETENTION_SWEEP_SECS=86400 AIOPS_LOG=warn \
  "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!

# The legs differ only in these two variables — Client::disabled() when unset.
REFHOST_LISTEN="127.0.0.1:$hport_bare" "$refhost" >"$work/host-bare.log" 2>&1 &
host_bare_pid=$!
REFHOST_LISTEN="127.0.0.1:$hport_on" \
  AIOPS_ENDPOINT="http://127.0.0.1:$cport" AIOPS_INGEST_KEY="$key" \
  "$refhost" >"$work/host-on.log" 2>&1 &
host_on_pid=$!

for _ in $(seq 1 150); do
  a="$(probe_code "http://127.0.0.1:$cport/readyz")"
  b="$(probe_code "http://127.0.0.1:$hport_bare/healthz")"
  c="$(probe_code "http://127.0.0.1:$hport_on/healthz")"
  [ "$a" = 200 ] && [ "$b" = 200 ] && [ "$c" = 200 ] && break
  sleep 0.2
done

fail_preflight() {
  echo "PREFLIGHT FAILED: $1" >&2
  for f in server.log host-bare.log host-on.log; do
    [ -f "$work/$f" ] && { echo "--- $f (tail) ---" >&2; tail -15 "$work/$f" >&2; }
  done
  exit 2
}
code="$(probe_code "http://127.0.0.1:$cport/healthz")"
[ "$code" = 200 ] || fail_preflight "collector /healthz answered $code"
code="$(post_code "http://127.0.0.1:$hport_bare")"
[ "$code" = 201 ] || fail_preflight "bare host POST /articles answered $code (expected 201)"
code="$(post_code "http://127.0.0.1:$hport_on")"
[ "$code" = 201 ] || fail_preflight "instrumented host POST /articles answered $code (expected 201)"

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
venue="Development observation on a shared laptop, **not** production capacity."
machine_rows="| Machine | ${machine} |
| Collector | release build, one process, ${store_desc}, 16 ingest connections |"
if [ -n "${GITHUB_RUN_ID:-}" ]; then
  machine_rows="${machine_rows}
| CI run | ${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID} |"
  venue="Run on a GitHub-hosted Actions runner (dedicated VM, **not** production capacity)."
fi
load_before="$(uptime | sed 's/.*load averages*: //' || echo unavailable)"
k6v="$(k6 version | head -1 | awk '{print $2}')"

set +e
HOST_BARE="http://127.0.0.1:$hport_bare" HOST_ON="http://127.0.0.1:$hport_on" \
RUN_ID="$run_id" RATE="$rate" DURATION="$duration" \
k6 run --quiet --summary-export "$work/k6.json" \
  --include-system-env-vars \
  perf/k6/overhead.js >"$work/k6.txt" 2>&1
k6_status=$?
set -e
load_after="$(uptime | sed 's/.*load averages*: //' || echo unavailable)"

# The instrumented leg's telemetry must have actually landed, and its SDK's own
# counters disclose every drop — an overhead figure taken while dropping data
# silently is exactly the trade the evidence plan forbids.
sdk_stats="$(http_get "http://127.0.0.1:$hport_on/aiops/stats")"
sdk_bare="$(http_get "http://127.0.0.1:$hport_bare/aiops/stats")"
stored_events="$(psql_db -c 'SELECT count(*) FROM events' 2>/dev/null || echo 0)"

metric() {
  python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); v=0 if v is None and sys.argv[3]=='count' else v; print('n/a' if v is None else (round(v,2) if isinstance(v,float) else v))" "$@"
}
raw() { python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); print('' if v is None else v)" "$@"; }
med_bare="$(raw "$work/k6.json" 'http_req_duration{scenario:bare}' med)"
med_on="$(raw "$work/k6.json" 'http_req_duration{scenario:instrumented}' med)"
dropped="$(python3 -c "import json,sys; print((json.loads(sys.argv[1])).get('dropped','n/a'))" "$sdk_stats" 2>/dev/null || echo n/a)"

# The cross-leg gate, in one place so the printed verdict is the tested one.
verdict="$(python3 - "$med_bare" "$med_on" "$budget" <<'PY'
import sys
try:
    bare, on, budget = float(sys.argv[1]), float(sys.argv[2]), float(sys.argv[3])
except (ValueError, IndexError):
    print('FAIL|n/a')
    sys.exit()
delta = on - bare
pct = delta / bare * 100 if bare else float('nan')
ok = delta <= bare * budget / 100
print(f'{"PASS" if ok else "FAIL"}|{delta:.2f} ms ({pct:.2f}%)')
PY
)"
gate="${verdict%%|*}"; delta_desc="${verdict#*|}"
dropped_ok=PASS
case "$dropped" in
  0) ;;
  *) dropped_ok=FAIL ;;
esac
stored_ok=PASS
[ "${stored_events:-0}" -gt 0 ] 2>/dev/null || stored_ok=FAIL

mkdir -p "$out_dir"
cp "$work/k6.json" "$out_dir/k6.json"
cat >"$out_dir/result.md" <<EOF
# Instrumentation overhead: ${name} (${date})

${venue}

| | |
|---|---|
| Commit | \`${commit}\` |
${machine_rows}
| Load average (1/5/15 min) before / after | ${load_before} / ${load_after} |
| Load generator | k6 ${k6v} on the same machine (open model: constant arrival rate) |
| Offered load | ${rate} req/s per leg for ${duration}, POST /articles on two reference hosts |
| Legs | \`bare\` = Client::disabled() · \`instrumented\` = live SDK -> this collector |
| Overhead budget | med(instrumented) − med(bare) < ${budget}% of med(bare) (docs/END_GOAL.md) |

## The legs (\`perf/k6/overhead.js\`, exit ${k6_status})

| Leg | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) | reqs |
|---|---|---|---|---|---|
| bare | $(metric "$work/k6.json" 'http_req_duration{scenario:bare}' med) | $(metric "$work/k6.json" 'http_req_duration{scenario:bare}' 'p(95)') | $(metric "$work/k6.json" 'http_req_duration{scenario:bare}' 'p(99)') | $(metric "$work/k6.json" 'http_req_duration{scenario:bare}' max) | $(metric "$work/k6.json" 'http_reqs{scenario:bare}' count) |
| instrumented | $(metric "$work/k6.json" 'http_req_duration{scenario:instrumented}' med) | $(metric "$work/k6.json" 'http_req_duration{scenario:instrumented}' 'p(95)') | $(metric "$work/k6.json" 'http_req_duration{scenario:instrumented}' 'p(99)') | $(metric "$work/k6.json" 'http_req_duration{scenario:instrumented}' max) | $(metric "$work/k6.json" 'http_reqs{scenario:instrumented}' count) |

| Gate | Verdict |
|---|---|
| Median overhead (instrumented − bare) | **${gate}** — ${delta_desc} |
| SDK drops during the run | **${dropped_ok}** — dropped=${dropped} |
| Instrumented telemetry actually stored | **${stored_ok}** — ${stored_events} events in the store |
| k6 thresholds | exit ${k6_status} |

## SDK counters (GET /aiops/stats — drops disclosed, never hidden)

- instrumented host: \`${sdk_stats}\`
- bare host: \`${sdk_bare}\`

## First failures (verbatim)

\`\`\`
$(grep 'AIOPS_FIRST_FAILURE ' "$work/k6.txt" | head -10 | sed 's/^AIOPS_FIRST_FAILURE //' || true)
\`\`\`

## Threshold verdicts

\`\`\`
$(grep -E '✓|✗' "$work/k6.txt" | sed 's/^ *//' || true)
\`\`\`
EOF
echo "wrote $out_dir/result.md (k6 exit $k6_status; median gate ${gate}; drops ${dropped_ok})"
if [ "$k6_status" -ne 0 ] || [ "$gate" != PASS ] || [ "$dropped_ok" != PASS ] || [ "$stored_ok" != PASS ]; then
  exit 1
fi
