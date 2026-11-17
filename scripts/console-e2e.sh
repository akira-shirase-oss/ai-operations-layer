#!/usr/bin/env bash
# The console in a real browser (Playwright, Chromium) against the real stack: the release
# collector serving the built console under its CSP, the reference host instrumented with
# the SDK, and data that exercises the honest states (a lost publish response, a complete
# operation, an operation known only from gap reports, hostile event text, a denial).
# Then the first real host (A2): Durable Runner (scripts/durable-runner.sh) running the
# article workflow through the adapter, with the collector reading the runner's answer.
set -euo pipefail
cd "$(dirname "$0")/.."

port=57420
host_port=57421
dr_port=53331
adapter_port=53332
worker_port=53333
db="aiops_e2e_$(date -u +%Y%m%d%H%M%S)"
work="$(mktemp -d)"
command -v brew >/dev/null && export PATH="$(brew --prefix rustup)/bin:$PATH"
unset CARGO_TARGET_DIR
# CI provides a PostgreSQL service through DATABASE_URL; locally the compose stack is used.
if [ -n "${DATABASE_URL:-}" ] && command -v psql >/dev/null; then
  psql_admin() { psql "$DATABASE_URL" -v ON_ERROR_STOP=1 -qtA "$@"; }
  db_base="${DATABASE_URL%/*}"
else
  scripts/stack.sh up >/dev/null
  psql_admin() { docker compose -f deploy/compose.yaml exec -T postgres psql -U aiops -d aiops -v ON_ERROR_STOP=1 -qtA "$@"; }
  db_base="postgres://aiops:aiops-dev@127.0.0.1:${AIOPS_PG_PORT:-57432}"
