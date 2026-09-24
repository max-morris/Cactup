#!/bin/sh
# Reuse the binary the live site already serves when no build input changed.
#
# Usage: ci/fetch-live-binary.sh <root> <target> <build> <out>
#
# Succeeds (exit 0, file at <out>) only when <root>/latest.json names exactly
# <build>, has an entry for <target>, and the immutable file it points at
# matches that entry's size and sha256. Any other outcome -- first deploy,
# a changed build id, network trouble, a CDN mid-propagation -- exits
# non-zero with one short line on stderr, and the caller builds instead.
set -eu

note() {
    printf 'fetch-live-binary: %s\n' "$1" >&2
}

if [ $# -ne 4 ]; then
    note "usage: $0 <root> <target> <build> <out>"
    exit 2
fi
root=${1%/}
target=$2
build=$3
out=$4

for cmd in curl jq sha256sum; do
    if ! command -v "$cmd" > /dev/null 2>&1; then
        note "need '$cmd' (command not found)"
        exit 1
    fi
done

outdir=$(dirname -- "$out")
mkdir -p "$outdir"
json=$(mktemp "$outdir/.latest.json.XXXXXX")
tmp=$(mktemp "$outdir/.cactup-live.XXXXXX")
trap 'rm -f "$json" "$tmp"' EXIT
trap 'exit 130' INT
trap 'exit 143' TERM

if ! curl -fsL --retry 2 --connect-timeout 10 --max-time 60 -o "$json" "$root/latest.json"; then
    note "no live latest.json at $root"
    exit 1
fi

live=$(jq -r '.build // empty' "$json" 2> /dev/null || true)
if [ "$live" != "$build" ]; then
    note "live build is '${live:-none}', want $build"
    exit 1
fi

path=$(jq -r --arg t "$target" '.targets[$t].path // empty' "$json" 2> /dev/null || true)
want_sha=$(jq -r --arg t "$target" '.targets[$t].sha256 // empty' "$json" 2> /dev/null || true)
want_size=$(jq -r --arg t "$target" '.targets[$t].size // empty' "$json" 2> /dev/null || true)
if [ -z "$path" ] || [ -z "$want_sha" ] || [ -z "$want_size" ]; then
    note "live latest.json has no complete entry for $target"
    exit 1
fi
case $path in
    /* | *..* | *://*)
        note "live latest.json has a suspicious path for $target: $path"
        exit 1
        ;;
esac

if ! curl -fsL --retry 2 --connect-timeout 10 --max-time 300 -o "$tmp" "$root/$path"; then
    note "cannot download $root/$path"
    exit 1
fi

size=$(wc -c < "$tmp" | tr -d ' ')
if [ "$size" != "$want_size" ]; then
    note "size mismatch for $path: got $size, want $want_size"
    exit 1
fi
sum=$(sha256sum "$tmp")
if [ "${sum%% *}" != "$want_sha" ]; then
    note "sha256 mismatch for $path"
    exit 1
fi

chmod 0755 "$tmp"
mv -f "$tmp" "$out"
note "reusing live $target build $build from $root/$path"
