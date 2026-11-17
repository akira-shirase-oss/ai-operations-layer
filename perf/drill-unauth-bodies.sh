#!/usr/bin/env bash
# Drill for review N2: unauthenticated clients must not make the collector buffer bodies.
# Opens N connections with well-shaped but unknown ingest keys, each sending a 999,999-byte
# body except its last byte, and records the collector's resident memory before, while they
# are open and after they close, and how each connection was answered.
#
#   perf/drill-unauth-bodies.sh [connections]      # default 256
#
# Writes perf/results/<date>-drill-unauth-bodies.md with the machine, commit and load.
set -euo pipefail
cd "$(dirname "$0")/.."

conns="${1:-256}"
port="${AIOPS_DRILL_PORT:-57412}"
date="$(date -u +%Y-%m-%d)"
out="perf/results/${date}-drill-unauth-bodies.md"
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
AIOPS_LISTEN="127.0.0.1:$port" AIOPS_LOG=warn AIOPS_RETENTION_SWEEP_SECS=86400 "$bin" serve >"$work/server.log" 2>&1 &
server_pid=$!
for _ in $(seq 1 100); do curl -fsS "http://127.0.0.1:$port/healthz" >/dev/null 2>&1 && break; sleep 0.2; done

python3 -I - "$port" "$conns" "$server_pid" > "$work/result.txt" <<'PY'
import asyncio, os, random, string, subprocess, sys, time
port, conns, pid = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]
def rss_mib():
    return int(subprocess.run(["ps", "-o", "rss=", "-p", pid], capture_output=True, text=True).stdout.strip()) / 1024
BODY = 999_999
async def one(results, hold):
    key = "aiops_ik_" + "".join(random.choice(string.ascii_letters + string.digits) for _ in range(43))
    try:
        r, w = await asyncio.open_connection("127.0.0.1", port)
    except OSError as e:
        results.append(f"connect failed: {type(e).__name__}"); return
    head = (f"POST /v1/ingest HTTP/1.1\r\nhost: x\r\nauthorization: Bearer {key}\r\n"
            f"content-type: application/json\r\ncontent-length: {BODY}\r\n\r\n").encode()
    try:
        w.write(head + b" " * (BODY - 1))
        await asyncio.wait_for(w.drain(), 5)
    except Exception:
        pass  # the collector may answer and close before taking the rest
    try:
        line = await asyncio.wait_for(r.readline(), 5)
        results.append(line.decode(errors="replace").strip() or "closed without an answer")
    except Exception as e:
        results.append(f"no answer in 5 s ({type(e).__name__})")
    await hold.wait()
    w.close()
async def main():
    before = rss_mib()
    results, hold = [], asyncio.Event()
    tasks = [asyncio.create_task(one(results, hold)) for _ in range(conns)]
    await asyncio.sleep(8)
    during = rss_mib()
    hold.set()
    await asyncio.gather(*tasks, return_exceptions=True)
    await asyncio.sleep(5)
    after = rss_mib()
    from collections import Counter
    print(f"rss_before_mib={before:.1f}")
    print(f"rss_during_mib={during:.1f}")
    print(f"rss_after_mib={after:.1f}")
    for k, v in Counter(results).most_common():
        print(f"answer {v}x: {k}")
asyncio.run(main())
PY
load_after="$(uptime | sed 's/.*averages*: //')"
healthz="$(curl -s -o /dev/null -w '%{http_code}' --max-time 5 "http://127.0.0.1:$port/healthz" || true)"
{
  echo "# Drill: unauthenticated bodies (review N2) — $date"
  echo
  echo "- Commit: \`$(git rev-parse --short HEAD)\`$(git diff --quiet || echo ' (with uncommitted changes)')"
  echo "- Machine: $(sysctl -n machdep.cpu.brand_string 2>/dev/null || uname -m), $(sysctl -n hw.ncpu 2>/dev/null) CPUs, $(( $(sysctl -n hw.memsize 2>/dev/null || echo 0) / 1073741824 )) GiB"
  echo "- Load average before / after: $load_before / $load_after"
  echo "- Shape: $conns connections, each with a well-shaped unknown ingest key and a 999,999-byte body sent except its last byte, held 8 s, then closed"
  echo "- Release collector on 127.0.0.1:$port, defaults (2,048 connections, 1 MiB body cap, 32 MiB ingest body budget)"
  echo
  echo '```'
  cat "$work/result.txt"
  echo "healthz after: $healthz"
  echo '```'
  echo
  before=$(sed -n 's/^rss_before_mib=//p' "$work/result.txt")
  during=$(sed -n 's/^rss_during_mib=//p' "$work/result.txt")
  if python3 -c "import sys; sys.exit(0 if float('$during') - float('$before') < 64 else 1)" && [ "$healthz" = 200 ]; then
    verdict=PASS
  else
    verdict=FAIL
  fi
  echo "**$verdict** — pass means resident memory while the connections are open stays within 64 MiB"
  echo "of the baseline (no bodies buffered) and \`/healthz\` answers afterwards. Answers: an unknown key"
  echo "is refused from its headers (401, or 503 when the credential-lookup budget is spent); a client"
  echo "still sending its body when the collector answers and closes usually sees a reset instead of the"
  echo "answer (the collector does not linger to drain an unauthenticated body)."
} > "$out"
cat "$out"
