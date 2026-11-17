#!/usr/bin/env bash
# The production path, end to end (docs/OPERATING.md): the release binaries, configured only
# by deploy/aiops-server.env.example (database, listener and console path substituted), in
# a clean environment: migrate, mint a project, key and token, serve the console under its
# CSP, take one operation from the SDK's example host, read it back, and stop on SIGTERM
# with exit 0. Uses its own database and port 57450.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v brew >/dev/null && export PATH="$(brew --prefix rustup)/bin:$PATH"
unset CARGO_TARGET_DIR
port=57450
db="aiops_prod_$(date -u +%Y%m%d%H%M%S)"
work="$(mktemp -d)"
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
  [ -n "${pid:-}" ] && kill "$pid" 2>/dev/null
  psql_admin -c "DROP DATABASE IF EXISTS $db WITH (FORCE)" >/dev/null 2>&1 || true
  rm -rf "$work"
}
trap cleanup EXIT
fail() { echo "production smoke FAILED: $1" >&2; tail -20 "$work/server.log" >&2 2>/dev/null || true; exit 1; }

cargo build --release --locked --quiet -p aiops-server
cargo build --release --locked --quiet -p aiops-sdk --example first_operation
[ -d apps/console/build ] || npm run build --workspace @aiops/console >/dev/null
sed -e "s|^AIOPS_DATABASE_URL=.*|AIOPS_DATABASE_URL=$db_base/$db|" \
    -e "s|^AIOPS_LISTEN=.*|AIOPS_LISTEN=127.0.0.1:$port|" \
    -e "s|^AIOPS_CONSOLE_DIR=.*|AIOPS_CONSOLE_DIR=$PWD/apps/console/build|" \
    deploy/aiops-server.env.example > "$work/aiops-server.env"
bin="$PWD/target/release/aiops-server"
# Only what the env file says, as systemd's EnvironmentFile gives it.
run() { env -i PATH=/usr/bin:/bin HOME="$work" bash -c 'set -a; . "$0"; set +a; shift; exec "$@"' "$work/aiops-server.env" -- "$@"; }

run "$bin" migrate | grep -q "migrations applied" || fail "migrate"
run "$bin" admin create-project prod >/dev/null || fail "create-project"
key="$(run "$bin" admin create-ingest-key --project prod --name example-host)" || fail "create-ingest-key"
token="$(run "$bin" admin create-token --actor operator --project prod 2>/dev/null)" || fail "create-token"
# Not through the run function: $! must be the server itself (env and bash exec into it).
env -i PATH=/usr/bin:/bin HOME="$work" bash -c 'set -a; . "$0"; set +a; exec "$1" serve' \
  "$work/aiops-server.env" "$bin" >"$work/server.log" 2>&1 &
pid=$!
for _ in $(seq 1 100); do curl -fsS "http://127.0.0.1:$port/readyz" >/dev/null 2>&1 && break; sleep 0.1; done
curl -fsS "http://127.0.0.1:$port/readyz" | grep -q '"ready":true' || fail "readyz"
curl -fsS -D "$work/h" -o /dev/null "http://127.0.0.1:$port/console/" || fail "console"
grep -qi '^content-security-policy:' "$work/h" || fail "the console is served without its CSP"

out="$(AIOPS_ENDPOINT="http://127.0.0.1:$port" AIOPS_INGEST_KEY="$key" target/release/examples/first_operation)"
echo "$out" | grep -q "emitted 8 events: 8 accepted, 0 dropped" || fail "the example host: $out"
list="$(curl -fsS "http://127.0.0.1:$port/v1/projects/prod/operations?request_id=example-1" -H "authorization: Bearer $token")"
echo "$list" | grep -q '"state":"uncertain"' || fail "read back: $list"

kill -TERM "$pid"
set +e; wait "$pid"; code=$?; set -e
pid=""
[ "$code" = 0 ] || fail "SIGTERM: exit $code"
grep -q "$key" "$work/server.log" && fail "the ingest key appears in the server log"
echo "production smoke ok: env-only config, migrate, admin, console+CSP, a host's operation read back, SIGTERM drain (exit 0)"
