#!/usr/bin/env bash
# The local stack: `up` starts PostgreSQL and waits until it answers, `down` stops it,
# `reset` throws its data away, `env` prints the variables the tests and the collector use.
set -euo pipefail
cd "$(dirname "$0")/.."

compose=(docker compose -f deploy/compose.yaml)
port="${AIOPS_PG_PORT:-57432}"
url="postgres://aiops:aiops-dev@127.0.0.1:${port}/aiops"

case "${1:-up}" in
  up)
    "${compose[@]}" up -d --wait postgres
    echo "stack up: $url"
    ;;
  down) "${compose[@]}" down ;;
  reset) "${compose[@]}" down -v ;;
  env)
    echo "export DATABASE_URL=$url"
    echo "export AIOPS_DATABASE_URL=$url"
    ;;
  *) echo "usage: scripts/stack.sh up|down|reset|env" >&2; exit 2 ;;
esac
