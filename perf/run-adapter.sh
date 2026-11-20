#!/usr/bin/env bash
# One recorded Durable Runner run, end to end: a fresh database, the release
# collector + dr-article-worker + aiops-dr-adapter + a real durable-server, k6
# submitting real jobs (runner -> adapter /h/{step} -> worker -> telemetry) while
# a second scenario walks ops -> /authority against the runner's own journal.
#
#   perf/run-adapter.sh <name> <jobs/s> <duration> [authority reads/s]
#   perf/run-adapter.sh smoke 5 30s 10          # short development check
#   perf/run-adapter.sh standard 20 300s 40     # the recorded run
#
# The runner binary comes from AIOPS_DR_SERVER (a prebuilt durable-server) or is
# built from AIOPS_DR_SRC (a durable-runner checkout; the drill pins v1.0.0).
# Writes perf/results/<date>-adapter-<name>.md (+ k6 JSON + every server's log).
#
# PostgreSQL: AIOPS_PG_URL for an external server (CI), else the local stack.
set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:?name}"
rate="${2:?job submissions per second}"
duration="${3:?duration (k6 format, e.g. 60s)}"
auth_rate="${4:-$(( rate * 2 ))}"
date="$(date -u +%Y-%m-%d)"
out="perf/results/${date}-adapter-${name}"
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
  for pid in "${server_pid:-}" "${runner_pid:-}" "${adapter_pid:-}" "${worker_pid:-}"; do
    # `wait` returns the killed child's status (143) — it must not abort the trap.
    { [ -n "$pid" ] && kill "$pid" 2>/dev/null && wait "$pid" 2>/dev/null; } || true
  done
  # The logs outlive the work dir — a failed run must keep its own evidence.
  mkdir -p perf/results
  for f in server worker adapter runner; do
    [ -f "$work/$f.log" ] && cp "$work/$f.log" "${out}.${f}.log" || true
  done
  [ -f "$work/k6.txt" ] && cp "$work/k6.txt" "${out}.k6.txt" || true
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

# The runner is a separate product: a prebuilt binary, or a checkout to build.
# Resolve it before the workspace build — a missing source must not burn build time.
if [ -n "${AIOPS_DR_SERVER:-}" ]; then
  dr_bin="$AIOPS_DR_SERVER"
elif [ -n "${AIOPS_DR_SRC:-}" ]; then
  cargo build --release --manifest-path "$AIOPS_DR_SRC/Cargo.toml" >/dev/null 2>&1
  dr_bin="$AIOPS_DR_SRC/target/release/durable-server"
else
  echo "set AIOPS_DR_SERVER=<durable-server binary> or AIOPS_DR_SRC=<durable-runner checkout>" >&2
  exit 2
fi
[ -x "$dr_bin" ] || { echo "durable-server not executable at $dr_bin" >&2; exit 2; }

cargo build --release --locked -p aiops-server -p aiops-durable-runner -p aiops-dr-article-worker >/dev/null 2>&1
bin=target/release/aiops-server
worker_bin=target/release/dr-article-worker
adapter_bin=target/release/aiops-dr-adapter

export AIOPS_DATABASE_URL="$pg_base/$db"
"$bin" admin create-project perf >/dev/null
ingest_key="$("$bin" admin create-ingest-key --project perf --name k6-adapter)"
read_token="$("$bin" admin create-token --actor perf-reader --project perf --scope read)"
admin_token="$("$bin" admin create-token --actor perf-admin --scope admin)"
dr_api="$(openssl rand -hex 32)"
dr_read="$(openssl rand -hex 32)"

# One HTTP helper pair; curl where it runs, python3's urllib where it can't.
if curl --version >/dev/null 2>&1; then
  http_get() { curl -fsS -m 5 ${2:+-H "$2"} "$1"; }
  http_post() { curl -fsS -m 10 -X POST -H 'content-type: application/json' ${3:+-H "$3"} -d "$2" "$1"; }
  probe_code() { local c; c="$(curl -sS -m 5 -o /dev/null -w '%{http_code}' ${2:+-H "$2"} "$1" 2>/dev/null || true)"; echo "${c:-000}"; }
