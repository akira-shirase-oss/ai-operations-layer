#!/usr/bin/env bash
# Failure drill: PostgreSQL restarts while the collector is ingesting (review finding 8).
# Expected: every answer is 200 or a retryable 503 (never another 5xx); every event the
# collector acknowledged is in the database afterwards; the collector recovers on its own.
set -euo pipefail
cd "$(dirname "$0")/.."
port=57415
db="aiops_drill_$(date -u +%Y%m%d%H%M%S)"
work="$(mktemp -d)"
run_id="$(openssl rand -hex 4)"
export PATH="$(brew --prefix rustup)/bin:$PATH"
unset CARGO_TARGET_DIR
scripts/stack.sh up >/dev/null
compose=(docker compose -f deploy/compose.yaml)
psql_admin() { "${compose[@]}" exec -T postgres psql -U aiops -d aiops -v ON_ERROR_STOP=1 -qtA "$@"; }
psql_db() { "${compose[@]}" exec -T postgres psql -U aiops -d "$db" -v ON_ERROR_STOP=1 -qtA "$@"; }
psql_admin -c "CREATE DATABASE $db"
cleanup() {
  [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null
  wait 2>/dev/null || true
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
cargo build --release --locked -p aiops-server --quiet
bin=target/release/aiops-server
export AIOPS_DATABASE_URL="postgres://aiops:aiops-dev@127.0.0.1:${AIOPS_PG_PORT:-57432}/$db"
"$bin" admin create-project perf >/dev/null
key="$("$bin" admin create-ingest-key --project perf --name k6)"
AIOPS_LISTEN="127.0.0.1:$port" AIOPS_INGEST_EVENTS_PER_SEC=10000000 AIOPS_INGEST_BURST_EVENTS=10000000 \
  AIOPS_LOG=warn "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do curl -fsS "http://127.0.0.1:$port/readyz" >/dev/null 2>&1 && break; sleep 0.2; done
commit="$(git rev-parse --short HEAD)$(git diff --quiet || echo '-dirty')"
machine="$(sysctl -n machdep.cpu.brand_string), $(sysctl -n hw.ncpu) cores, $(( $(sysctl -n hw.memsize) / 1073741824 )) GB, macOS $(sw_vers -productVersion)"
load_before="$(uptime | sed 's/.*load averages*: //')"
k6 run --quiet --summary-export "$work/k6.json" -e BASE_URL="http://127.0.0.1:$port" -e INGEST_KEY="$key" \
  -e RUN_ID="$run_id" -e RATE=100 -e DURATION=30s perf/k6/ingest.js >"$work/k6.txt" 2>&1 &
k6_pid=$!
sleep 8
restart_at="$(date -u +%H:%M:%S)"
"${compose[@]}" restart postgres >/dev/null 2>&1
restarted_at="$(date -u +%H:%M:%S)"
wait "$k6_pid" || true
ready_after="$(curl -s -o /dev/null -w '%{http_code}' "http://127.0.0.1:$port/readyz")"
stored="$(psql_db -c 'SELECT count(*) FROM events')"
m() { python3 -c "import json,sys; v=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); print(v.get('count', 0))" "$work/k6.json" "$1"; }
iters="$(m iterations)"; sent="$(m events_sent)"; acked="$(m events_acknowledged)"
shed="$(m batches_shed)"; s5xx="$(m batches_server_error)"
internal_logged="$(grep -c '"internal error"' "$work/server.log" || true)"
verdict="PASS"
[ "$s5xx" != "0" ] && verdict="FAIL (a non-503 5xx was answered)"
[ "$stored" -lt "$acked" ] && verdict="FAIL (acknowledged events missing from the database)"
[ "$ready_after" != "200" ] && verdict="FAIL (the collector did not recover)"
out="perf/results/$(date -u +%Y-%m-%d)-drill-pg-restart-$(date -u +%H%M%S).md"
cat >"$out" <<MD
# Drill: PostgreSQL restarts during ingest ($(date -u +%Y-%m-%d)) — ${verdict}

Development observation on a shared laptop (review finding 8).

| | |
|---|---|
| Commit | \`${commit}\` |
| Machine | ${machine}; load average before: ${load_before} |
| Load | k6, 100 batches/s × 10 events for 30 s, against the release collector |
| Restart | \`docker compose restart postgres\` from ${restart_at} to ${restarted_at} UTC |
| Batches | ${iters} |
| Answered 503 (retryable) | ${shed} |
| **Answered another 5xx** | **${s5xx}** |
| Events sent / acknowledged by the collector | ${sent} / ${acked} |
| **Events in the database afterwards** | **${stored}** (≥ acknowledged; a 503 batch may still have committed — re-sending it is idempotent) |
| "internal error" lines in the collector log | ${internal_logged} |
| \`/readyz\` after the run (no collector restart) | ${ready_after} |
MD
echo "wrote $out: $verdict"