fi
psql_admin -c "CREATE DATABASE $db" >/dev/null
cleanup() {
  for pid in "${adapter_pid:-}" "${worker_pid:-}" "${dr_pid:-}"; do
    [ -n "$pid" ] && kill "$pid" 2>/dev/null
  done
  [ -n "${host_pid:-}" ] && kill "$host_pid" 2>/dev/null
  [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null
  wait 2>/dev/null || true
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

cargo build --release --locked -p aiops-server -p aiops-reference-host -p aiops-durable-runner \
  -p aiops-dr-article-worker --quiet
npm run build --workspace @aiops/console >/dev/null
durable_server="$(scripts/durable-runner.sh)"
DURABLE_API_KEY="$(openssl rand -hex 32)"
DURABLE_READ_KEY="$(openssl rand -hex 32)"
export DURABLE_API_KEY DURABLE_READ_KEY
export AIOPS_DATABASE_URL="$db_base/$db"
bin=target/release/aiops-server
"$bin" admin create-project pilot >/dev/null
"$bin" admin create-project other >/dev/null
key="$("$bin" admin create-ingest-key --project pilot --name reference-host)"
probe_key="$("$bin" admin create-ingest-key --project pilot --name e2e-probe)"
token="$("$bin" admin create-token --actor e2e --project pilot 2>/dev/null)"
other_token="$("$bin" admin create-token --actor outsider --project other 2>/dev/null)"

dr_key="$("$bin" admin create-ingest-key --project pilot --name durable-runner-adapter)"

AIOPS_LISTEN="127.0.0.1:$port" AIOPS_CONSOLE_DIR="apps/console/build" AIOPS_LOG=warn \
  AIOPS_RETENTION_SWEEP_SECS=86400 \
  AIOPS_AUTHORITY_DURABLE_RUNNER="dr-e2e=http://127.0.0.1:$dr_port" \
  AIOPS_AUTHORITY_DURABLE_RUNNER_READ_KEY="$DURABLE_READ_KEY" \
  "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
REFHOST_LISTEN="127.0.0.1:$host_port" AIOPS_ENDPOINT="http://127.0.0.1:$port" AIOPS_INGEST_KEY="$key" \
  REFHOST_ALLOW_FAULTS=1 REFHOST_LOG_FILE=/dev/null target/release/reference-host >/dev/null 2>&1 &
host_pid=$!
for _ in $(seq 1 100); do
  curl -fsS "http://127.0.0.1:$port/readyz" >/dev/null 2>&1 && curl -fsS "http://127.0.0.1:$host_port/healthz" >/dev/null 2>&1 && break
  sleep 0.2
done

article() { curl -s -o /dev/null -X POST "http://127.0.0.1:$host_port/articles" -H 'content-type: application/json' -H "x-request-id: $1" -d "{\"topic\":\"e2e\",\"fault\":$2}"; }
article e2e-ok null
article e2e-lost '"publish_lost_response"'

now="$(python3 -c 'import datetime; print(datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ"))')"
xss_op="e2e0000000000000000000000000a551"
e3_op="e2e00000000000000000000000000e30"
e2_op="e2e00000000000000000000000000e20"
gap_op="e2e0000000000000000000000000d0e5"
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/v1/ingest" -H "authorization: Bearer $probe_key" -H 'content-type: application/json' -d @- <<JSON
{"schema_version": 1,
 "source": {"instance_id": "e2e000000000000000000000000000aa", "service": "e2e-probe", "sdk": "curl"},
 "events": [
  {"event_id": "e2e00000000000000000000000000001", "type": "operation.started", "operation_id": "$xss_op", "seq": 1, "op_seq": 1,
   "event_time": "$now", "name": "publish_article", "request_id": "e2e-xss",
   "attributes": {"retry.reason": "<img src=x onerror=alert(1)>", "http.route": "/a\u202eb"}},
  {"event_id": "e2e00000000000000000000000000002", "type": "operation.finished", "operation_id": "$xss_op", "seq": 2, "op_seq": 2,
   "event_time": "$now", "outcome": "succeeded", "events_emitted": 2},
  {"event_id": "e2e00000000000000000000000000003", "type": "gap.reported", "operation_id": "$gap_op", "seq": 3, "op_seq": 0,
   "event_time": "$now", "dropped": 10, "reason": "send_failed", "first_dropped_at": "$now", "last_dropped_at": "$now"},
  {"event_id": "e2e00000000000000000000000000004", "type": "operation.started", "operation_id": "$e3_op", "seq": 4, "op_seq": 1,
   "event_time": "$now", "name": "publish_article", "request_id": "e2e-e3"},
  {"event_id": "e2e00000000000000000000000000006", "type": "step.finished", "operation_id": "$e3_op", "seq": 6, "op_seq": 3,
   "event_time": "$now", "step_id": "e300000000000003", "attempt": 1, "kind": "tool", "name": "publish",
   "outcome": "uncertain", "uncertain_reason": "timeout_after_send", "error_code": "publish_timeout"},
  {"event_id": "e2e00000000000000000000000000007", "type": "operation.finished", "operation_id": "$e3_op", "seq": 7, "op_seq": 4,
   "event_time": "$now", "outcome": "failed", "failure_class": "internal", "events_emitted": 4},
  {"event_id": "e2e00000000000000000000000000008", "type": "operation.started", "operation_id": "$e2_op", "seq": 8, "op_seq": 1,
   "event_time": "$now", "name": "publish_article", "request_id": "e2e-e2"},
  {"event_id": "e2e00000000000000000000000000009", "type": "operation.finished", "operation_id": "$e2_op", "seq": 9, "op_seq": 2,
   "event_time": "$now", "outcome": "uncertain", "uncertain_reason": "timeout_after_send", "events_emitted": 2}
 ]}