else
  http_get() {
    python3 - "$1" "$2" <<'PY'
import sys, urllib.request
req = urllib.request.Request(sys.argv[1])
if sys.argv[2]:
    req.add_header("authorization", sys.argv[2])
try:
    sys.stdout.write(urllib.request.urlopen(req, timeout=5).read().decode())
except Exception as e:
    sys.stderr.write(f"http_get: {e}\n")
    sys.exit(1)
PY
  }
  http_post() {
    python3 - "$1" "$2" "$3" <<'PY'
import sys, urllib.request
req = urllib.request.Request(sys.argv[1], data=sys.argv[2].encode(), method='POST')
req.add_header('content-type', 'application/json')
if sys.argv[3]:
    req.add_header("authorization", sys.argv[3])
try:
    sys.stdout.write(urllib.request.urlopen(req, timeout=10).read().decode())
except Exception as e:
    sys.stderr.write(f"http_post: {e}\n")
    sys.exit(1)
PY
  }
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


# A fixed high port sits inside the kernel's ephemeral source-port pool: an
# outbound call in the same job — a token mint's Postgres connection — can hold
# it, and the socket's TIME_WAIT outlives the mint (CI run 44730055284 died so:
# `bind 127.0.0.1:57410` EADDRINUSE with nothing answering). Default: an
# OS-chosen free port. An explicit override is honored after a bounded wait
# for it to come free — a held port aborts loudly here, not at the server bind.
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
worker_port="$(choose_port "${AIOPS_PERF_WORKER_PORT:-}")"
adapter_port="$(choose_port "${AIOPS_PERF_ADAPTER_PORT:-}")"
runner_port="$(choose_port "${AIOPS_PERF_RUNNER_PORT:-}")"
# Free picks released their probe socket, so two picks can repeat — re-pick.
while [ "$worker_port" = "$port" ]; do worker_port="$(free_port)"; done
while [ "$adapter_port" = "$port" ] || [ "$adapter_port" = "$worker_port" ]; do adapter_port="$(free_port)"; done
while [ "$runner_port" = "$port" ] || [ "$runner_port" = "$worker_port" ] || [ "$runner_port" = "$adapter_port" ]; do runner_port="$(free_port)"; done
echo "collector=$port worker=$worker_port adapter=$adapter_port runner=$runner_port" >&2

runner_url="http://127.0.0.1:$runner_port"
adapter_url="http://127.0.0.1:$adapter_port"
worker_url="http://127.0.0.1:$worker_port"
steps="generate validate factcheck publish"

# 1. The worker: the steps' real upstreams and the publish effect's authority.
WORKER_LISTEN="127.0.0.1:$worker_port" WORKER_HANG_MS=0 \
  "$worker_bin" >"$work/worker.log" 2>&1 &
worker_pid=$!

# 2. The adapter between runner and worker: proxies /h/{step}, reports telemetry.
python3 - "$work/adapter.json" <<PY
import json
steps = "${steps}".split()
cfg = {"listen": "127.0.0.1:$adapter_port",
       "runner": {"label": "perfdr", "url": "$runner_url", "step_timeout_ms": 5000},
       "workflow": "publish_article",
       "routes": [{"route": s,
                   "kind": "model" if s == "generate" else ("validation" if s == "validate" else "tool"),
                   "upstream": "$worker_url/" + s,
                   **({"model": "fake-model-1",
                       "price": {"input_micros_per_token": 1, "output_micros_per_token": 4,
                                 "currency": "USD", "basis": "fake-list-price-2026-11"}}
                      if s == "generate" else {}),
                   **({"external_effect": True} if s == "publish" else {})}
                  for s in steps]}
