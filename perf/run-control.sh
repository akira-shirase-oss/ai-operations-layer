#!/usr/bin/env bash
# One recorded control-surface run, end to end: a fresh database in the local stack,
# the release collector, k6 driving the control mix while ingest runs concurrently
# at its reference rate — the invariant under test is that a flood of control reads
# never starves the write path. The database is counted afterwards, as always.
#
#   perf/run-control.sh <name> <control req/s> <duration> [ingest batches/s] [revoke pool]
#   perf/run-control.sh smoke 20 30s 20 12        # short development check
#   perf/run-control.sh million 300 3400s 100 600 # ≥1,000,000 control requests
#
# Writes perf/results/<date>-control-<name>.md (+ the k6 summary as JSON). The result
# carries the machine, the commit, the load average before and after, and every miss.
#
# PostgreSQL: same modes as run-ingest.sh — AIOPS_PG_URL for an external server (CI),
# otherwise the local docker stack.
set -euo pipefail
cd "$(dirname "$0")/.."

name="${1:?name}"
rate="${2:?rate (control requests per second)}"
duration="${3:?duration (k6 format, e.g. 60s)}"
ingest_rate="${4:-100}"
revoke_pool="${5:-200}"
date="$(date -u +%Y-%m-%d)"
out="perf/results/${date}-control-${name}"
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
  # `wait` returns the killed child's status (143) — it must not abort the trap.
  { [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null && wait "$server_pid" 2>/dev/null; } || true
  # The logs outlive the work dir — the last failed run deleted its own evidence.
  mkdir -p perf/results
  [ -f "$work/server.log" ] && cp "$work/server.log" "${out}.server.log" || true
  [ -f "$work/control.txt" ] && cp "$work/control.txt" "${out}.k6.txt" || true
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

cargo build --release --locked -p aiops-server >/dev/null 2>&1
bin=target/release/aiops-server
export AIOPS_DATABASE_URL="$pg_base/$db"
"$bin" admin create-project perf >/dev/null
key="$("$bin" admin create-ingest-key --project perf --name k6)"

# Every call in the mix sits under its caller's control bucket (5 rps/token), so the
# run mints enough credentials that the offered rate is honest sustained capacity
# inside the limit — never an unlimited one. Admins carry audit/credentials/revokes;
# readers carry controls reads and the audited denials.
admins=$(( rate / 4 + 4 ))
readers=$(( rate / 20 + 2 ))
admin_list=""
for i in $(seq 1 "$admins"); do
  t="$("$bin" admin create-token --actor "ops-$i" --scope admin 2>/dev/null)"
  admin_list="${admin_list:+$admin_list,}$t"
done
reader_list=""
for i in $(seq 1 "$readers"); do
  t="$("$bin" admin create-token --actor "view-$i" --project perf --scope read 2>/dev/null)"
  reader_list="${reader_list:+$reader_list,}$t"
done
# The revoke pool: minted ids (the secret is discarded — revoking needs the id only,
# and a secret never enters the run). First pass over the pool grants; after that
# every pass is an audited 404, the same shape a revoke race reads.
revoke_ids=""
for i in $(seq 1 "$revoke_pool"); do
  id="$("$bin" admin create-token --actor "revokee-$i" --project perf --scope read 2>&1 >/dev/null \
       | sed -n 's/^token \([0-9a-f-]*\) .*/\1/p')"
  revoke_ids="${revoke_ids:+$revoke_ids,}$id"
done

# One HTTP GET; print the body, fail on transport or HTTP error. curl where it
# runs; on machines where curl is out of reach (a sandbox), python3's urllib —
# the same stand-in pattern as perf-psql for SQL.
if curl --version >/dev/null 2>&1; then
  # $2 carries an Authorization header — it rides a stdin -K config so `ps`
  # never sees the token (white-hat S3).
  http_get() {
    if [ $# -ge 2 ]; then curl -fsS -m 5 -K - "$1" <<<"header = \"$2\"";
    else curl -fsS -m 5 "$1"; fi
  }
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
  # The header rides AIOPS_HDR in the environment, never argv.
  http_get() {
    AIOPS_HDR="${2-}" python3 - "$1" <<'PY'
import os, sys, urllib.request
req = urllib.request.Request(sys.argv[1])
hdr = os.environ.get("AIOPS_HDR", "")
if hdr:
    name, _, value = hdr.partition(":")
    req.add_header(name.strip(), value.strip())
try:
    sys.stdout.write(urllib.request.urlopen(req, timeout=5).read().decode())
except Exception as e:
    sys.stderr.write(f"http_get: {e}\n")
    sys.exit(1)
PY
  }
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
echo "collector on 127.0.0.1:$port" >&2

AIOPS_LISTEN="127.0.0.1:$port" \
AIOPS_INGEST_EVENTS_PER_SEC=10000000 AIOPS_INGEST_BURST_EVENTS=10000000 \
AIOPS_QUERY_REQUESTS_PER_SEC=100000 AIOPS_QUERY_BURST=100000 \
AIOPS_RETENTION_SWEEP_SECS=86400 AIOPS_LOG=warn \
  "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do
  http_get "http://127.0.0.1:$port/readyz" >/dev/null 2>&1 && break
  sleep 0.2
done

# Pre-flight (CI 44544883621 burned 57 minutes against a dead listener): the
# readyz loop only waits — it can't fail. Verify before any load: /healthz
# answers, then one authenticated control call proves auth+pool+project. A
# miss here aborts with the collector's log tail instead of an all-zero run.
fail_preflight() {
  echo "PREFLIGHT FAILED: $1" >&2
  [ -f "$work/server.log" ] && { echo '--- collector log (tail) ---' >&2; tail -30 "$work/server.log" >&2; }
  exit 2
}
code="$(probe_code "http://127.0.0.1:$port/healthz")"
[ "$code" = 200 ] || fail_preflight "GET /healthz answered HTTP $code — the collector is not up; load would only see connection-refused."
code="$(probe_code "http://127.0.0.1:$port/v1/admin/credentials" "authorization: Bearer ${admin_list%%,*}")"
[ "$code" = 200 ] || fail_preflight "GET /v1/admin/credentials with a minted admin token answered HTTP $code (expected 200)."

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
load_before="$(uptime | sed 's/.*load averages*: //' || echo 'unavailable')"
k6v="$(k6 version | head -1 | awk '{print $2}')"

set +e
# Tokens ride in the environment, never in -e argv (white-hat S3).
BASE_URL="http://127.0.0.1:$port" \
ADMINS="$admin_list" READERS="$reader_list" REVOKE_IDS="$revoke_ids" \
INGEST_KEY="$key" RUN_ID="$run_id" PROJECT=perf \
RATE="$rate" INGEST_RATE="$ingest_rate" DURATION="$duration" \
OPERATIONS=100000 OPS_PER_BATCH=1 \
k6 run --quiet --summary-export "$work/control.json" \
  --include-system-env-vars \
  perf/k6/control.js >"$work/control.txt" 2>&1
control_status=$?
set -e
# pipefail + set -e: a denied tool (ps/uptime in a sandbox) would abort the run
# through the pipeline's nonzero status — let it degrade to "unavailable" instead.
load_after="$(uptime | sed 's/.*load averages*: //' || echo 'unavailable')"
rss_kb="$(ps -o rss= -p "$server_pid" 2>/dev/null | tr -d ' ' || true)"

stored_events="$(psql_db -c 'SELECT count(*) FROM events')"
stored_ops="$(psql_db -c 'SELECT count(*) FROM operations')"
audit_rows="$(psql_db -c 'SELECT count(*) FROM audit_log')"
audit_denials="$(psql_db -c "SELECT count(*) FROM audit_log WHERE decision = 'denied'")"
tokens_revoked="$(psql_db -c 'SELECT count(*) FROM api_tokens WHERE revoked_at IS NOT NULL')"
collector_stats="$(http_get "http://127.0.0.1:$port/v1/collector/stats" "authorization: Bearer ${admin_list%%,*}" || echo 'unavailable')"

mkdir -p perf/results
cp "$work/control.json" "$out.control.json"

metric() {
  python3 -c "import json,sys; m=json.load(open(sys.argv[1]))['metrics'].get(sys.argv[2],{}); v=m.get(sys.argv[3], m.get('values',{}).get(sys.argv[3])); v=0 if v is None and sys.argv[3]=='count' else v; print('n/a' if v is None else (round(v,2) if isinstance(v,float) else v))" "$@"
}
ctl="$work/control.json"
cat >"$out.md" <<EOF
# Control-surface run: ${name} (${date})

${venue}

| | |
|---|---|
| Commit | \`${commit}\` |
${machine_rows}
| Load average (1/5/15 min) before | ${load_before} |
| … after | ${load_after} |
| Load generator | k6 ${k6v} on the same machine (open model: constant arrival rate) |
| Offered load | ${rate} control req/s for ${duration} + ${ingest_rate} ingest batches/s concurrently |
| Credentials | ${admins} admin tokens, ${readers} read tokens, a ${revoke_pool}-id revoke pool |

## The mix (\`perf/k6/control.js\`, exit ${control_status})

~40% audit page walk · ~30% recovery reads · ~15% credentials · ~10% audited denials · ~5% revokes.

| Surface | p50 (ms) | p95 (ms) | p99 (ms) | max (ms) |
|---|---|---|---|---|
| audit (page walk, filters) | $(metric "$ctl" 'http_req_duration{name:audit}' med) | $(metric "$ctl" 'http_req_duration{name:audit}' 'p(95)') | $(metric "$ctl" 'http_req_duration{name:audit}' 'p(99)') | $(metric "$ctl" 'http_req_duration{name:audit}' max) |
| controls (recovery reads) | $(metric "$ctl" 'http_req_duration{name:controls}' med) | $(metric "$ctl" 'http_req_duration{name:controls}' 'p(95)') | $(metric "$ctl" 'http_req_duration{name:controls}' 'p(99)') | $(metric "$ctl" 'http_req_duration{name:controls}' max) |
| credentials | $(metric "$ctl" 'http_req_duration{name:credentials}' med) | $(metric "$ctl" 'http_req_duration{name:credentials}' 'p(95)') | $(metric "$ctl" 'http_req_duration{name:credentials}' 'p(99)') | $(metric "$ctl" 'http_req_duration{name:credentials}' max) |
| denied (audited 403) | $(metric "$ctl" 'http_req_duration{name:denied}' med) | $(metric "$ctl" 'http_req_duration{name:denied}' 'p(95)') | $(metric "$ctl" 'http_req_duration{name:denied}' 'p(99)') | $(metric "$ctl" 'http_req_duration{name:denied}' max) |
| revoke (allowed→audited 404) | $(metric "$ctl" 'http_req_duration{name:revoke}' med) | $(metric "$ctl" 'http_req_duration{name:revoke}' 'p(95)') | $(metric "$ctl" 'http_req_duration{name:revoke}' 'p(99)') | $(metric "$ctl" 'http_req_duration{name:revoke}' max) |

| Metric | Value |
|---|---|
| Requests (total) | $(metric "$ctl" http_reqs count) |
| Control requests (rate achieved) | $(metric "$ctl" control_requests rate) |
| Rate-limited by the control bucket (429) | $(metric "$ctl" rate_limited count) |
| Audit pages walked | $(metric "$ctl" audit_pages_walked count) |
| Checks passed (rate) | $(metric "$ctl" checks value) |
| HTTP failures (rate, real errors only — expected 4xx incl. 429 excluded) | $(metric "$ctl" http_req_failed value) |
| Dropped iterations | $(metric "$ctl" dropped_iterations count) |

## The invariant — ingest concurrent, never starved (scenario \`ingest\`)

| Metric | Value |
|---|---|
| Events sent | $(metric "$ctl" events_sent count) |
| **Events lost** | $(metric "$ctl" events_lost count) |
| Batches retried after no reply | $(metric "$ctl" batches_retried count) |
| Ingest p50 / p95 / p99 (ms) | $(metric "$ctl" 'http_req_duration{scenario:ingest}' med) / $(metric "$ctl" 'http_req_duration{scenario:ingest}' 'p(95)') / $(metric "$ctl" 'http_req_duration{scenario:ingest}' 'p(99)') |

## What the database holds afterwards (counted, not inferred)

| | |
|---|---|
| Events stored | ${stored_events} |
| Operations stored | ${stored_ops} |
| Audit rows (the log the walk read) | ${audit_rows} |
| … denials among them | ${audit_denials} |
| Tokens revoked by the run | ${tokens_revoked} |
| Collector RSS after the run | ${rss_kb:-unavailable (process tools denied here)}${rss_kb:+ KiB} |
| Collector counters | \`${collector_stats}\` |

## First failures (verbatim — status 0 means the request never left k6)

\`\`\`
$(grep 'AIOPS_FIRST_FAILURE ' "$work/control.txt" | head -10 | sed 's/^AIOPS_FIRST_FAILURE //' || true)
\`\`\`

## Threshold verdicts

\`\`\`
$(grep -E '✓|✗' "$work/control.txt" | sed 's/^ *//' || true)
\`\`\`
EOF
echo "wrote $out.md (control exit $control_status)"
exit "$control_status"
