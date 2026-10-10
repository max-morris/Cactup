#!/bin/sh
# Platform smoke test: start a tutorial container the way an attendee's is
# started, then, as the tutorial user,
#   - install cactup with the documented installer from the local update site
#     and check that it is the "previous" build;
#   - run a cactup command on a terminal and check that it updated itself to
#     "current";
#   - check that SLURM is up with the node the MDB entry describes, and run a
#     trivial job through it;
#   - install the ET_2026_05_v0 release from the git mirrors (with origins
#     still naming the upstream URLs), check that the machine is discovered as
#     cactup-tutorial, and submit a simulation through cactup and SLURM (its
#     executable a stand-in: real builds come from the bakes).
#
# Usage: tutorial/tests/platform/smoke.sh [IMAGE]
set -eu

image=${1:-cactup-tutorial:lab}
name=cactup-tutorial-smoke-$$
net=cactup-tutorial-smoke-net-$$
logs=$(mktemp -d)
fail=0

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

cleanup() {
    docker rm -f "$name" > /dev/null 2>&1 || true
    docker network rm "$net" > /dev/null 2>&1 || true
    rm -rf "$logs"
}
trap cleanup EXIT INT TERM

# The image must not carry a munge key: each container makes its own.
docker run --rm --entrypoint sh "$image" -c 'test ! -e /etc/munge/munge.key' && r=ok || r=no
check "$r" "the image carries no munge key"

# No route to the internet: everything must come from the image.
docker network create --internal "$net" > /dev/null
docker run -d --name "$name" --network "$net" --hostname cactup-tutorial -e CACTUP_TUTORIAL_TOKEN=smoke "$image" > /dev/null
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
check ok "the container started (hostname, update site, munge, SLURM, JupyterLab)"

ids=$(docker exec "$name" sh -c 'cat /opt/cactup-tutorial/update-root/*/cactup.sha256; jq -r .build /opt/cactup-tutorial/update-root/latest.json')
previous=$(printf '%s\n' "$ids" | sed -n '1s/.*cactup-//p')
current=$(printf '%s\n' "$ids" | sed -n 2p)
[ -n "$previous" ] && [ -n "$current" ] && [ "$previous" != "$current" ] && r=ok || r=no
check "$r" "the update site offers previous ($previous) to the installer and current ($current) to updates"

as_user 'command -v cactup' > /dev/null 2>&1 && r=no || r=ok
check "$r" "cactup is not preinstalled"

as_user 'curl -sSf http://127.0.0.1:8765/cactup-init.sh | CACTUP_UPDATE_ROOT=http://127.0.0.1:8765 sh -s -- -y' > /dev/null
installed=$(as_user 'cactup --version' 2>&1 || true)
case $installed in *"$previous"*) r=ok ;; *) r=no ;; esac
check "$r" "the documented installer installs previous (got: $installed)"

# The update check runs only when stderr is a terminal.
as_user 'script -qec "cactup list" /dev/null' > /dev/null 2>&1 || true
updated=$(as_user 'cactup --version' 2>&1 || true)
case $updated in *"$current"*) r=ok ;; *) r=no ;; esac
check "$r" "the first command on a terminal updates to current (got: $updated)"

node=$(docker exec "$name" scontrol show node cactup-tutorial 2>&1)
case $node in *"CPUTot=4"*"State=IDLE"*) r=ok ;; *) r=no ;; esac
check "$r" "SLURM's node has 4 CPUs and is idle"
partitions=$(docker exec "$name" sinfo -h -o '%P %l' | tr '\n' ' ')
case $partitions in *"debug* 30:00"*"short 3:00"*"batch 4:00:00"*"gpu 4:00:00"*) r=ok ;; *) r=no ;; esac
check "$r" "partitions debug (default), short, batch, gpu (got: $partitions)"

state=$(docker exec "$name" sinfo -h -p gpu -o %a 2>&1)
case $state in inact*) r=ok ;; *) r=no ;; esac
check "$r" "without a GPU the gpu partition is inactive (got: $state)"
as_user 'sbatch -p short -t 5:00 --wrap true' > /dev/null 2>&1 && r=no || r=ok
check "$r" "a job longer than its partition allows is refused"

as_user 'sbatch -W -p debug -n 2 -t 1:00 -o smoke.out --wrap "echo job \$SLURM_JOB_ID ran on \$(hostname) with \$SLURM_NTASKS tasks"' > /dev/null 2>&1 || true
out=$(as_user 'cat smoke.out' 2>&1 || true)
case $out in *"ran on cactup-tutorial with 2 tasks"*) r=ok ;; *) r=no ;; esac
check "$r" "a job runs through SLURM (got: $out)"

