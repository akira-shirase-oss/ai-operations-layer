#!/bin/bash
# Mutation proof for the attack suite (workspace AGENTS.md, 2026-11-20):
# every attack test must fail without its protection. For each named mutant
# in tests/attacks/mutants.py this applies the edit, runs the attack tests
# that assert that protection — they MUST fail — then restores the tree
# byte-for-byte. An attack that stays green under its mutant does not prove
# the protection and this script fails loudly.
#
# Needs DATABASE_URL (a scratch database per test is created/dropped against
# it, the same way the attack suite itself runs). Usage:
#   scripts/prove-attacks.sh [mutant ...]   # all mutants when none are named
set -uo pipefail
cd "$(dirname "$0")/.."

: "${DATABASE_URL:?DATABASE_URL must be set — the attack suite spawns real servers on scratch databases}"
export AIOPS_DATABASE_URL="$DATABASE_URL"

MUT=crates/aiops-server/tests/attacks/mutants.py
names=("$@")
if [ ${#names[@]} -eq 0 ]; then
    names=()
    # macOS ships bash 3.2 — no mapfile.
    for n in $(python3 "$MUT" list); do names+=("$n"); done
fi
[ ${#names[@]} -gt 0 ] || { echo "prove-attacks: no mutants found" >&2; exit 1; }

# A mutant build shares almost every artifact; heavy.sh's CARGO_INCREMENTAL=0
# is right for the main gate but would make each of these rebuilds full.
export CARGO_INCREMENTAL=1

# Mutants must start from a clean tree — a leftover mutant from a killed run
# would make later tests fail for the wrong reason and prove nothing.
if ! git diff --quiet -- 'crates/*/src'; then
    echo "prove-attacks: refusing to run on a dirty source tree (leftover mutant?)" >&2
    git status --short -- 'crates/*/src' >&2
    exit 1
fi

failed=0
for name in "${names[@]}"; do
    tests=$(python3 "$MUT" tests "$name")
    echo "prove-attacks: mutant $name → must red: $tests"
    python3 "$MUT" apply "$name"
    # Restore whatever happens — a crashed run must not leave a mutated tree.
    if cargo test -p aiops-server --test attacks -- $tests >.scratch/prove-"$name".log 2>&1; then
        python3 "$MUT" restore "$name"
        echo "prove-attacks: FAIL — $name stayed GREEN (the suite does not prove this protection); see .scratch/prove-$name.log"
        failed=1
    else
        python3 "$MUT" restore "$name"
        echo "prove-attacks: ok — $name went red as required"
    fi
done

# The tree must be byte-identical afterwards — no mutant may leak.
if ! git diff --quiet -- 'crates/*/src'; then
    echo "prove-attacks: FAIL — a mutant leaked into the working tree:" >&2
    git diff --stat -- 'crates/*/src' >&2
    exit 1
fi

[ "$failed" -eq 0 ] && echo "prove-attacks: all mutants proved (${#names[@]})"
exit "$failed"
