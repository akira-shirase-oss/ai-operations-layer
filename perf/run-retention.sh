#!/usr/bin/env bash
# Retention throughput (review finding 5): N expired events plus fresh ones in a scratch
# database, then one `aiops-server sweep` (the same loop the collector schedules: 5,000-row
# DELETEs, 20 per call, 50 ms between calls), timed. The database is counted afterwards.
#
#   perf/run-retention.sh [expired rows, default 1000000] [fresh rows, default 100000]
set -euo pipefail
cd "$(dirname "$0")/.."
expired="${1:-1000000}"
fresh="${2:-100000}"
db="aiops_ret_$(date -u +%Y%m%d%H%M%S)"
export PATH="$(brew --prefix rustup)/bin:$PATH"
unset CARGO_TARGET_DIR
scripts/stack.sh up >/dev/null
psql_admin() { docker compose -f deploy/compose.yaml exec -T postgres psql -U aiops -d aiops -v ON_ERROR_STOP=1 -qtA "$@"; }
psql_db() { docker compose -f deploy/compose.yaml exec -T postgres psql -U aiops -d "$db" -v ON_ERROR_STOP=1 -qtA "$@"; }
psql_admin -c "CREATE DATABASE $db"
trap 'psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true' EXIT
cargo build --release --locked -p aiops-server --quiet
bin=target/release/aiops-server
export AIOPS_DATABASE_URL="postgres://aiops:aiops-dev@127.0.0.1:${AIOPS_PG_PORT:-57432}/$db"
"$bin" admin create-project perf >/dev/null
"$bin" admin create-ingest-key --project perf --name h >/dev/null
insert() { # count, received-at expression, offset
  psql_db -c "INSERT INTO events (project_id, event_id, operation_id, instance_id, ingest_key_id, seq, op_seq,
                event_time, received_at, body)
              SELECT p.id, md5('e' || (i + $3))::uuid, md5('o' || ((i + $3) / 10))::uuid,
                     '11111111-1111-1111-1111-111111111111', k.id, i + $3, (i % 10) + 1, $2, $2,
                     jsonb_build_object('event_id', md5('e' || (i + $3)), 'type', 'step.finished',
                       'operation_id', md5('o' || ((i + $3) / 10)), 'seq', i + $3, 'op_seq', (i % 10) + 1,
                       'event_time', $2, 'step_id', '00f067aa0ba902b7', 'attempt', 1, 'outcome', 'succeeded',
                       'kind', 'model', 'name', 'generate')
              FROM generate_series(1, $1) AS i, projects p, ingest_keys k"
}
t0=$(date +%s)
insert "$expired" "now() - interval '15 days'" 0
insert "$fresh" "now()" "$expired"
psql_db -c "ANALYZE events"
load_t=$(( $(date +%s) - t0 ))
before="$(psql_db -c 'SELECT count(*) FROM events')"
bytes_before="$(psql_db -c "SELECT pg_total_relation_size('events')")"
commit="$(git rev-parse --short HEAD)$(git diff --quiet || echo '-dirty')"
machine="$(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cores, $(( $(sysctl -n hw.memsize) / 1073741824 )) GB, macOS $(sw_vers -productVersion)"
load_before="$(uptime | sed 's/.*load averages*: //')"
result="$("$bin" sweep)"
load_after="$(uptime | sed 's/.*load averages*: //')"
after="$(psql_db -c 'SELECT count(*) FROM events')"
left_expired="$(psql_db -c "SELECT count(*) FROM events WHERE received_at < now() - interval '14 days'")"
secs="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['seconds'])" "$result")"
deleted="$(python3 -c "import json,sys; print(json.loads(sys.argv[1])['events'])" "$result")"
rate="$(python3 -c "print(round($deleted / max($secs, 0.001)))")"
out="perf/results/$(date -u +%Y-%m-%d)-retention-sweep.md"
cat >"$out" <<MD
# Retention sweep throughput ($(date -u +%Y-%m-%d))

Development observation on a shared laptop, not production capacity (review finding 5).

| | |
|---|---|
| Commit | \`${commit}\` |
| Machine | ${machine} |
| Load average before / after | ${load_before} / ${load_after} |
| PostgreSQL | 18.6 in Docker (local stack), default settings plus \`shared_buffers=256MB\` |
| Rows | ${expired} expired (received 15 days ago) + ${fresh} fresh; loaded in ${load_t} s |
| \`events\` before | ${before} rows, ${bytes_before} bytes |
| Sweep (\`aiops-server sweep\`: 5,000-row DELETEs, 20 per call, 50 ms between calls) | \`${result}\` |
| **Deleted per second** | **${rate}** |
| \`events\` after | ${after} rows; expired rows left: ${left_expired} |

The guide's reference load writes 1,000 events/s (3.6 million an hour). One scheduled run
now keeps deleting while expired rows remain, so the sweep keeps up as long as its
deletion rate exceeds the write rate; the rate above is the margin on this machine.
Before the fix a run deleted at most 100,000 rows per table and then waited an hour.
MD
echo "wrote $out: $result"