cpus=$(docker exec "$name" sh -c 'for p in /proc/[0-9]*; do tr "\0" "\n" < "$p/environ"; done 2>/dev/null' |
    sed -n 's/^CACTUP_TUTORIAL_CPUS=//p' | head -1)
[ -n "$cpus" ] && r=ok || r=no
check "$r" "the notebook server knows the container's CPUs (CACTUP_TUTORIAL_CPUS=$cpus)"

started=$(date +%s)
as_user 'cactup install ET_2026_05_v0 --silent' > $logs/install.log 2>&1 && r=ok || r=no
check "$r" "cactup install ET_2026_05_v0 from the mirrors ($(( $(date +%s) - started )) s)"
[ "$r" = ok ] || tail -20 $logs/install.log

origin=$(as_user 'git -C ~/.cactup/cacti/ET_2026_05_v0/Cactus/repos/flesh config --get remote.origin.url || git -C "$(ls -d ~/.cactup/cacti/ET_2026_05_v0/Cactus/repos/* | head -1)" config --get remote.origin.url' 2>&1 || true)
case $origin in https://*) r=ok ;; *) r=no ;; esac
check "$r" "installed repositories still name their upstream (origin: $origin)"

tips=$(as_user 'cactup knob wisdom-frequency' 2>&1 || true)
case $tips in *off*) r=ok ;; *) r=no ;; esac
check "$r" "cactup's random tips are off in the notebooks (wisdom-frequency: $tips)"

machine=$(as_user 'cactup machine show' 2>&1 | sed -n 1,5p || true)
case $machine in *cactup-tutorial*) r=ok ;; *) r=no ;; esac
check "$r" "the machine is discovered as cactup-tutorial"

as_user 'printf "#!/bin/sh
echo stand-in cactus \$@ rank \${OMPI_COMM_WORLD_RANK:-?}
" > ~/fake-cactus && chmod +x ~/fake-cactus &&
         printf "ActiveThorns = ""
" > ~/smoke.par &&
         cactup build smoke --thornlist /opt/cactup-tutorial/thornlists/tutorial.th --virtual-executable ~/fake-cactus' \
    > $logs/build.log 2>&1 && r=ok || r=no
check "$r" "a config (stand-in executable) builds"
[ "$r" = ok ] || tail -20 $logs/build.log

as_user 'cactup sim submit smoke-sim ~/smoke.par --config smoke -T 2 -w 00:02:00 -s' > $logs/submit.log 2>&1 && r=ok || r=no
check "$r" "cactup sim submit goes through SLURM"
[ "$r" = ok ] || tail -20 $logs/submit.log
i=0
while as_user 'squeue -h' 2>/dev/null | grep -q .; do
    i=$((i + 1)); [ "$i" -gt 60 ] && break; sleep 2
done
log=$(as_user 'cactup sim log smoke-sim' 2>&1 || true)
case $log in *"stand-in cactus"*"rank 1"*|*"rank 1"*"stand-in cactus"*) r=ok ;; *) r=no ;; esac
check "$r" "the simulation ran on 2 MPI ranks (log: $(printf '%s' "$log" | grep -m2 stand-in | tr '\n' ' '))"

