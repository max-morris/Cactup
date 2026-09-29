#!/bin/sh
# Build the Cactup tutorial image.
#
# Usage: tutorial/image/build.sh [--target STAGE] [--tag TAG] [docker build args...]
#
# The cactup build stamps come from git, the way CI computes them: "current"
# is the last commit that touched the binary's inputs, "previous" the one
# before it. Both binaries are built from this checkout; "previous" only
# carries the older stamp, which is all the auto-update demo needs.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH='' cd -- "$here/../.." && pwd)

target=lab
tag=cactup-tutorial
while [ $# -gt 0 ]; do
    case $1 in
        --target) target=$2; shift 2 ;;
        --tag) tag=$2; shift 2 ;;
        *) break ;;
    esac
done

# Keep in sync with the stamp job in .github/workflows/ci.yml.
inputs="src build.rs Cargo.toml Cargo.lock resources mdb/GENERATION mdb/generic"
# shellcheck disable=SC2086
stamps=$(git -C "$repo" log -2 --format='%h %cI' --abbrev=7 -- $inputs)
current_id=$(printf '%s\n' "$stamps" | sed -n 1p | cut -d' ' -f1)
current_date=$(printf '%s\n' "$stamps" | sed -n 1p | cut -d' ' -f2)
previous_id=$(printf '%s\n' "$stamps" | sed -n 2p | cut -d' ' -f1)
previous_date=$(printf '%s\n' "$stamps" | sed -n 2p | cut -d' ' -f2)
[ -n "$previous_id" ] || { echo "build.sh: need two commits touching the cactup sources" >&2; exit 1; }

echo "build.sh: cactup previous $previous_id ($previous_date), current $current_id ($current_date)"
exec docker build \
    -f "$here/Dockerfile" \
    --target "$target" \
    -t "$tag:$target" \
    --build-arg CACTUP_PREVIOUS_ID="$previous_id" \
    --build-arg CACTUP_PREVIOUS_DATE="$previous_date" \
    --build-arg CACTUP_CURRENT_ID="$current_id" \
    --build-arg CACTUP_CURRENT_DATE="$current_date" \
    "$@" \
    "$repo"
