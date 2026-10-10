#!/bin/sh
# Bring up the tutorial's hub on this machine and check it end to end
# (check_hub.py): sign-ups with the session token, refusals, every notebook
# run in two attendees' containers at once, and a restart.
#
#   tutorial/tests/hub/run.sh [--keep] [check_hub.py options, e.g. --only 1,2]
#   tutorial/tests/hub/run.sh --down     (remove what a --keep run left)
#
# It uses a compose project, network and port of its own (cactup-hubtest,
# cactup-hubtest-users, plain HTTP on 127.0.0.1:18000: the setup behind a
# site's proxy), and a shared directory under ~/tmp, so it doesn't meet a real
# deployment on the same machine. Run one at a time: two share all of that.
# With rootless Docker, cpusets are off (it ignores them) and the token file's
# group is the container's root group. Everything it made is
# removed at the end (containers, networks, volumes), unless --keep: then
# browse.sh can look at the hub, and `run.sh --down` removes it all.
set -eu

here=$(cd "$(dirname "$0")" && pwd)
deploy=$here/../../deploy
work=${CACTUP_HUBTEST_DIR:-$HOME/tmp/cactup-hubtest}
keep= down=
if [ "${1:-}" = --keep ]; then keep=yes; shift; fi
if [ "${1:-}" = --down ]; then down=yes; fi

mkdir -p "$work/shared"
if docker info --format '{{.SecurityOptions}}' | grep -q rootless; then
    socket=${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/docker.sock cpuset=0 gid=0
else
    socket=/var/run/docker.sock cpuset=1 gid=$(id -g)
fi
cat > "$work/test.env" <<EOF
COMPOSE_PROFILES=plain
PLAIN_BIND=127.0.0.1
PLAIN_PORT=18000
SECURE_COOKIES=0
USER_NETWORK=cactup-hubtest-users
SHARED_DIR=$work/shared
SHARED_GID=$gid
HUB_ADMINS=
DOCKER_SOCKET=$socket
CPUSET=$cpuset
EOF

compose() { docker compose -p cactup-hubtest --env-file "$work/test.env" -f "$deploy/compose.yaml" "$@"; }
cleanup() {
    [ -n "$keep" ] && return
    # The attendees' containers and volumes are the spawner's, not compose's:
    # the ones check_hub.py (check1a2b-0) and browse.sh (browse1a2b) signed
    # up, matched exactly: a real attendee may be named "checkers".
    docker ps -aq -f 'name=^cactup-check[0-9a-f]{4}-2d[0-9]+$' -f 'name=^cactup-browse[0-9a-f]{4}$' |
        xargs -r docker rm -f > /dev/null
    compose down -v > /dev/null 2>&1 || true
    docker volume ls -q -f 'name=^cactup-(home|slurm)-check[0-9a-f]{4}-2d[0-9]+$' \
        -f 'name=^cactup-(home|slurm)-browse[0-9a-f]{4}$' | xargs -r docker volume rm > /dev/null
}
trap cleanup EXIT
[ -z "$down" ] || exit 0

compose up -d --build
for _ in $(seq 60); do
    [ -s "$work/shared/session-token" ] && curl -fs http://127.0.0.1:18000/hub/login > /dev/null && break
    sleep 1
done
python3 "$here/check_hub.py" --url http://127.0.0.1:18000 --shared "$work/shared" \
    --hub-container cactup-hubtest-hub-1 "$@"