open("$work/adapter.json", "w").write(json.dumps(cfg))
PY
AIOPS_DR_ADAPTER_CONFIG="$work/adapter.json" DURABLE_READ_KEY="$dr_read" \
  AIOPS_ENDPOINT="http://127.0.0.1:$port" AIOPS_INGEST_KEY="$ingest_key" \
  "$adapter_bin" >"$work/adapter.log" 2>&1 &
adapter_pid=$!

# 3. The durable-server: journal in the work dir, handlers pointed at the adapter.
handler_args=()
for s in $steps; do handler_args+=(--handler "$s=$adapter_url/h/$s"); done
DURABLE_API_KEY="$dr_api" DURABLE_READ_KEY="$dr_read" \
  "$dr_bin" --journal "$work/journal" --listen "127.0.0.1:$runner_port" \
  --step-timeout-ms 5000 "${handler_args[@]}" >"$work/runner.log" 2>&1 &
runner_pid=$!

# 4. The collector, its authority read configured at the runner (D-027).
AIOPS_LISTEN="127.0.0.1:$port" \
AIOPS_AUTHORITY_DURABLE_RUNNER="perf=perfdr=$runner_url" \
AIOPS_AUTHORITY_DURABLE_RUNNER_READ_KEY="$dr_read" \
AIOPS_INGEST_EVENTS_PER_SEC=10000000 AIOPS_INGEST_BURST_EVENTS=10000000 \
AIOPS_QUERY_REQUESTS_PER_SEC=100000 AIOPS_QUERY_BURST=100000 \
AIOPS_RETENTION_SWEEP_SECS=86400 AIOPS_LOG=warn \
  "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!

# All four services get their own wait loop — a slow start is not a dead service,
# and the probe below must not race it.
for url in "http://127.0.0.1:$port/readyz" "$runner_url/health" "$adapter_url/healthz" "$worker_url/healthz"; do
  for _ in $(seq 1 100); do
    http_get "$url" >/dev/null 2>&1 && break
    sleep 0.2
  done
done

# Pre-flight: every health endpoint, then ONE real job through the whole chain —
# submit -> adapter steps -> telemetry -> ops list -> authority read — before any
# load. A dead link aborts with the log tails, not an all-zero run (CI 44544883621).
fail_preflight() {
  echo "PREFLIGHT FAILED: $1" >&2
  for f in server runner adapter worker; do
    [ -f "$work/$f.log" ] && { echo "--- $f log (tail) ---" >&2; tail -20 "$work/$f.log" >&2; }
  done
  exit 2
}
code="$(probe_code "http://127.0.0.1:$port/healthz")"
[ "$code" = 200 ] || fail_preflight "collector /healthz answered HTTP $code"
code="$(probe_code "$runner_url/health")"
[ "$code" = 200 ] || fail_preflight "durable-server /health answered HTTP $code"
code="$(probe_code "$adapter_url/healthz")"
[ "$code" = 200 ] || fail_preflight "adapter /healthz answered HTTP $code"
code="$(probe_code "$worker_url/healthz")"
[ "$code" = 200 ] || fail_preflight "worker /healthz answered HTTP $code"

spec="$(python3 -c "import json,base64; print(json.dumps({'op_key':'article/preflight-$run_id','spec':{'version':1,'steps':[{'name':s,'handler':s} for s in '${steps}'.split()],'input_b64':base64.b64encode(json.dumps({'topic':'preflight'}).encode()).decode()}}))")"
warm_json="$(http_post "$runner_url/jobs" "$spec" "authorization: Bearer $dr_api" 2>/dev/null)" \
  || fail_preflight "submitting the warm-up job to the runner failed"
warm_rid="$(python3 -c 'import json,sys; print((json.loads(sys.argv[1]) or {}).get("run_id",""))' "$warm_json" 2>/dev/null || echo '')"
[ -n "$warm_rid" ] || fail_preflight "the runner's submit answer carried no run_id: ${warm_json}"
warm_deadline=$(( SECONDS + 30 ))
warm_state=""
while [ $SECONDS -lt $warm_deadline ]; do
  warm_state="$(http_get "$runner_url/jobs/$warm_rid" "authorization: Bearer $dr_read" 2>/dev/null \
                | python3 -c 'import json,sys; print((json.load(sys.stdin) or {}).get("state",""))' 2>/dev/null || echo '')"
  [ "$warm_state" = "done" ] && break
  case "$warm_state" in failed*|cancelled) break;; esac
  sleep 0.3
