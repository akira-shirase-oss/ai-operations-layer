#!/usr/bin/env bash
# Each workspace member by itself, lib and bins only — no dev-dependencies.
# `cargo check --workspace` (and `cargo test`) unify features across the whole
# build, so a crate can borrow a feature another target's dev-deps happen to
# enable and still compile. A package-only build — `cargo build --release -p`,
# the console-e2e path — has no such rescue: reqwest's `json` lived only in
# [dev-dependencies] while src/ called `.json()`, and the workspace check was
# green. This is the gate the workspace check cannot give.
set -euo pipefail
cd "$(dirname "$0")/.."
command -v brew >/dev/null && export PATH="$(brew --prefix rustup)/bin:$PATH"
cargo metadata --format-version 1 --no-deps --locked -q | python3 -c '
import json, sys
for p in json.load(sys.stdin)["packages"]:
    kinds = {k for t in p["targets"] for k in t["kind"]}
    args = (["--lib"] if "lib" in kinds else []) + (["--bins"] if "bin" in kinds else [])
    print(p["name"] + "\t" + " ".join(args))
' | while IFS=$'\t' read -r member targets; do
    echo "check: $member (alone)"
    # shellcheck disable=SC2086
    cargo check --locked -p "$member" $targets --quiet
done
echo "each member checks alone"
