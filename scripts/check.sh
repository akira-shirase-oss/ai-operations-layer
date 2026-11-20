#!/usr/bin/env bash
# Runs what CI runs: the toolchain pin, Rust gates (fmt, clippy -D warnings, tests against
# real PostgreSQL), the architecture rule, then the console's type-check and build.
# Run it before every push.
set -euo pipefail
cd "$(dirname "$0")/.."

scripts/check-toolchain.sh
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --locked -- -D warnings
scripts/check-isolated.sh
if [ -z "${DATABASE_URL:-}" ]; then
  # The store, API and black-box tests create their own databases in this server.
  scripts/stack.sh up >/dev/null
  eval "$(scripts/stack.sh env)"
fi
cargo test --workspace --locked
scripts/check-architecture.sh
scripts/check-perf-scenarios.sh
npm run check --workspace @aiops/ui
npm run check --workspace @aiops/console
npm run build --workspace @aiops/console
echo "check ok"