JSON
# Review N4: a step with a 128-byte name whose outcome is the longest one ("outcome unknown:
# connection_lost_after_send"); its badge must stay whole in the events list.
long_op="e2e00000000000000000000000001049"
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/v1/ingest" -H "authorization: Bearer $probe_key" -H 'content-type: application/json' -d @- <<JSON
{"schema_version": 1,
 "source": {"instance_id": "e2e000000000000000000000000000dd", "service": "e2e-long", "sdk": "curl"},
 "events": [
  {"event_id": "e2e000000000000000000000000001a1", "type": "operation.started", "operation_id": "$long_op", "seq": 1, "op_seq": 1,
   "event_time": "$now", "name": "publish_article", "request_id": "e2e-long", "attributes": {"retry.reason": "rrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrrr"}},
  {"event_id": "e2e000000000000000000000000001a2", "type": "step.started", "operation_id": "$long_op", "seq": 2, "op_seq": 2,
   "event_time": "$now", "step_id": "1049000000000001", "attempt": 1, "kind": "tool", "name": "generate_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx", "external_effect": true},
  {"event_id": "e2e000000000000000000000000001a3", "type": "step.finished", "operation_id": "$long_op", "seq": 3, "op_seq": 3,
   "event_time": "$now", "step_id": "1049000000000001", "attempt": 1, "kind": "tool", "name": "generate_xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
   "outcome": "uncertain", "uncertain_reason": "connection_lost_after_send", "error_code": "publish_reset"},
  {"event_id": "e2e000000000000000000000000001a4", "type": "operation.finished", "operation_id": "$long_op", "seq": 4, "op_seq": 4,
   "event_time": "$now", "outcome": "uncertain", "uncertain_reason": "connection_lost_after_send", "events_emitted": 4}
 ]}
JSON
# A second service (its own instance) declares the same operation succeeded.
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/v1/ingest" -H "authorization: Bearer $probe_key" -H 'content-type: application/json' -d @- <<JSON
{"schema_version": 1,
 "source": {"instance_id": "e2e000000000000000000000000000bb", "service": "e2e-downstream", "sdk": "curl"},
 "events": [
  {"event_id": "e2e0000000000000000000000000000a", "type": "operation.finished", "operation_id": "$e2_op", "seq": 1, "op_seq": 1,
   "event_time": "$now", "outcome": "succeeded", "events_emitted": 1}
 ]}
JSON
# Two releases for the comparison journey: cmp-a with two succeeded operations; cmp-b with one
# succeeded, one failed and one that never finished.
python3 - "$now" > "$work/releases.json" <<'PY'
import json, sys
now = sys.argv[1]
events, seq = [], 0
for n, (release, outcome) in enumerate([("cmp-a", "succeeded"), ("cmp-a", "succeeded"), ("cmp-b", "succeeded"),
                                        ("cmp-b", "failed"), ("cmp-b", None)], start=1):
    op = "c0de%028x" % n
    seq += 1
    events.append({"event_id": "c0de%028x" % (n * 2), "type": "operation.started", "operation_id": op, "seq": seq,
                   "op_seq": 1, "event_time": now, "name": "publish_article", "release": release,
                   "request_id": "e2e-cmp-%d" % n})
    if outcome:
        seq += 1
        fin = {"event_id": "c0de%028x" % (n * 2 + 1), "type": "operation.finished", "operation_id": op, "seq": seq,
               "op_seq": 2, "event_time": now, "outcome": outcome, "events_emitted": 2}
        if outcome == "failed":
            fin["failure_class"] = "rejected"
        events.append(fin)
print(json.dumps({"schema_version": 1, "source": {"instance_id": "e2e000000000000000000000000000ee",
                  "service": "e2e-releases", "sdk": "python"}, "events": events}))
PY
curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/v1/ingest" -H "authorization: Bearer $probe_key" \
  -H 'content-type: application/json' --data-binary @"$work/releases.json"
# A 5,000-event operation (the timeline's cap) in ten batches of 500: a bulk import of 2,499
# effect-free tool calls, each started and finished, between the operation's start and finish.
big_op="e2e000000000000000000000000b1600"
for b in $(seq 0 9); do
  python3 - "$b" "$big_op" "$now" <<'PY' > "$work/big-$b.json"
