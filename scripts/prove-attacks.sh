#!/bin/bash
# Mutation proof for the attack suite (workspace AGENTS.md, 2026-11-20):
# every attack test must fail without its protection. For each named mutant
# in tests/attacks/mutants.py this applies the edit, runs the attack tests
# that assert that protection — they MUST fail — then restores the tree
# byte-for-byte. An attack that stays green under its mutant does not prove
# the protection and this script fails loudly.
#
# Proof, not exit status (review round 6, B3): "red" means the NAMED test
# printed its own `test <module>::<name> ... FAILED` line — a compile error,
# a missing database or some unrelated test's failure all exit nonzero too
# and prove nothing. The unmutated baseline runs first and must pass, so a
# broken tree cannot masquerade as a caught mutant.
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
# would make later tests fail for the wrong reason and prove nothing. The
# check is --porcelain, so an UNTRACKED file under src counts too: it could
# shadow a mutant's edit or survive its restore (review round 6, B3).
if [ -n "$(git status --porcelain -- 'crates/*/src')" ]; then
    echo "prove-attacks: refusing to run on a dirty source tree (leftover mutant? untracked file?)" >&2
    git status --short -- 'crates/*/src' >&2
    exit 1
fi

# Every named test must print `test <module>::<name> ... <want>` in the log.
# The `::` boundary keeps a bare name from matching a longer test's name.
lines_show() {
    local log=$1 want=$2; shift 2
    local t
    for t in "$@"; do
        grep -E "test [a-zA-Z0-9_]+::$t \.\.\. $want" "$log" >/dev/null || return 1
    done
    return 0
}

# The GREEN baseline first: every named test must pass unmutated — a red
# baseline means the tree, toolchain or database is already broken and no
# mutant run under it can be read (review round 6, B3).
all_tests=()
for name in "${names[@]}"; do
    for t in $(python3 "$MUT" tests "$name"); do all_tests+=("$t"); done
done
echo "prove-attacks: baseline — ${#all_tests[@]} named tests must pass unmutated"
if ! cargo test -p aiops-server --test attacks -- "${all_tests[@]}" >.scratch/prove-baseline.log 2>&1; then
    echo "prove-attacks: FAIL — the unmutated baseline is red (compile, database or an already-broken test); see .scratch/prove-baseline.log" >&2
    exit 1
fi
if ! lines_show .scratch/prove-baseline.log ok "${all_tests[@]}"; then
    echo "prove-attacks: FAIL — a named test never ran in the baseline (stale filter?); see .scratch/prove-baseline.log" >&2
    exit 1
fi
echo "prove-attacks: baseline green (${#all_tests[@]} tests)"

failed=0
for name in "${names[@]}"; do
    tests=()
    for t in $(python3 "$MUT" tests "$name"); do tests+=("$t"); done
    echo "prove-attacks: mutant $name → must red: ${tests[*]}"
    python3 "$MUT" apply "$name"
    # Restore whatever happens — a crashed run must not leave a mutated tree.
    status=0
    cargo test -p aiops-server --test attacks -- "${tests[@]}" >.scratch/prove-"$name".log 2>&1 || status=$?
    python3 "$MUT" restore "$name"
    if [ "$status" -eq 0 ]; then
        echo "prove-attacks: FAIL — $name stayed GREEN (the suite does not prove this protection); see .scratch/prove-$name.log"
        failed=1
    elif ! lines_show .scratch/prove-"$name".log FAILED "${tests[@]}"; then
        echo "prove-attacks: FAIL — $name exited nonzero but a named test's own FAILED line is missing (compile error, database down or a different test failed); see .scratch/prove-$name.log"
        failed=1
    else
        echo "prove-attacks: ok — $name went red on its named test(s)"
    fi
done

# The tree must be byte-identical afterwards — no mutant may leak, and no
# mutant may leave a stray file behind.
if [ -n "$(git status --porcelain -- 'crates/*/src')" ]; then
    echo "prove-attacks: FAIL — a mutant leaked into the working tree:" >&2
    git status --porcelain -- 'crates/*/src' >&2
    exit 1
fi

[ "$failed" -eq 0 ] && echo "prove-attacks: all mutants proved (${#names[@]})"
exit "$failed"
