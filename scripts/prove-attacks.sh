#!/bin/bash
# Mutation proof for the attack suite (workspace AGENTS.md, 2026-11-20):
# every attack test must fail without its protection. For each named mutant
# in tests/attacks/mutants.py this applies the edit, runs the attack tests
# that assert that protection — they MUST fail — then restores the tree
# byte-for-byte. An attack that stays green under its mutant does not prove
# the protection and this script fails loudly.
#
# Proof, not exit status (review round 6, B3 + round 7, N4): "red" means the
# NAMED test printed its own `test <module>::<name> ... FAILED` line AND the
# panic landed at the test's pinned decisive assertion (mutants.py `pin`) —
# a compile error, a missing database, a setup unwrap or a different test's
# failure are all INCONCLUSIVE, never kills. The unmutated baseline runs
# first and must pass, so a broken tree cannot masquerade as a caught mutant.
# Mutants build under -D warnings: a mutant that does not compile clean
# cannot be proved and must be redrawn, not counted.
#
# Needs DATABASE_URL (a scratch database per test is created/dropped against
# it, the same way the attack suite itself runs). Usage:
#   scripts/prove-attacks.sh [mutant ...]   # all mutants when none are named
set -uo pipefail
cd "$(dirname "$0")/.."

: "${DATABASE_URL:?DATABASE_URL must be set — the attack suite spawns real servers on scratch databases}"
export AIOPS_DATABASE_URL="$DATABASE_URL"

MUT=crates/aiops-server/tests/attacks/mutants.py

fail() {
    echo "prove-attacks: FAIL — $*" >&2
    exit 1
}

# mutants.py is the source of truth for names, tests, pins and the audit —
# every call is exit-checked (review round 7, N4-e): a traceback must fail
# the run, never produce an empty word list that sweeps a whole suite.
mut() {
    python3 "$MUT" "$@" || fail "mutants.py $* exited $?"
}

# Mutants must start from a clean tree — a leftover mutant or stray file
# would make later tests fail for the wrong reason and prove nothing. The
# check is whole-tree --porcelain --untracked-files=all (review round 7,
# N3): the previous 'crates/*/src' pathspec matched no path, so the guard
# and the leak check were no-ops.
if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    echo "prove-attacks: refusing to run on a dirty tree (leftover mutant? untracked file?)" >&2
    git status --short >&2
    exit 1
fi

