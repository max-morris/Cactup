#!/bin/sh
# Build the Cactup tutorial image.
#
# Usage: tutorial/image/build.sh [--target STAGE] [--tag TAG] [--update-mirrors]
#                                [docker build args...]
#
# By default it builds the whole image, cactup-tutorial:tutorial: first the lab
# image (cactup-tutorial:lab), then the bakes, in a bake container started from
# it, then the lab image plus the bakes. --target lab (or an earlier stage)
# stops before the bakes.
#
# The bakes are cached outside Docker, in $CACTUP_TUTORIAL_BAKES (default
# ~/.cache/cactup-tutorial/bakes), per toolchain and fingerprint: a rebuild
# only rebakes what changed (see "Bakes" in tutorial/README.md).
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
# cactup itself comes from the commit tutorial/cactup.pin names, not from the
# checkout: build.sh extracts that commit's cactup sources, installer
# (cactup-init.sh) and machine database (mdb/) with `git archive` into a
# snapshot the image and the MDB mirror are built from. Moving the pin is a
# procedure of its own: see tutorial/UPDATING.md.
#
# The cactup build stamps come from git at the pinned commit, the way CI
# computes them: "current" is the last commit that touched the binary's
# inputs, "previous" the one before it. Both binaries are built from the
# pinned sources; "previous" only carries the older stamp, which is all the
# auto-update demo needs.
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
repo=$(CDPATH='' cd -- "$here/../.." && pwd)

target=tutorial
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

# The pinned cactup: a snapshot of its commit's files, removed on exit.
pin=$(sed -n 's/^commit[[:space:]]*=[[:space:]]*\([0-9a-f]\{40\}\)[[:space:]]*$/\1/p' "$here/../cactup.pin")
[ -n "$pin" ] || { echo "build.sh: tutorial/cactup.pin names no commit" >&2; exit 1; }
git -C "$repo" cat-file -e "$pin^{commit}" 2>/dev/null \
    || { echo "build.sh: the pinned commit $pin is not in this repository (fetch it)" >&2; exit 1; }
snapshot=$(mktemp -d "${TMPDIR:-/tmp}/cactup-tutorial-pin.XXXXXX")
trap 'rm -rf "$snapshot"' EXIT
git -C "$repo" archive "$pin" -- Cargo.toml Cargo.lock build.rs cactupdocs/Cargo.toml cactupdocs/src \
    src resources mdb cactup-init.sh | tar -x -C "$snapshot"
echo "build.sh: cactup pinned at $(git -C "$repo" log -1 --format='%h %s' "$pin")"

mirrors=${CACTUP_TUTORIAL_MIRRORS:-${XDG_CACHE_HOME:-$HOME/.cache}/cactup-tutorial/mirrors}
lock=$here/../mirrors/mirrors.lock
if [ "$update" = yes ]; then
    python3 "$here/../mirrors/mirror.py" sync --root "$mirrors" --rule-root /opt/cactup-mirrors --lock "$lock" \
        --mdb-dir "$snapshot/mdb"
else
    python3 "$here/../mirrors/mirror.py" pin --root "$mirrors" --rule-root /opt/cactup-mirrors --lock "$lock" \
        --mdb-dir "$snapshot/mdb"
fi

# Keep in sync with the stamp job in .github/workflows/ci.yml.
inputs="src build.rs Cargo.toml Cargo.lock resources mdb/GENERATION mdb/generic"
# shellcheck disable=SC2086
stamps=$(git -C "$repo" log -2 --format='%h %cI' --abbrev=7 "$pin" -- $inputs)
current_id=$(printf '%s\n' "$stamps" | sed -n 1p | cut -d' ' -f1)
current_date=$(printf '%s\n' "$stamps" | sed -n 1p | cut -d' ' -f2)
previous_id=$(printf '%s\n' "$stamps" | sed -n 2p | cut -d' ' -f1)
previous_date=$(printf '%s\n' "$stamps" | sed -n 2p | cut -d' ' -f2)
[ -n "$previous_id" ] || { echo "build.sh: need two commits touching the cactup sources" >&2; exit 1; }

echo "build.sh: cactup previous $previous_id ($previous_date), current $current_id ($current_date)"

bakes=${CACTUP_TUTORIAL_BAKES:-${XDG_CACHE_HOME:-$HOME/.cache}/cactup-tutorial/bakes}
mkdir -p "$bakes"
context=$bakes/.context

build() {
    stage=$1
    shift
    docker build \
        -f "$here/Dockerfile" \
        --target "$stage" \
        -t "$tag:$stage" \
        --build-arg CACTUP_PREVIOUS_ID="$previous_id" \
        --build-arg CACTUP_PREVIOUS_DATE="$previous_date" \
        --build-arg CACTUP_CURRENT_ID="$current_id" \
        --build-arg CACTUP_CURRENT_DATE="$current_date" \
        ${DEBIAN_SNAPSHOT:+--build-arg DEBIAN_SNAPSHOT="$DEBIAN_SNAPSHOT"} \
        --build-context mirrors="$mirrors" \
        --build-context cactup-src="$snapshot" \
        --build-context bakes="$context" \
        "$@"
}

# (Every build names the bakes context; only the last stage reads it.)
mkdir -p "$context"
if [ "$target" != tutorial ]; then
    build "$target" "$@" "$repo"
    exit
fi

build lab "$@" "$repo"

# The bakes: real installs and builds in a container identical to an
# attendee's (hostname, user, paths, mirrors), with an empty home of its own.
# Only what the bakes need; no network (installs come from the mirrors).
echo "build.sh: baking"
bake() {
    docker run --rm --hostname cactup-tutorial --network none \
        --mount type=volume,dst=/home/cactus,volume-nocopy \
        -v "$here/../bake:/bake:ro" -v "$bakes:/bakes" \
        "$@"
}
bake --entrypoint python3 "$tag:lab" /bake/bake.py B1 B2a B2b B4 B5 B6 --wanted wanted-cpu

# The GPU bake: the same lab image, with the CUDA toolkit mounted from a
# container of the cuda image (kept between builds while the image is the
# same, so its volume is made once).
build cuda "$@" "$repo"
source=cactup-tutorial-cuda
if [ "$(docker inspect -f '{{.Image}}' "$source" 2>/dev/null)" != "$(docker image inspect -f '{{.Id}}' "$tag:cuda")" ]; then
    docker rm -f -v "$source" > /dev/null 2>&1 || true
    docker create --name "$source" "$tag:cuda" true > /dev/null
fi
bake --volumes-from "$source:ro" --entrypoint python3 "$tag:lab" /bake/bake.py B3 --wanted wanted-cuda

toolchain=$(cat "$bakes/last-toolchain")
rm -rf "$context"
mkdir -p "$context"
cat "$bakes/$toolchain/wanted-cpu" "$bakes/$toolchain/wanted-cuda" | while read -r fp; do
    cp -al "$bakes/$toolchain/$fp" "$context/$fp"
done

build tutorial "$@" "$repo"
