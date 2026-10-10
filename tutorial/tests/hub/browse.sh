#!/bin/sh
# Look at a running local hub (run.sh --keep) in headless Chromium:
# browse.py, in the Playwright container on the hub's front network.
# Screenshots go to OUTDIR.
#
#   tutorial/tests/hub/browse.sh [OUTDIR]
set -eu

here=$(cd "$(dirname "$0")" && pwd)
out=${1:-$here/out}
work=${CACTUP_HUBTEST_DIR:-$HOME/tmp/cactup-hubtest}
mkdir -p "$out"
docker build -q -t cactup-tutorial-playwright -f "$here/../browser/Dockerfile.playwright" "$here/../browser" > /dev/null
status=0
docker run --rm --network cactup-hubtest_front -v "$out:/out" -v "$here:/check:ro" cactup-tutorial-playwright \
    python /check/browse.py http://caddy-plain "$(cat "$work/shared/session-token")" > "$out/browse.log" 2>&1 || true
cat "$out/browse.log"
grep -q '^0 problem(s)' "$out/browse.log" || status=1
# What the terminal ran (browse.py names the user): DockerSpawner's container
# name spells a dash "-2d".
user=$(sed -n 's/^user //p' "$out/browse.log")
if [ -n "$user" ] && [ "$(docker exec "cactup-$(echo "$user" | sed 's/-/-2d/g')" \
        cat /home/cactus/.browse-terminal-check 2>/dev/null)" = cactup-tutorial ]; then
    echo "ok   a terminal runs commands"
else
    echo "FAIL a terminal runs commands"
    status=1
fi
exit $status
