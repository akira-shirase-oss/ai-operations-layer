#!/usr/bin/env bash
# Drill for review N3: clients that never read their responses must not hold the collector's
# connection slots. With the cap lowered to 8, eight clients with 4 KiB receive buffers each
# pipeline 4,000 `GET /healthz` and never read; then a fresh client asks for `/healthz` and
# the time until it is answered is recorded. No console or credentials are needed.
#
#   perf/drill-stalled-readers.sh
#
# Writes perf/results/<date>-drill-stalled-readers.md with the machine, commit and load.
set -euo pipefail
cd "$(dirname "$0")/.."

port="${AIOPS_DRILL_PORT:-57413}"
date="$(date -u +%Y-%m-%d)"
out="perf/results/${date}-drill-stalled-readers.md"
db="aiops_drill_$(date -u +%Y%m%d%H%M%S)"
work="$(mktemp -d)"
command -v brew >/dev/null && export PATH="$(brew --prefix rustup)/bin:$PATH"
unset CARGO_TARGET_DIR
scripts/stack.sh up >/dev/null
psql_admin() { docker compose -f deploy/compose.yaml exec -T postgres psql -U aiops -d aiops -v ON_ERROR_STOP=1 -qtA "$@"; }
psql_admin -c "CREATE DATABASE $db" >/dev/null
cleanup() {
  [ -n "${server_pid:-}" ] && kill "$server_pid" 2>/dev/null
  wait 2>/dev/null || true
  psql_admin -c "DROP DATABASE IF EXISTS $db" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT

cargo build --release --locked -p aiops-server --quiet
export AIOPS_DATABASE_URL="postgres://aiops:aiops-dev@127.0.0.1:${AIOPS_PG_PORT:-57432}/$db"
bin=target/release/aiops-server
"$bin" admin create-project pilot >/dev/null
load_before="$(uptime | sed 's/.*averages*: //')"
AIOPS_LISTEN="127.0.0.1:$port" AIOPS_MAX_CONNECTIONS=8 AIOPS_LOG=warn AIOPS_RETENTION_SWEEP_SECS=86400 \
  "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null 2>&1 && break; sleep 0.2; done

python3 -I - "$port" > "$work/result.txt" <<'PY'
import socket, sys, time
port = int(sys.argv[1])
stalled = []
for _ in range(8):
    s = socket.socket()
    s.setsockopt(socket.SOL_SOCKET, socket.SO_RCVBUF, 4096)
    s.connect(("127.0.0.1", port))
    s.setblocking(False)
    data = b"GET /healthz HTTP/1.1\r\nhost: x\r\n\r\n" * 4000
    sent = 0
    deadline = time.time() + 2
    while sent < len(data) and time.time() < deadline:
        try:
            sent += s.send(data[sent:])
        except BlockingIOError:
            time.sleep(0.01)
        except OSError:
            break
    stalled.append(s)
time.sleep(0.5)
started = time.time()
answer = None
while time.time() - started < 60 and answer is None:
    try:
        c = socket.create_connection(("127.0.0.1", port), timeout=60 - (time.time() - started))
        c.settimeout(max(0.1, 60 - (time.time() - started)))
        c.sendall(b"GET /healthz HTTP/1.1\r\nhost: x\r\nconnection: close\r\n\r\n")
        line = c.recv(64)
        if line.startswith(b"HTTP/1.1 200"):
            answer = time.time() - started
        c.close()
    except OSError:
        time.sleep(0.1)
print(f"fresh_healthz_answered_after_s={'%.1f' % answer if answer is not None else 'none in 60'}")
for s in stalled:
    s.close()
PY
load_after="$(uptime | sed 's/.*averages*: //')"
after="$(sed -n 's/^fresh_healthz_answered_after_s=//p' "$work/result.txt")"
if [ "$after" != "none in 60" ] && python3 -c "import sys; sys.exit(0 if float('$after') < 20 else 1)"; then verdict=PASS; else verdict=FAIL; fi
{
  echo "# Drill: clients that never read (review N3) — $date"
  echo
  echo "- Commit: \`$(git rev-parse --short HEAD)\`$(git diff --quiet || echo ' (with uncommitted changes)')"
  echo "- Machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sysctl -n hw.ncpu 2>/dev/null) CPUs"
  echo "- Load average before / after: $load_before / $load_after"
  echo "- Shape: release collector on 127.0.0.1:$port with \`AIOPS_MAX_CONNECTIONS=8\`; 8 clients with 4 KiB receive buffers each pipeline 4,000 \`GET /healthz\` and never read; then a fresh \`GET /healthz\`"
  echo
  echo '```'
  cat "$work/result.txt"
  echo '```'
  echo
  echo "**$verdict** — pass means the fresh \`/healthz\` is answered within 20 s (the 10 s write-stall"
  echo "deadline cuts the non-reading clients and frees their slots)."
} > "$out"
cat "$out"
