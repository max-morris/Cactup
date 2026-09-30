#!/bin/sh
# Build replay check: in a fresh tutorial container, as an attendee would,
#   - install cactup and the ET_2026_05_v0 release, and build the `tutorial`
#     config: the shim restores the B1 bake and replays its output;
#   - check that the replay takes about 45 s and never pauses for more than
#     2 s, and that its output is the bake's real build's, cactup's own lines
#     included (only the build id's timestamp and pid, and make's jobserver
#     fifo, named after its pid, may differ);
#   - run the restored executable on the tutorial's linear-wave test;
#   - edit one source file and build: a real incremental build compiling
#     only that file; revert it and build again (the same file, for real);
#     then build once more: up to date.
#
# Usage: tutorial/tests/platform/replay.sh [IMAGE]
set -eu

image=${1:-cactup-tutorial:tutorial}
name=cactup-tutorial-replay-$$
net=cactup-tutorial-replay-net-$$
logs=$(mktemp -d)
fail=0
thornlist=/opt/cactup-tutorial/thornlists/tutorial.th
thorn=Cottonmouth/CottonmouthZ4c4m
file=src/CottonmouthZ4c4m_sync_state.cpp

check() {
    if [ "$1" = ok ]; then
        printf 'ok   %s\n' "$2"
    else
        printf 'FAIL %s\n' "$2"
        fail=1
    fi
}

as_user() {
    docker exec -u cactus -w /home/cactus -e HOME=/home/cactus \
        -e PATH=/home/cactus/.cactup/bin:/opt/venv/bin:/usr/local/bin:/usr/bin:/bin "$name" bash -lc "umask 022; $1"
}

# Runs a command as the tutorial user, writing its stdout and stderr to
# $logs/$1.out and .err, and prints "<status> <seconds> <longest pause>": the
# longest time with no output at all, from the start to the end.
timed() {
    docker exec -i -u cactus -w /home/cactus -e HOME=/home/cactus \
        -e PATH=/home/cactus/.cactup/bin:/opt/venv/bin:/usr/local/bin:/usr/bin:/bin "$name" \
        python3 - "$2" > "$logs/$1.json" <<'EOF'
import json, os, selectors, subprocess, sys, time
start = time.monotonic()
proc = subprocess.Popen(["bash", "-c", "umask 022; " + sys.argv[1]], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
sel = selectors.DefaultSelector()
sel.register(proc.stdout, selectors.EVENT_READ, "out")
sel.register(proc.stderr, selectors.EVENT_READ, "err")
got = {"out": b"", "err": b""}
last, longest, open_ = start, 0.0, 2
while open_:
    for key, _ in sel.select():
        chunk = os.read(key.fileobj.fileno(), 65536)
        now = time.monotonic()
        if not chunk:
            sel.unregister(key.fileobj)
            open_ -= 1
            continue
        longest = max(longest, now - last)
        last = now
        got[key.data] += chunk
status = proc.wait()
end = time.monotonic()
longest = max(longest, end - last)
print(json.dumps({"status": status, "seconds": end - start, "pause": longest,
                  "out": got["out"].decode(errors="replace"), "err": got["err"].decode(errors="replace")}))
EOF
    python3 - "$logs/$1" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1] + ".json"))
open(sys.argv[1] + ".out", "w").write(r["out"])
open(sys.argv[1] + ".err", "w").write(r["err"])
print(r["status"], round(r["seconds"], 1), round(r["pause"], 1))
EOF
}

cleanup() {
    docker rm -f "$name" > /dev/null 2>&1 || true
    docker network rm "$net" > /dev/null 2>&1 || true
    rm -rf "$logs"
}
trap cleanup EXIT INT TERM

docker network create --internal "$net" > /dev/null
docker run -d --name "$name" --network "$net" --hostname cactup-tutorial --cpus 4 \
    -e CACTUP_TUTORIAL_TOKEN=replay "$image" > /dev/null
i=0
until docker exec "$name" curl -fs http://127.0.0.1:8888/api > /dev/null 2>&1; do
    i=$((i + 1))
    if [ "$i" -gt 120 ]; then
        docker logs "$name" 2>&1 | tail -20
        echo "the notebook server did not start"
        exit 1
    fi
    sleep 1
done

as_user 'curl -sSf http://127.0.0.1:8765/cactup-init.sh | CACTUP_UPDATE_ROOT=http://127.0.0.1:8765 sh -s -- -y && ~/.cactup/bin/cactup update' \
    > "$logs/cactup.log" 2>&1 && r=ok || r=no
current=$(docker exec "$name" jq -r .build /opt/cactup-tutorial/update-root/latest.json)
as_user 'cactup --version' 2>&1 | grep -q "$current" || r=no
check "$r" "cactup is installed and updated to the current build ($current)"
as_user 'cactup install ET_2026_05_v0 --silent' > "$logs/install.log" 2>&1 && r=ok || r=no
check "$r" "ET_2026_05_v0 is installed from the mirrors"

