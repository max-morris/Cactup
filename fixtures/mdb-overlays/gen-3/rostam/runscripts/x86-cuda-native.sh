#!/bin/bash
# rostam runscript for flavor x86-cuda-native (universe x86-modules). Launches
# the executable directly with srun; OpenMPI 5.0.7 is built against PMIx 5,
# which Slurm's pmix plugin (pmix_v5) serves.

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
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

time srun -u --mpi=pmix \
    -N @NODES@ -n @TASKS@ \
    --cpus-per-task @CPUS_PER_TASK@ \
    --gpus-per-task @GPUS_PER_TASK@ \
    @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date
echo "Done."
