#!/bin/sh
# Build the Cactup tutorial image.
#
# Usage: tutorial/image/build.sh [--target STAGE] [--tag TAG] [--update-mirrors]
#                                [docker build args...]
#
# The git mirrors the image serves installs from are kept outside Docker, in
# $CACTUP_TUTORIAL_MIRRORS (default ~/.cache/cactup-tutorial/mirrors). Each
# build first brings them to the committed lock, tutorial/mirrors/mirrors.lock,
# so every build serves the same commits; --update-mirrors moves them to
# upstream's tips instead and rewrites the lock (commit it on purpose: it
# changes what the bakes were built from).
#
# DEBIAN_SNAPSHOT overrides the Dockerfile's snapshot.debian.org date.
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
update=no
while [ $# -gt 0 ]; do
    case $1 in
        --target) target=$2; shift 2 ;;
        --tag) tag=$2; shift 2 ;;
        --update-mirrors) update=yes; shift ;;
        *) break ;;
    esac
done

mirrors=${CACTUP_TUTORIAL_MIRRORS:-${XDG_CACHE_HOME:-$HOME/.cache}/cactup-tutorial/mirrors}
lock=$here/../mirrors/mirrors.lock
if [ "$update" = yes ]; then
    python3 "$here/../mirrors/mirror.py" sync --root "$mirrors" --rule-root /opt/cactup-mirrors --lock "$lock"
else
    python3 "$here/../mirrors/mirror.py" pin --root "$mirrors" --rule-root /opt/cactup-mirrors --lock "$lock"
fi

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
    ${DEBIAN_SNAPSHOT:+--build-arg DEBIAN_SNAPSHOT="$DEBIAN_SNAPSHOT"} \
    --build-context mirrors="$mirrors" \
    "$@" \
    "$repo"
