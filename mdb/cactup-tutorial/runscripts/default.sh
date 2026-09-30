#!/bin/bash
# cactup-tutorial runscript (variant "default"). It runs inside the job the
# submitscript requested (or directly, for `cactup sim run`), on the one node
# there is, and launches Cactus with OpenMPI 5's mpirun:
#   --bind-to none   OpenMPI binds each rank to a core by default. Here
#                    several jobs share the node, and SLURM (task/none, no
#                    cgroups) does not give each job its own cores, so every
#                    job would bind its ranks to the same first cores. Unbound,
#                    the kernel spreads them over the container's CPUs. The
#                    threads are left unbound too (no OMP_PLACES or
#                    OMP_PROC_BIND).
#   --mca pml ob1 --mca btl self,sm
#                    messages go through shared memory only: the node is the
#                    whole machine, so there is no network to probe for. The
#                    shared-memory transport is named `sm` in OpenMPI 5 (it
#                    was `vader` before).
#
# `cactup sim run --debug` sets @RUNDEBUG@ to 1, and every rank then runs
# under @DEBUGGER@ (gdb) in batch mode: the run proceeds as usual, and a rank
# that crashes prints its backtrace. -return-child-result keeps the program's
# own exit status, so a clean run still counts as a success.

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date
# More than 0 when SLURM requeued the job and this is a rerun of the same
# restart (the output above, if any, is the attempt it interrupted).
echo "Times SLURM requeued this job: ${SLURM_RESTART_COUNT:-0}"

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

if [ @RUNDEBUG@ -eq 0 ]; then
    time mpirun -np @TASKS@ --bind-to none --mca pml ob1 --mca btl self,sm \
        @EXECUTABLE@ -L 3 @PARFILE@
else
    time mpirun -np @TASKS@ --bind-to none --mca pml ob1 --mca btl self,sm \
        @DEBUGGER@ -q -batch -return-child-result -ex run -ex bt -ex quit \
        --args @EXECUTABLE@ -L 3 @PARFILE@
fi

echo "Stopping:"
date
echo "Done."