set -- $(timed build "cactup build tutorial --thornlist $thornlist")
status=$1 seconds=$2 pause=$3
[ "$status" = 0 ] && r=ok || r=no
check "$r" "cactup build tutorial succeeds (status $status)"
[ "$r" = ok ] || tail -20 "$logs/build.err"
awk -v s="$seconds" 'BEGIN { exit !(s >= 40 && s <= 55) }' && r=ok || r=no
check "$r" "the replayed build takes 40-55 s ($seconds s)"
awk -v p="$pause" 'BEGIN { exit !(p <= 2) }' && r=ok || r=no
check "$r" "its output never pauses for more than 2 s (longest: $pause s)"
grep -q 'not precomputed' "$logs/build.err" && r=no || r=ok
check "$r" "the shim found the bake (no \"not precomputed\" note)"

# The bake's own cactup transcript, against this one.
bake=$(docker exec "$name" sh -c 'ls -d /opt/cactup-bakes/*/ | head -1')
for stream in out err; do
    docker exec "$name" cat "$bake/transcript.$stream" > "$logs/bake.$stream"
    for f in "$logs/bake.$stream" "$logs/build.$stream"; do
        sed -E -e 's/-[0-9]{4}\.[0-9]{2}\.[0-9]{2}-[0-9]{2}\.[0-9]{2}\.[0-9]{2}-[0-9]+\./-DATE-PID./g' \
            -e 's/GMfifo[0-9]+/GMfifoPID/g' "$f" > "$f.norm"
    done
    if diff "$logs/bake.$stream.norm" "$logs/build.$stream.norm" > "$logs/diff.$stream"; then
        r=ok
    else
        r=no
        head -20 "$logs/diff.$stream"
    fi
    check "$r" "the replay's std$stream is the real build's ($(wc -l < "$logs/build.$stream") lines)"
done

# What an attendee can look at agrees with a build that just happened: the
# banner's compile date, and object mtimes spread over the build.
banner=$(as_user '~/Cactus/exe/cactus_tutorial -v 2>&1 | sed -n "s/.*Compiled on \(.*\) at \(.*\)/\1 \2/p" | head -1' || true)
compiled=$(as_user "date -d '$banner' +%s" 2>/dev/null || echo 0)
linked=$(as_user 'date -r ~/Cactus/exe/cactus_tutorial +%s')
[ $((linked - compiled)) -ge -5 ] && [ $((linked - compiled)) -le 5 ] && r=ok || r=no
check "$r" "the banner's compile time is this build's, not the bake's ($banner)"
spread=$(as_user 'find ~/Cactus/configs/tutorial/build -name "*.o" -printf "%TT\n" | cut -c1-8 | sort -u | wc -l')
[ "$spread" -ge 10 ] && r=ok || r=no
check "$r" "restored objects' mtimes spread over the build ($spread distinct seconds)"

info=$(as_user 'cat ~/Cactus/configs/tutorial/config-info')
attempt=$(as_user 'ls -d ~/.cactup/cacti/ET_2026_05_v0/Cactus/configs/tutorial/.cactup-builds/* | tail -1')
configured=$(printf '%s\n' "$info" | sed -n 's/^# CONFIG-DATE *: \(.*\) (GMT)$/\1/p')
configured=$(as_user "date -u -d '$configured' +%s" 2>/dev/null || echo 0)
[ $((linked - configured)) -ge 0 ] && [ $((linked - configured)) -le 120 ] && r=ok || r=no
case $info in *"$attempt/cactup-thornlist.th"*) ;; *) r=no ;; esac
check "$r" "config-info names this build's attempt and configure date"

# The restored executable runs the tutorial's linear-wave test.
as_user "mkdir -p ~/replay-run && cd ~/replay-run && timeout 300 mpirun -n 2 ~/Cactus/exe/cactus_tutorial \
    ~/Cactus/arrangements/$thorn/test/linear_wave_z4c.par" > "$logs/run.log" 2>&1 && r=ok || r=no
grep -q 'Done\.' "$logs/run.log" || r=no
check "$r" "the restored executable runs the linear-wave test on 2 ranks"
[ "$r" = ok ] || tail -20 "$logs/run.log"

# A real incremental build on the restored tree, and back.
as_user "echo '// an edit' >> ~/Cactus/arrangements/$thorn/$file"
set -- $(timed edit "cactup build tutorial")
compiled=$(grep -c '^COMPILING' "$logs/edit.out" || true)
[ "$1" = 0 ] && [ "$compiled" = 1 ] && grep -q "COMPILING $thorn/$file" "$logs/edit.out" && r=ok || r=no
check "$r" "after an edit, a real build compiles only that file ($compiled compiled, $2 s)"
awk -v s="$2" 'BEGIN { exit !(s <= 120) }' && r=ok || r=no
check "$r" "the incremental build takes at most 2 minutes ($2 s)"

as_user "git -C ~/Cactus/arrangements/$thorn checkout -- $file"
set -- $(timed revert "cactup build tutorial")
compiled=$(grep -c '^COMPILING' "$logs/revert.out" || true)
[ "$1" = 0 ] && [ "$compiled" = 1 ] && r=ok || r=no
check "$r" "after reverting, the build is real and compiles that file again ($compiled compiled, $2 s)"
grep -q 'not precomputed' "$logs/edit.err" "$logs/revert.err" && r=no || r=ok
check "$r" "no \"not precomputed\" note for incremental builds"

set -- $(timed again "cactup build tutorial")
grep -q 'is up to date' "$logs/again.out" "$logs/again.err" && r=ok || r=no
check "$r" "building once more: up to date ($2 s)"

[ "$fail" -eq 0 ] && echo "0 failure(s)" || echo "failures"
exit "$fail"