import json, sys
b, op, now = int(sys.argv[1]), sys.argv[2], sys.argv[3]
events = []
for i in range(b * 500 + 1, b * 500 + 501):
    e = {"event_id": "e2eb%028x" % i, "operation_id": op, "seq": i, "op_seq": i, "event_time": now}
    if i == 1:
        e.update(type="operation.started", name="bulk_import", request_id="e2e-big")
    elif i == 5000:
        e.update(type="operation.finished", outcome="succeeded", events_emitted=5000)
    else:
        step = {"step_id": "%016x" % (i // 2), "kind": "tool", "name": "copy", "attempt": 1}
        if i % 2 == 0:
            e.update(type="step.started", external_effect=False, **step)
        else:
            e.update(type="step.finished", outcome="succeeded", **step)
    events.append(e)
print(json.dumps({"schema_version": 1, "source": {"instance_id": "e2e000000000000000000000000000cc", "service": "e2e-bulk", "sdk": "python"}, "events": events}))
PY
  curl -fsS -o /dev/null -X POST "http://127.0.0.1:$port/v1/ingest" -H "authorization: Bearer $probe_key" \
    -H 'content-type: application/json' --data-binary @"$work/big-$b.json"
done
# Wait until the reference host's operations (sent by its SDK) are listed, rather than for a
# fixed time: on a loaded machine the SDK's flush and the store can take longer than a second.
for req in e2e-ok e2e-lost; do
  for _ in $(seq 1 150); do
    curl -fsS "http://127.0.0.1:$port/v1/projects/pilot/operations?request_id=$req" -H "authorization: Bearer $token" \
      | grep -q '"finish_outcome"\|"state":"\(succeeded\|uncertain\|failed\)"' && break
    sleep 0.2
  done
done

# --- The first real host (A2): the article workflow as Durable Runner handlers. ---------
# The worker is uninstrumented; Durable Runner calls the adapter, which forwards to it.
WORKER_LISTEN="127.0.0.1:$worker_port" WORKER_HANG_MS=3000 target/release/dr-article-worker \
  >"$work/worker.log" 2>&1 &
worker_pid=$!
cat >"$work/adapter.json" <<JSON
{"listen": "127.0.0.1:$adapter_port",
 "runner": {"label": "dr-e2e", "url": "http://127.0.0.1:$dr_port", "step_timeout_ms": 1000},
 "workflow": "publish_article",
 "routes": [
  {"route": "generate", "kind": "model", "upstream": "http://127.0.0.1:$worker_port/generate", "model": "fake-model-1",
   "price": {"input_micros_per_token": 1, "output_micros_per_token": 4, "currency": "USD", "basis": "fake-list-price-2026-11"}},
  {"route": "validate", "kind": "validation", "upstream": "http://127.0.0.1:$worker_port/validate"},
  {"route": "factcheck", "kind": "tool", "upstream": "http://127.0.0.1:$worker_port/factcheck"},
  {"route": "publish", "kind": "tool", "external_effect": true, "upstream": "http://127.0.0.1:$worker_port/publish"}
 ]}
JSON
start_adapter() {
  AIOPS_DR_ADAPTER_CONFIG="$work/adapter.json" AIOPS_ENDPOINT="http://127.0.0.1:$port" AIOPS_INGEST_KEY="$dr_key" \
    target/release/aiops-dr-adapter >>"$work/adapter.log" 2>&1 &
  adapter_pid=$!
  for _ in $(seq 1 100); do curl -fsS "http://127.0.0.1:$adapter_port/healthz" >/dev/null 2>&1 && return; sleep 0.05; done
  echo "the adapter did not start" >&2; tail -5 "$work/adapter.log" >&2; exit 1
}
start_adapter
handlers=()
for h in generate validate factcheck publish; do handlers+=(--handler "$h=http://127.0.0.1:$adapter_port/h/$h"); done
"$durable_server" --journal "$work/journal" --listen "127.0.0.1:$dr_port" --step-timeout-ms 1000 "${handlers[@]}" \
  >"$work/durable.log" 2>&1 &
dr_pid=$!
for _ in $(seq 1 100); do curl -fsS "http://127.0.0.1:$dr_port/health" >/dev/null 2>&1 && break; sleep 0.1; done
submit() { # op_key fault → run id
  local input; input="$(printf '{"topic":"e2e","fault":%s}' "$2" | base64 | tr -d '\n')"
  curl -fsS -X POST "http://127.0.0.1:$dr_port/jobs" -H "authorization: Bearer $DURABLE_API_KEY" \
    -H 'content-type: application/json' \
    -d "{\"op_key\":\"$1\",\"spec\":{\"version\":1,\"steps\":[{\"name\":\"generate\",\"handler\":\"generate\"},{\"name\":\"validate\",\"handler\":\"validate\"},{\"name\":\"factcheck\",\"handler\":\"factcheck\"},{\"name\":\"publish\",\"handler\":\"publish\"}],\"input_b64\":\"$input\"}}" \
    | python3 -c 'import json,sys; print(json.load(sys.stdin)["run_id"])'
}
op_of() { # run id → operation ID (aiops_domain::correlate)
  python3 -c 'import hashlib,sys
p=lambda b: len(b).to_bytes(8,"big")+b
print(hashlib.sha256(b"aiops/job-runner/v1\0"+p(b"operation")+p(b"dr-e2e")+p(int(sys.argv[1]).to_bytes(8,"big"))).hexdigest()[:32])' "$1"
}
settle() { # run id… : wait until the runner says each is terminal and its finish is stored
  for run in "$@"; do
    for _ in $(seq 1 300); do
      state="$(curl -fsS "http://127.0.0.1:$dr_port/jobs/$run" -H "authorization: Bearer $DURABLE_READ_KEY" \
        | python3 -c 'import json,sys; print(json.load(sys.stdin)["state"])')"
      case "$state" in done|failed:*|cancelled) break ;; esac
      sleep 0.1
    done
    op="$(op_of "$run")"
    for _ in $(seq 1 100); do # the adapter's finish, declared from the runner's answer
      curl -fsS "http://127.0.0.1:$port/v1/projects/pilot/operations/$op" -H "authorization: Bearer $token" \
        | grep -q '"type":"operation.finished"' && break
      sleep 0.1
    done
    echo "durable runner run $run: $state"
  done
}
run_ok="$(submit article/e2e-dr-ok null)"
run_lost="$(submit article/e2e-dr-lost '"publish_lost_response"')"
run_invalid="$(submit article/e2e-dr-invalid '"validation_failed"')"
settle "$run_ok" "$run_lost" "$run_invalid"
# An adapter restart in the middle of a step, once nothing else is in flight: kill it as
# soon as its start of the run is stored, while the slow generate call is in flight. The
# runner reconciles and retries into a new adapter instance, so the first instance's tail
# is unknown while the runner says done: missing telemetry, not a failure.
run_restart="$(submit article/e2e-dr-restart '"slow_generate"')"
op_restart="$(op_of "$run_restart")"
for _ in $(seq 1 30); do
  curl -fsS -o /dev/null "http://127.0.0.1:$port/v1/projects/pilot/operations/$op_restart" -H "authorization: Bearer $token" && break
  sleep 0.05
done
kill -9 "$adapter_pid"; wait "$adapter_pid" 2>/dev/null || true
sleep 0.2
start_adapter
settle "$run_restart"

AIOPS_E2E_DR_OK=article/e2e-dr-ok AIOPS_E2E_DR_LOST=article/e2e-dr-lost AIOPS_E2E_DR_INVALID=article/e2e-dr-invalid \
AIOPS_E2E_DR_RESTART=article/e2e-dr-restart \
AIOPS_SCREENS_DIR="${AIOPS_SCREENS_DIR:-}" \
AIOPS_E2E_BASE="http://127.0.0.1:$port" AIOPS_E2E_TOKEN="$token" AIOPS_E2E_OTHER_TOKEN="$other_token" \
AIOPS_E2E_OK=e2e-ok AIOPS_E2E_LOST=e2e-lost AIOPS_E2E_E3=e2e-e3 AIOPS_E2E_E2=e2e-e2 AIOPS_E2E_BIG=e2e-big AIOPS_E2E_LONG=e2e-long AIOPS_E2E_XSS=e2e-xss AIOPS_E2E_GAP="$gap_op" \
  npm run test:e2e --workspace @aiops/console
