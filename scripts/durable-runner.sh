#!/usr/bin/env bash
# Prints the path of a release `durable-server` built from Durable Runner's public
# repository at the revision this layer is tested against (its D-050 handler-call headers).
# Uses DURABLE_RUNNER_DIR (a checkout) when set, else clones into target/durable-runner-src.
# Either way the runner is built in its own repository's target/ (never this one's).
set -euo pipefail
cd "$(dirname "$0")/.."

rev=9d240a7d043a7701db011bf9857e264e7217f7f8
repo=https://github.com/akira-shirase-oss/durable-runner.git
command -v brew >/dev/null && export PATH="$(brew --prefix rustup)/bin:$PATH"
unset CARGO_TARGET_DIR

dir="${DURABLE_RUNNER_DIR:-}"
if [ -z "$dir" ]; then
  dir=target/durable-runner-src
  if [ ! -d "$dir/.git" ]; then
    git clone --quiet "$repo" "$dir" >&2
  fi
  git -C "$dir" fetch --quiet origin >&2 || true
  git -C "$dir" checkout --quiet "$rev" >&2
elif ! git -C "$dir" merge-base --is-ancestor "$rev" HEAD 2>/dev/null; then
  echo "error: $dir does not contain Durable Runner $rev (the handler-call headers)" >&2
  exit 1
fi
(cd "$dir" && cargo build --release --locked --quiet -p durable-server >&2)
echo "$(cd "$dir" && pwd)/target/release/durable-server"
