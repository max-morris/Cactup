#!/bin/sh
# Frontend check: start a tutorial container and a Playwright container on a
# private network, run check_frontend.py, and leave screenshots in OUTDIR.
#
# Usage: tutorial/tests/browser/run.sh [IMAGE] [OUTDIR]
set -eu

here=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
image=${1:-cactup-tutorial:lab}
out=${2:-$here/out}
net=cactup-tutorial-browser-$$
lab=cactup-tutorial-lab-$$
token=browser-check

mkdir -p "$out"
work=$(mktemp -d)
cp "$here/languages.ipynb" "$work/"
chmod -R a+rwX "$work"

cleanup() {
    docker rm -f "$lab" > /dev/null 2>&1 || true
    docker network rm "$net" > /dev/null 2>&1 || true
    # Files the notebook server wrote belong to the container's user, which
    # under rootless Docker is a subordinate uid the host user can't delete.
    docker run --rm --user 0 --entrypoint sh -v "$work:/work" "$image" \
        -c 'rm -rf /work/* /work/.[!.]*' > /dev/null 2>&1 || true
    rm -rf "$work"
}
trap cleanup EXIT INT TERM

docker network create "$net" > /dev/null
docker run -d --rm --name "$lab" --network "$net" --network-alias lab \
    --hostname cactup-tutorial -e CACTUP_TUTORIAL_TOKEN="$token" \
    -v "$work:/home/cactus/tutorial" "$image" > /dev/null

docker build -q -t cactup-tutorial-playwright -f "$here/Dockerfile.playwright" "$here" > /dev/null
docker run --rm --network "$net" -v "$out:/out" -v "$here:/check:ro" cactup-tutorial-playwright \
    sh -c "for i in \$(seq 60); do curl -fs http://lab:8888/api > /dev/null && break; sleep 1; done; \
           python /check/check_frontend.py 'http://lab:8888/lab/tree/tutorial/languages.ipynb?token=$token'"
