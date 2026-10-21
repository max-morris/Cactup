#!/bin/bash
# rostam runscript for flavor x86-cpu-image (universe cactus-cpu; medusa and
# buran). Launches with SLURM PMI *outside* the container:
# `srun --mpi=pmi2 … apptainer exec … cactus`, like x86-cuda-image.sh but
# with the CPU image and no GPUs. The x86-cpu-image optionlist sets
# coerce-run-universe = false so this script is not itself wrapped in the
# universe (design §4.8 MPI boundary).

echo "Preparing:"
set -x
set -e

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
# MPICH's UCX netmod uses /proc/<peer-pid>/fd and SysV shm; Apptainer's
# PID namespace makes that fail (Permission denied / shmat Invalid argument).
export UCX_POSIX_USE_PROC_LINK=n
export UCX_MEMTYPE_CACHE=n
# CMA uses process_vm_readv, which Apptainer rejects (Operation not permitted).
export UCX_TLS=^cma,^knem,^xpmem
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

time srun -u --mpi=pmi2 \
    -N @NODES@ -n @TASKS@ \
    --cpus-per-task @CPUS_PER_TASK@ \
    apptainer exec --no-pid --bind /work --bind /dev/shm \
    docker://stevenrbrandt/cactus \
    @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date
echo "Done."
