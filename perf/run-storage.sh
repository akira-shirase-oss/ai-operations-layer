#!/usr/bin/env bash
# Storage-per-day at the reference shape, end to end: a fresh database, the
# release collector, an ingest run at the reference shape, then the tables are
# measured — per-table heap and index bytes, row counts, bytes per event, and a
# projection to a day at the guide's reference load (1,000 events/s). The
# measurement is descriptive (this run establishes the baseline); the run's
# gate is the seed's own loss-free thresholds.
#
#   perf/run-storage.sh <name> <batches/s> <duration>
#   perf/run-storage.sh smoke 100 60s          # development check
#   perf/run-storage.sh million-ci 500 200s    # ~1,000,000 events then measure
#
# Writes perf/results/${AIOPS_RUN_NAME:-<date>-storage-<name>}/ with result.md,
# storage.json (machine-readable sizes), the seed's k6 summary and the log.
#
# PostgreSQL: same modes as run-ingest.sh — AIOPS_PG_URL for an external server
# (CI), otherwise the local docker stack.
set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:?name}"
rate="${2:?rate (batches per second)}"
duration="${3:?duration (k6 format)}"
date="$(date -u +%Y-%m-%d)"
out_dir="perf/results/${AIOPS_RUN_NAME:-${date}-storage-${name}}"
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
  for f in server.log ingest.txt; do
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
RATE="$rate" DURATION="$duration" OPS_PER_BATCH=1 \
k6 run --quiet --summary-export "$work/ingest.json" \
  --include-system-env-vars \
  perf/k6/ingest.js >"$work/ingest.txt" 2>&1
ingest_status=$?
set -e
load_after="$(uptime | sed 's/.*load averages*: //' || echo unavailable)"

stored_events="$(psql_db -c 'SELECT count(*) FROM events')"
stored_ops="$(psql_db -c 'SELECT count(*) FROM operations')"
db_bytes="$(psql_db -c "SELECT pg_database_size('$db')")"

# The measurement itself: every public table's heap and index footprint plus its
# row count, in one statement per row of a JSON document.
sizes_json="$(psql_db -c "
SELECT jsonb_object_agg(c.relname, jsonb_build_object(
  'rows', c.reltuples::bigint,
  'heap_bytes', pg_relation_size(c.oid),
  'index_bytes', pg_indexes_size(c.oid),
  'total_bytes', pg_total_relation_size(c.oid)))
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'public' AND c.relkind = 'r'")"

mkdir -p "$out_dir"
cp "$work/ingest.json" "$out_dir/ingest.json"
python3 - "$sizes_json" "$stored_events" "$db_bytes" "$work/sizes.txt" "$out_dir/storage.json" <<'PY'
import json, sys
sizes = json.loads(sys.argv[1])
stored = int(sys.argv[2])
out = {
    "tables": sizes,
    "events_stored": stored,
    "database_bytes": int(sys.argv[3]),
    "bytes_per_event_events_table": round(sizes.get("events", {}).get("total_bytes", 0) / max(stored, 1), 2),
}
with open(sys.argv[5], "w") as f:
    json.dump(out, f, indent=2, sort_keys=True)
lines = []
for t in sorted(sizes, key=lambda x: -sizes[x]["total_bytes"]):
    s = sizes[t]
    lines.append(f"| {t} | {s['rows']} | {s['heap_bytes']} | {s['index_bytes']} | {s['total_bytes']} |")
with open(sys.argv[4], "w") as f:
    f.write("\n".join(lines) + "\n")
PY

bytes_per_event="$(python3 -c "import json; print(json.load(open('$out_dir/storage.json'))['bytes_per_event_events_table'])")"
# A day at the guide's reference load is 86,400,000 events; the projection uses
# the events table's measured bytes/event — an extrapolation, labeled as such.
per_day="$(python3 -c "print(round(${bytes_per_event} * 86400000 / 1073741824, 2))")"

metric() {
  python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); v=0 if v is None and sys.argv[3]=='count' else v; print('n/a' if v is None else (round(v,2) if isinstance(v,float) else v))" "$@"
}
cat >"$out_dir/result.md" <<EOF
# Storage per day at the reference shape: ${name} (${date})

${venue}

| | |
|---|---|
| Commit | \`${commit}\` |
${machine_rows}
| Load average (1/5/15 min) before / after | ${load_before} / ${load_after} |
| Load generator | k6 ${k6v} on the same machine |
| Offered load | ${rate} batches/s × 10 events for ${duration} |
| Stored | ${stored_events} events, ${stored_ops} operations |

## Bytes per event and per table (measured after the run)

| Table | rows (est.) | heap bytes | index bytes | total bytes |
|---|---|---|---|---|
$(cat "$work/sizes.txt")
| **database total** | | | | **${db_bytes}** |

| Projection | Value |
|---|---|
| \`events\` bytes/event (table + indexes) | ${bytes_per_event} |
| Projected events-table footprint per day at 1,000 events/s (86.4 M/day) | ${per_day} GiB/day |

The projection is linear on this run's bytes/event — a lab extrapolation, not a
capacity forecast; TOAST compression and bloat drift with the workload.

## The seed (\`perf/k6/ingest.js\`, exit ${ingest_status})

| Metric | Value |
|---|---|
| Events sent | $(metric "$work/ingest.json" events_sent count) |
| Events lost | $(metric "$work/ingest.json" events_lost count) |
| Batches rate-limited / shed | $(metric "$work/ingest.json" batches_rate_limited count) / $(metric "$work/ingest.json" batches_shed count) |
| Ingest p50 / p95 / p99 (ms) | $(metric "$work/ingest.json" http_req_duration med) / $(metric "$work/ingest.json" http_req_duration 'p(95)') / $(metric "$work/ingest.json" http_req_duration 'p(99)') |

## Threshold verdicts

\`\`\`
$(grep -E '✓|✗' "$work/ingest.txt" | sed 's/^ *//' || true)
\`\`\`
EOF
echo "wrote $out_dir/result.md (ingest exit $ingest_status; ${bytes_per_event} B/event)"
exit "$ingest_status"