# An applied mutant is restored on ANY exit — a killed run must not leave a
# mutated security file in the shared tree (review round 7, N3).
applied=""
restore_applied() {
    if [ -n "$applied" ]; then
        python3 "$MUT" restore "$applied" \
            || echo "prove-attacks: restore of $applied FAILED — check the tree" >&2
        applied=""
    fi
}
trap restore_applied EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# The selector: names given on the command line must be KNOWN mutants and
# well-formed words (review round 7, N4-e/f) — an unknown name previously
# ran the entire suite unfiltered, and a word-split value must be plain.
names=("$@")
all_names=()
while IFS= read -r n; do all_names+=("$n"); done < <(mut list)
if [ ${#names[@]} -eq 0 ]; then
    names=("${all_names[@]}")
fi
[ ${#names[@]} -gt 0 ] || fail "no mutants found"
known=" ${all_names[*]} "
for n in "${names[@]}"; do
    [[ $n =~ ^[a-z0-9-]+$ ]] || fail "malformed mutant name: $n"
    [[ $known == *" $n "* ]] || fail "unknown mutant: $n (mut list: ${all_names[*]})"
done

# N4-(h): every attack test needs a mutant or a written exemption, every
# mutant's test list must resolve, and every pin must still sit on an
# assertion — audited BEFORE any mutant touches the tree.
mut audit || fail "mutant/test/pin enumeration failed — see above"

# A mutant build shares almost every artifact; heavy.sh's CARGO_INCREMENTAL=0
# is right for the main gate but would make each of these rebuilds full.
export CARGO_INCREMENTAL=1
# N4-(g): mutants compile under -D warnings — cargo caps dependency lints at
# `allow`, so this bites the workspace code a mutant lives in. A mutant that
# warns is inconclusive: the named test's own FAILED line will be missing.
export RUSTFLAGS="${RUSTFLAGS:+$RUSTFLAGS }-D warnings"

# Every named test must print `test <module>::<name> ... <want>` in the log.
# The `::` boundary keeps a bare name from matching a longer test's name.
# Zero names is a configuration bug, not a pass (review round 7, N4-e).
lines_show() {
    local log=$1 want=$2
    shift 2
    [ $# -gt 0 ] || return 1
    local t
    for t in "$@"; do
        grep -E "test [a-zA-Z0-9_]+::$t \.\.\. $want" "$log" >/dev/null || return 1
    done
    return 0
}

# A kill is pinned (N4-b): the test's panic must land at one of its listed
# decisive assertions (`mut pin <test>` prints `file:l1,l2,...`) — anywhere
# else means the mutant broke setup, not the protection.
pin_hit() {
    local log=$1 t=$2
    local spec file lines
    spec=$(mut pin "$t") || return 1
    file=${spec%%:*}
    lines=${spec##*:}
    grep -E "panicked at .*${file//./\\.}:(${lines//,/|}):" "$log" >/dev/null
}

# The word-split outputs mutants.py hands back must be plain test names —
# a stray token would be passed to cargo as a filter (review round 7, N4-f).
tests_for() {
    local out t
    out=$(mut tests "$1") || return 1
    for t in $out; do
        [[ $t =~ ^[a-z0-9_]+$ ]] || fail "mutant $1 names a malformed test: $t"
    done
    printf '%s\n' "$out"
}

# The GREEN baseline first: every named test must pass unmutated — a red
# baseline means the tree, toolchain or database is already broken and no
# mutant run under it can be read (review round 6, B3).
all_tests=()
for name in "${names[@]}"; do
    for t in $(tests_for "$name"); do all_tests+=("$t"); done
done
echo "prove-attacks: baseline — ${#all_tests[@]} named tests must pass unmutated"
if ! cargo test -p aiops-server --test attacks -- "${all_tests[@]}" >.scratch/prove-baseline.log 2>&1; then
    fail "the unmutated baseline is red (compile, database or an already-broken test); see .scratch/prove-baseline.log"
fi
if ! lines_show .scratch/prove-baseline.log ok "${all_tests[@]}"; then
    fail "a named test never ran in the baseline (stale filter?); see .scratch/prove-baseline.log"
fi
echo "prove-attacks: baseline green (${#all_tests[@]} tests)"

failed=0
for name in "${names[@]}"; do
    tests=()
    for t in $(tests_for "$name"); do tests+=("$t"); done
    echo "prove-attacks: mutant $name → must red: ${tests[*]}"
    mut apply "$name" || { failed=1; continue; }
    applied=$name
    status=0
    cargo test -p aiops-server --test attacks -- "${tests[@]}" >.scratch/prove-"$name".log 2>&1 || status=$?
    mut restore "$name" || fail "restore of $name failed — the tree may be mutated"
    applied=""
    if [ "$status" -eq 0 ]; then
        echo "prove-attacks: FAIL — $name stayed GREEN (the suite does not prove this protection); see .scratch/prove-$name.log"
        failed=1
        continue
    fi
    if ! lines_show .scratch/prove-"$name".log FAILED "${tests[@]}"; then
        echo "prove-attacks: FAIL — $name exited nonzero but a named test's own FAILED line is missing (compile error, database down or a different test failed); see .scratch/prove-$name.log"
        failed=1
        continue
    fi
    inconclusive=""
    for t in "${tests[@]}"; do
        if ! pin_hit .scratch/prove-"$name".log "$t"; then
            inconclusive="$inconclusive $t"
        fi
    done
    if [ -n "$inconclusive" ]; then
        echo "prove-attacks: FAIL — $name failed, but$inconclusive did not die at its pinned assertion (setup, helper or another assert — INCONCLUSIVE); see .scratch/prove-$name.log"
        failed=1
    else
        echo "prove-attacks: ok — $name went red on its pinned assertion(s)"
    fi
done

# The tree must be byte-identical afterwards — no mutant may leak, and no
# mutant may leave a stray file behind (whole-tree, untracked included).
if [ -n "$(git status --porcelain --untracked-files=all)" ]; then
    echo "prove-attacks: FAIL — a mutant leaked into the working tree:" >&2
    git status --porcelain --untracked-files=all >&2
    exit 1
fi

[ "$failed" -eq 0 ] && echo "prove-attacks: all mutants proved (${#names[@]})"
exit "$failed"
