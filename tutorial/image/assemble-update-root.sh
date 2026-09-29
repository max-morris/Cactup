#!/bin/sh
# Lay out a cactup update root, the same shape ci/assemble-site.sh publishes,
# for the one target the tutorial image runs on.
#
# Usage: assemble-update-root.sh <root> <target> <binary> <build-id> <build-date>
#
# Resulting layout (under <root>):
#   cactup-init.sh
#   latest.json                 {build, date, mdb_generation, targets}
#   <target>/cactup-<build>
#   <target>/cactup.sha256      "<hash>  cactup-<build>"
set -eu

die() {
    printf 'assemble-update-root: %s\n' "$1" >&2
    exit 1
}

[ $# -eq 5 ] || die "usage: $0 <root> <target> <binary> <build-id> <build-date>"
root=$1 target=$2 binary=$3 build=$4 date=$5
here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
generation=$(tr -d ' \t\r\n' < "$here/GENERATION")

mkdir -p "$root/$target"
cp "$here/cactup-init.sh" "$root/cactup-init.sh"
cp "$binary" "$root/$target/cactup-$build"
chmod 0755 "$root/$target/cactup-$build"
sum=$(sha256sum "$root/$target/cactup-$build")
hash=${sum%% *}
size=$(wc -c < "$root/$target/cactup-$build" | tr -d ' ')
printf '%s  %s\n' "$hash" "cactup-$build" > "$root/$target/cactup.sha256"
jq -n --arg b "$build" --arg d "$date" --argjson g "$generation" \
    --arg t "$target" --arg p "$target/cactup-$build" --arg h "$hash" --argjson s "$size" \
    '{build: $b, date: $d, mdb_generation: $g, targets: {($t): {path: $p, sha256: $h, size: $s}}}' \
    > "$root/latest.json"
