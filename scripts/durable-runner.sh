#!/usr/bin/env bash
# Prints the path of a release `durable-server` (with `durable-cli` beside it) built from
# Durable Runner's public
# repository at the revision this layer is tested against (its D-050 handler-call headers).
# Uses DURABLE_RUNNER_DIR (a checkout) when set, else a clone in DURABLE_RUNNER_SRC (default:
# the temporary directory; never under target/, which CI's cache restores incompletely: run
# 44093842622 got a clone whose objects were missing). A clone that cannot read the tested
# revision is replaced. Either way the runner is built in its own repository's target/.
set -euo pipefail
cd "$(dirname "$0")/.."

rev=9d240a7d043a7701db011bf9857e264e7217f7f8
repo=https://github.com/akira-shirase-oss/durable-runner.git
command -v brew >/dev/null && export PATH="$(brew --prefix rustup)/bin:$PATH"
unset CARGO_TARGET_DIR

dir="${DURABLE_RUNNER_DIR:-}"
if [ -z "$dir" ]; then
  dir="${DURABLE_RUNNER_SRC:-${RUNNER_TEMP:-${TMPDIR:-/tmp}}/aiops-durable-runner-src}"
  has_rev() { git -C "$dir" cat-file -e "$rev^{tree}" 2>/dev/null; }
  has_rev || git -C "$dir" fetch --quiet origin >&2 2>/dev/null || true
  if ! has_rev; then
    rm -rf "$dir"
    git clone --quiet "$repo" "$dir" >&2
  fi
  git -C "$dir" checkout --quiet --force "$rev" >&2
elif ! git -C "$dir" merge-base --is-ancestor "$rev" HEAD 2>/dev/null; then
  echo "error: $dir does not contain Durable Runner $rev (the handler-call headers)" >&2
  exit 1
fi
(cd "$dir" && cargo build --release --locked --quiet -p durable-server -p durable-cli >&2)
echo "$(cd "$dir" && pwd)/target/release/durable-server"
