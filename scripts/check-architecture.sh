#!/usr/bin/env bash
# aiops-domain is the contract in pure form: no async runtime, HTTP, storage, clock or
# randomness crates may appear in its [dependencies], and its source may not read the
# clock, the environment, files or the network. Keep it that way so every rule stays
# cheap to test.
set -euo pipefail
cd "$(dirname "$0")/.."

manifest=crates/aiops-domain/Cargo.toml
bad="$(sed -n '/^\[dependencies\]/,/^\[/p' "$manifest" \
  | grep -nE '^(tokio|axum|sqlx|reqwest|tower|hyper|getrandom|rand|uuid|chrono)' || true)"
if [ -n "$bad" ]; then
  echo "error: aiops-domain must not depend on I/O crates:" >&2
  echo "$bad" >&2
  exit 1
fi
src="$(grep -rnE 'std::(fs|net|env|process|thread)|SystemTime|Instant::now|OffsetDateTime::now' crates/aiops-domain/src || true)"
if [ -n "$src" ]; then
  echo "error: aiops-domain source must not do I/O or read clocks:" >&2
  echo "$src" >&2
  exit 1
fi
# The SDK may not depend on the server or the store: hosts link it, nothing else.
sdk="$(sed -n '/^\[dependencies\]/,/^\[/p' crates/aiops-sdk/Cargo.toml 2>/dev/null \
  | grep -nE '^(aiops-store|aiops-server|sqlx|axum)' || true)"
if [ -n "$sdk" ]; then
  echo "error: aiops-sdk must not depend on the collector or its store:" >&2
  echo "$sdk" >&2
  exit 1
fi
echo "architecture ok: aiops-domain has no I/O; aiops-sdk depends on neither store nor server"