# A job running when the container stops must not come back as a ghost that
# holds the node: SLURM requeues it once the container is up again, and it
# runs again at once. Three times: the container's start races slurmd's
# registration, and one pass could miss a failure.
for round in 1 2 3; do
    job=$(as_user 'sbatch --parsable -p debug -n 4 -t 5:00 --wrap "sleep 300"' 2>/dev/null || true)
    later=$(as_user 'sbatch --parsable -p debug -n 1 -t 1:00 --begin=now+2hours --wrap true' 2>/dev/null || true)
    i=0
    until docker exec "$name" squeue -h -j "$job" -o %T 2>/dev/null | grep -q RUNNING; do
        i=$((i + 1)); [ "$i" -gt 30 ] && break; sleep 1
    done
    # queued behind it, waiting for the node
    waiter=$(as_user 'sbatch --parsable -p debug -n 4 -t 1:00 --wrap true' 2>/dev/null || true)
    docker restart "$name" > /dev/null
    i=0
    until docker exec "$name" curl -fs http://127.0.0.1:8888/api > /dev/null 2>&1; do
        i=$((i + 1)); [ "$i" -gt 120 ] && break; sleep 1
    done
    [ "$(docker inspect -f '{{.State.Running}}' "$name")" = true ] && r=ok || r=no
    check "$r" "restart $round with a job running: the container comes up"
    restarts=$(docker exec "$name" scontrol show job "$job" 2>&1 | sed -n 's/.*Restarts=\([0-9]*\).*/\1/p')
    i=0
    until docker exec "$name" squeue -h -j "$job" -o %T 2>/dev/null | grep -q RUNNING; do
        i=$((i + 1)); [ "$i" -gt 20 ] && break; sleep 1
    done
    [ "${restarts:-0}" -ge 1 ] && [ "$i" -le 20 ] && r=ok || r=no
    check "$r" "restart $round: the interrupted job is requeued and runs again at once (Restarts=${restarts:-?}, ${i} s)"
    state=$(docker exec "$name" squeue -h -j "$waiter" -o %T 2>&1)
    [ "$state" = PENDING ] && r=ok || r=no
    check "$r" "restart $round: the job that was waiting behind it still waits (got: $state)"
    reason=$(docker exec "$name" squeue -h -j "$later" -o '%T %r' 2>&1)
    [ "$reason" = "PENDING BeginTime" ] && r=ok || r=no
    check "$r" "restart $round: a job delayed on purpose (--begin) keeps its start time (got: $reason)"
    as_user "scancel $job $later $waiter" > /dev/null 2>&1 || true
done

# A node slurmd drained (its spool disk filled up, say) stays drained across
# restarts; the container must still come up, with the node usable again.
docker exec "$name" scontrol update NodeName=cactup-tutorial State=DRAIN Reason=smoke > /dev/null 2>&1 || true
docker restart "$name" > /dev/null
i=0
until docker exec "$name" curl -fs http://127.0.0.1:8888/api > /dev/null 2>&1; do
    i=$((i + 1)); [ "$i" -gt 120 ] && break; sleep 1
done
node=$(docker exec "$name" sinfo -h -n cactup-tutorial -o %T 2>&1 | head -1)
case $node in idle|mixed|allocated) r=ok ;; *) r=no ;; esac
check "$r" "a drained node doesn't keep the container from starting (node: $node)"

# JupyterLab shut down from its own menu (File > Shut Down) right after a
# submit: SLURM must still have saved its state, so job ids don't repeat.
before=$(as_user 'sbatch --parsable -p debug -t 1:00 --wrap true' 2>/dev/null || true)
docker exec "$name" curl -fs -X POST -H 'Authorization: token smoke' http://127.0.0.1:8888/api/shutdown > /dev/null 2>&1 || true
i=0
while [ "$(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null)" = true ]; do
    i=$((i + 1)); [ "$i" -gt 30 ] && break; sleep 1
done
docker start "$name" > /dev/null
i=0
until docker exec "$name" curl -fs http://127.0.0.1:8888/api > /dev/null 2>&1; do
    i=$((i + 1)); [ "$i" -gt 120 ] && break; sleep 1
done
after=$(as_user 'sbatch --parsable -p debug -t 1:00 --wrap true' 2>/dev/null || true)
[ -n "$before" ] && [ -n "$after" ] && [ "$after" -gt "$before" ] && r=ok || r=no
check "$r" "job ids keep counting after JupyterLab shuts itself down ($before, then $after)"

# A home that lacks the seeding marker (restored by an older tool, say) must
# keep everything in it when the container starts again.
as_user 'rm -f ~/.cactup-tutorial-seeded' > /dev/null 2>&1 || true
docker restart "$name" > /dev/null
i=0
until docker exec "$name" curl -fs http://127.0.0.1:8888/api > /dev/null 2>&1; do
    i=$((i + 1)); [ "$i" -gt 120 ] && break; sleep 1
done
kept=$(as_user 'cactup list; grep -c cactup ~/.bashrc' 2>&1 || true)
case $kept in *ET_2026_05_v0*) r=ok ;; *) r=no ;; esac
case $kept in *"No installations"*) r=no ;; esac
[ "$(printf '%s\n' "$kept" | tail -1)" -ge 1 ] 2>/dev/null || r=no
check "$r" "seeding a home again keeps the attendee's cactup state and shell setup"

[ "$fail" -eq 0 ] && echo "0 failure(s)" || echo "failures"
exit "$fail"
