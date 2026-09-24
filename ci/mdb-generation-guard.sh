#!/bin/sh
# MDB generation guard (the mdb-compat CI job).
#
# Every published MDB commit is read by every deployed binary of its
# generation, so a commit that does not bump mdb/GENERATION must keep working
# in both directions against the commit that last set it ($base, the oldest
# binary of the current generation):
#
#   (a) the base binary loads the MDB of this commit
#       (worktree at $base, mdb/ taken from HEAD);
#   (b) this binary still loads what the generation promised overlays
#       (worktree at HEAD, mdb/ taken from $base).
#
# Each direction runs the MDB sweep tests of the mdb module:
# every in-repo machine must load and validate (meta.toml schema, variant
# files, optionlists) and every hostname.regexp must compile. A direction is
# skipped when it would only re-run $base itself (nothing on its side changed).
#
# What this cannot catch, and needs human judgment (see the docs page
# cactupdocs/content/authors/mdb-generations.md, "What counts as breaking"):
# template variables a binary substitutes differently, the .py script
# protocol, optionlist [cactup] header keys an older binary silently ignores,
# and any other change in meaning that still loads. The regexp test
# also hard-codes which machines are undiscoverable, so adding a new
# undiscoverable machine trips direction (a) even though older binaries cope.
#
# Usage: ci/mdb-generation-guard.sh   (from anywhere inside the repo)
# Environment: CARGO_TARGET_DIR (default <repo>/target/mdb-guard) is shared by
# both worktrees so the dependency build is reused.
set -eu

TESTS="mdb::tests::loads_and_validates_every_dev_machine mdb::tests::every_dev_machine_regexp_compiles"
NTESTS=2

# Build inputs besides mdb/ (the rest of the build id's path list).
CODE_PATHS="src build.rs Cargo.toml Cargo.lock resources"

say() {
    printf 'mdb-guard: %s\n' "$1" >&2
}

repo=$(git rev-parse --show-toplevel)
cd "$repo"

base=$(git log -1 --format=%H -- mdb/GENERATION)
if [ -z "$base" ]; then
    printf '::error::mdb/GENERATION is not tracked in this history\n'
    exit 1
fi
head=$(git rev-parse HEAD)
generation=$(git show "$base:mdb/GENERATION" | tr -d ' \t\r\n')

if [ "$base" = "$head" ]; then
    say "this commit sets mdb/GENERATION ($generation); nothing to compare against"
    exit 0
fi
say "generation $generation was set by $(git log -1 --format='%h %s' "$base")"

run_a=yes
run_b=yes
if git diff --quiet "$base" "$head" -- mdb; then
    say "(a) skipped: mdb/ is unchanged since the base"
    run_a=no
fi
# shellcheck disable=SC2086  # CODE_PATHS is a word list
if git diff --quiet "$base" "$head" -- $CODE_PATHS; then
    say "(b) skipped: the code is unchanged since the base"
    run_b=no
fi
if [ "$run_a" = no ] && [ "$run_b" = no ]; then
    exit 0
fi

CARGO_TARGET_DIR=${CARGO_TARGET_DIR:-$repo/target/mdb-guard}
export CARGO_TARGET_DIR

work=$(mktemp -d "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/mdb-guard.XXXXXX")
cleanup() {
    for wt in "$work/code-base" "$work/code-head"; do
        if [ -d "$wt" ]; then
            git -C "$repo" worktree remove --force "$wt" > /dev/null 2>&1 || true
        fi
    done
    rm -rf "$work"
    git -C "$repo" worktree prune > /dev/null 2>&1 || true
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

# check <label> <code-commit> <mdb-commit>: worktree at <code-commit> with its
# mdb/ replaced by <mdb-commit>'s, then run the sweep tests there.
check() {
    label=$1
    code=$2
    mdb=$3
    wt="$work/code-$label"
    git worktree add --quiet --detach "$wt" "$code"
    git archive --format=tar -o "$work/mdb-$label.tar" "$mdb" mdb
    rm -rf "$wt/mdb"
    tar -xf "$work/mdb-$label.tar" -C "$wt"
    log="$work/test-$label.log"
    status=0
    # shellcheck disable=SC2086  # TESTS is a word list
    (cd "$wt" && cargo test --locked --bin cactup -- --exact $TESTS) > "$log" || status=$?
    cat "$log"
    # A test renamed since the base would match nothing and "pass"; insist on
    # the full count.
    if [ "$status" -eq 0 ] && ! grep -q "test result: ok. $NTESTS passed" "$log"; then
        say "expected $NTESTS tests to run; the sweep test names may have changed"
        status=1
    fi
    return "$status"
}

failed=no
if [ "$run_a" = yes ]; then
    say "(a) generation-$generation base binary vs. the MDB of this commit"
    if ! check base "$base" "$head"; then
        printf '::error::The MDB at this commit fails to load in the first generation-%s binary. Bump mdb/GENERATION and add a "## Generation %s" entry to mdb/GENERATIONS.md (see https://max-morris.github.io/Cactup/authors/mdb-generations.html).\n' \
            "$generation" "$((generation + 1))"
        failed=yes
    fi
fi
if [ "$run_b" = yes ]; then
    say "(b) this binary vs. the generation-$generation base MDB"
    if ! check head "$head" "$base"; then
        printf '::error::This binary no longer loads the generation-%s MDB it must still accept (overlays written for it). Bump mdb/GENERATION and add a "## Generation %s" entry to mdb/GENERATIONS.md (see https://max-morris.github.io/Cactup/authors/mdb-generations.html).\n' \
            "$generation" "$((generation + 1))"
        failed=yes
    fi
fi
[ "$failed" = no ]
