#!/bin/bash
# rostam runscript for flavor arm-cuda-native (universe arm-conda; the spark
# partition): a native aarch64 build, launched directly with srun. The
# flavor's MPICH (conda-forge) cannot use PMIx, which is this Slurm's default,
# so the launch asks for PMI-2.

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

time srun -u --mpi=pmi2 \
    -N @NODES@ -n @TASKS@ \
    --cpus-per-task @CPUS_PER_TASK@ \
    --gpus-per-task @GPUS_PER_TASK@ \
    @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date
echo "Done."