done
[ "$warm_state" = "done" ] || fail_preflight "warm-up job did not reach done (state: '${warm_state:-no answer}')"
code="$(probe_code "http://127.0.0.1:$port/v1/admin/credentials" "authorization: Bearer $admin_token")"
[ "$code" = 200 ] || fail_preflight "authenticated /v1/admin/credentials answered HTTP $code"
# The chain lands telemetry -> an op -> an authority answer; check the last hop.
# The adapter flushes asynchronously — give the op a real window to appear.
op_id=""
op_deadline=$(( SECONDS + 20 ))
while [ $SECONDS -lt $op_deadline ] && [ -z "$op_id" ]; do
  op_id="$(http_get "http://127.0.0.1:$port/v1/projects/perf/operations?limit=1" "authorization: Bearer $read_token" 2>/dev/null \
           | python3 -c 'import json,sys; o=(json.load(sys.stdin) or {}).get("operations") or []; print(o[0]["operation_id"] if o else "")' 2>/dev/null || echo '')"
  [ -z "$op_id" ] && sleep 0.5
done
[ -n "$op_id" ] || fail_preflight "no operation landed from the warm-up job — the adapter's telemetry never reached the collector"
code="$(probe_code "http://127.0.0.1:$port/v1/projects/perf/operations/$op_id/authority" "authorization: Bearer $read_token")"
[ "$code" = 200 ] || fail_preflight "authority read on the warm-up op answered HTTP $code"

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
| Stack | release builds; ${store_desc}; runner binary \`${dr_bin}\` |"
venue="Development observation on a shared laptop, **not** production capacity."
if [ -n "${GITHUB_RUN_ID:-}" ]; then
  machine_rows="${machine_rows}
| CI run | ${GITHUB_SERVER_URL}/${GITHUB_REPOSITORY}/actions/runs/${GITHUB_RUN_ID} |"
  venue="Run on a GitHub-hosted Actions runner (dedicated VM, **not** production capacity)."
fi
load_before="$(uptime | sed 's/.*load averages*: //' || echo 'unavailable')"
k6v="$(k6 version | head -1 | awk '{print $2}')"

set +e
k6 run --quiet --summary-export "$work/adapter.json.out" \
  -e RUNNER_URL="$runner_url" -e DR_API="$dr_api" -e DR_READ="$dr_read" \
  -e BASE_URL="http://127.0.0.1:$port" -e TOKEN="$read_token" -e PROJECT=perf \
  -e RATE="$rate" -e AUTH_RATE="$auth_rate" -e DURATION="$duration" -e RUN_ID="$run_id" \
  perf/k6/adapter.js >"$work/k6.txt" 2>&1
k6_status=$?
set -e
load_after="$(uptime | sed 's/.*load averages*: //' || echo 'unavailable')"
rss_kb="$(ps -o rss= -p "$server_pid" 2>/dev/null | tr -d ' ' || true)"

stored_events="$(psql_db -c 'SELECT count(*) FROM events')"
stored_ops="$(psql_db -c 'SELECT count(*) FROM operations')"
stored_links="$(psql_db -c "SELECT count(DISTINCT operation_id) FROM events WHERE body->>'type' = 'operation.started' AND body->'attributes' ? 'job.runner'")"
adapter_stats="$(http_get "$adapter_url/stats" || echo 'unavailable')"
collector_stats="$(http_get "http://127.0.0.1:$port/v1/collector/stats" "authorization: Bearer $admin_token" || echo 'unavailable')"

mkdir -p perf/results
cp "$work/adapter.json.out" "$out.adapter.json"

metric() {
  python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); v=0 if v is None and sys.argv[3]=='count' else v; print('n/a' if v is None else (round(v,2) if isinstance(v,float) else v))" "$@"
}
sum="$work/adapter.json.out"
cat >"$out.md" <<EOF
# Durable Runner adapter + authority run: ${name} (${date})

${venue}

| | |
|---|---|
| Commit | \`${commit}\` |
${machine_rows}
| Load average (1/5/15 min) before | ${load_before} |
| … after | ${load_after} |
| Load generator | k6 ${k6v} on the same machine (constant arrival rate, two scenarios) |
| Offered load | ${rate} job submissions/s for ${duration} + ${auth_rate} authority reads/s |
| Chain | k6 -> durable-server -> aiops-dr-adapter -> dr-article-worker; adapter -> collector |

## The run (\`perf/k6/adapter.js\`, exit ${k6_status})

Real \`POST /jobs\` submissions (4-step publish_article) polled to a terminal state
on the runner's own journal; concurrent read-token ops-list -> /authority walks.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| POST /jobs (runner admission) | $(metric "$sum" 'http_req_duration{endpoint:submit}' med) | $(metric "$sum" 'http_req_duration{endpoint:submit}' 'p(95)') | $(metric "$sum" 'http_req_duration{endpoint:submit}' 'p(99)') | $(metric "$sum" 'http_req_duration{endpoint:submit}' max) |
| run completion (submit -> journal done, s) | — | $(metric "$sum" run_seconds 'p(95)') | $(metric "$sum" run_seconds 'p(99)') | $(metric "$sum" run_seconds max) |
| ops list | $(metric "$sum" 'http_req_duration{endpoint:ops-list}' med) | $(metric "$sum" 'http_req_duration{endpoint:ops-list}' 'p(95)') | $(metric "$sum" 'http_req_duration{endpoint:ops-list}' 'p(99)') | $(metric "$sum" 'http_req_duration{endpoint:ops-list}' max) |
| authority read (proxied to the runner) | $(metric "$sum" 'http_req_duration{endpoint:authority}' med) | $(metric "$sum" 'http_req_duration{endpoint:authority}' 'p(95)') | $(metric "$sum" 'http_req_duration{endpoint:authority}' 'p(99)') | $(metric "$sum" 'http_req_duration{endpoint:authority}' max) |

| Metric | Value |
|---|---|
| Jobs submitted | $(metric "$sum" jobs_submitted count) |
| Runs reaching \`done\` | $(metric "$sum" runs_done count) |
| Runs failed / timed out | $(metric "$sum" runs_failed count) |
| Ops lists that found ops | $(metric "$sum" ops_listed count) |
| Authority reads answered | $(metric "$sum" authority_answered count) |
| … truthful unavailable/not-linked answers | $(metric "$sum" authority_unavailable count) |
| Checks passed (rate) | $(metric "$sum" checks value) |
| HTTP failures (rate) | $(metric "$sum" http_req_failed value) |
| Dropped iterations | $(metric "$sum" dropped_iterations count) |

## What the stores hold afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | ${stored_events} |
| Operations stored (jobs + warm-up) | ${stored_ops} |
| Operation links to the runner | ${stored_links} |
| Collector RSS after the run | ${rss_kb:-unavailable (process tools denied here)}${rss_kb:+ KiB} |
| Adapter counters | \`${adapter_stats}\` |
| Collector counters | \`${collector_stats}\` |

## First failures (verbatim — status 0 means the request never left k6)

\`\`\`
$(grep 'AIOPS_FIRST_FAILURE ' "$work/k6.txt" | head -10 | sed 's/^AIOPS_FIRST_FAILURE //' || true)
\`\`\`

## Threshold verdicts

\`\`\`
$(grep -E '✓|✗' "$work/k6.txt" | sed 's/^ *//' || true)
\`\`\`
EOF
echo "wrote $out.md (k6 exit $k6_status)"
exit "$k6_status"
