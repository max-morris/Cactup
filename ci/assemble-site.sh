#!/bin/sh
# Assemble the GitHub Pages site: the generated docs plus the installer, the
# release binaries and the version manifest the installed binaries poll.
#
# Usage: CACTUP_BUILD_ID=<sha7> CACTUP_BUILD_DATE=<%cI> ci/assemble-site.sh <site> <dist>
#
#   <site>  the docs output (must already hold index.html); files are added
#   <dist>  holds bin-<target>/cactup for every published target
#
# Resulting layout (under <site>):
#   cactup-init.sh
#   latest.json                     {build, date, mdb_generation, targets}
#   <target>/cactup                 stable alias (a convenience for manual downloads)
#   <target>/cactup-<build>         immutable copy, what latest.json and the installer fetch
#   <target>/cactup.sha256          "<hash>  cactup-<build>", naming that copy
#
# latest.json points at the immutable path because the Pages CDN caches every
# file independently: a client that read a fresh latest.json must never be
# handed a stale binary under the same name (spec §17).
#
# Runs locally too. It touches only the paths named here, never a glob over
# the repo (an untracked .claude/ worktree copy lives inside the checkout).
set -eu

# Every target a release publishes. Keep in sync with the build matrix in
# .github/workflows/ci.yml and CACTUP_TARGETS in cactup-init.sh.
TARGETS="x86_64-unknown-linux-musl aarch64-unknown-linux-musl"

die() {
    printf 'assemble-site: %s\n' "$1" >&2
    exit 1
}

[ $# -eq 2 ] || die "usage: CACTUP_BUILD_ID=.. CACTUP_BUILD_DATE=.. $0 <site> <dist>"
site=$1
dist=$2

repo=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd)

build=${CACTUP_BUILD_ID:-}
date=${CACTUP_BUILD_DATE:-}
[ -n "$build" ] || die "CACTUP_BUILD_ID is not set"
[ -n "$date" ] || die "CACTUP_BUILD_DATE is not set"
case $build in
    *[!0-9a-f]*) die "CACTUP_BUILD_ID is not a lowercase hex commit id: $build" ;;
esac
if [ ${#build} -lt 7 ] || [ ${#build} -gt 40 ]; then
    die "CACTUP_BUILD_ID must be 7 to 40 hex digits: $build"
fi
# The same shape build.rs accepts (RFC 3339 with an explicit offset, which is
# what `git log --format=%cI` prints): a looser date here would publish a
# manifest whose date no binary can order, and every client would then see
# the release as older than itself.
case $date in
    [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]Z) ;;
    [0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9][+-][0-9][0-9]:[0-9][0-9]) ;;
    *) die "CACTUP_BUILD_DATE must be RFC 3339 (YYYY-MM-DDTHH:MM:SS followed by Z or +HH:MM): $date" ;;
esac

[ -f "$site/index.html" ] || die "$site/index.html is missing (build the docs first)"

[ -f "$repo/mdb/GENERATION" ] || die "$repo/mdb/GENERATION is missing"
generation=$(tr -d ' \t\r\n' < "$repo/mdb/GENERATION")
case $generation in
    '' | *[!0-9]* | 0?*) die "mdb/GENERATION is not a plain integer: '$generation'" ;;
esac

need() {
    command -v "$1" > /dev/null 2>&1 || die "need '$1' (command not found)"
}
need jq
need sha256sum

cp "$repo/cactup-init.sh" "$site/cactup-init.sh"

targets='{}'
for target in $TARGETS; do
    src="$dist/bin-$target/cactup"
    [ -f "$src" ] || die "missing binary for $target: $src"
    mkdir -p "$site/$target"
    cp "$src" "$site/$target/cactup-$build"
    cp "$src" "$site/$target/cactup"
    chmod 0755 "$site/$target/cactup-$build" "$site/$target/cactup"
    sum=$(sha256sum "$site/$target/cactup-$build")
    hash=${sum%% *}
    size=$(wc -c < "$site/$target/cactup-$build" | tr -d ' ')
    printf '%s  %s\n' "$hash" "cactup-$build" > "$site/$target/cactup.sha256"
    targets=$(printf '%s' "$targets" | jq -c \
        --arg t "$target" --arg p "$target/cactup-$build" --arg h "$hash" --argjson s "$size" \
        '. + {($t): {path: $p, sha256: $h, size: $s}}')
    printf 'assemble-site: %s  %s bytes  %s\n' "$target" "$size" "$hash"
done

jq -n --arg b "$build" --arg d "$date" --argjson g "$generation" --argjson t "$targets" \
    '{build: $b, date: $d, mdb_generation: $g, targets: $t}' > "$site/latest.json"

for target in $TARGETS; do
    jq -e --arg t "$target" '.targets[$t].path' "$site/latest.json" > /dev/null \
        || die "latest.json lacks the $target entry"
done
printf 'assemble-site: wrote %s/latest.json (build %s, mdb generation %s)\n' "$site" "$build" "$generation"
