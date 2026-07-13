#! /bin/bash
# db1.hpc.lsu.edu runscript (variant "default"), ported from simfactory2
# mdb/runscripts/db-new.run (upstream's native rewrite — no more Singularity
# image; module-loads gcc/cuda/mpich and srun-launches the binary directly).
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).
#
# $QUEUE is a shell variable the GRES branch reads; upstream never exports it
# (the submitscript sets @QUEUE@ only in the #SBATCH -p directive), so it is
# unset at runtime and GRES stays empty for the "gpu" queue — the srun below
# still requests a GPU via --gpus-per-task=1. The gpu2/gpu4 branches are kept
# verbatim for the day a user runs on those partitions.

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

module purge
module load gcc/9.3.0
module load cuda/12.4.0 mpich/3.3.2/intel-19.1.3
module list

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_PLACES=cores
env | sort > .cactup/ENVIRONMENT

#if [ $(((@TASKS@*@CPUS_PER_TASK@)%48)) != 0 -a $(((@TASKS@*@CPUS_PER_TASK@))) != 24 ]
#then
#    echo "Deep Bayou requires you either use half a node (24 cores),"
#    echo "or multiple whole nodes (multiples of 48 cores)."
#    echo "Please adjust your call to simfactory accordingly."
#    exit 2
#fi

GRES=""
if [ "$QUEUE" = gpu4 ]
then
    GRES="--gres=gpu:4"
fi
if [ "$QUEUE" = gpu2 ]
then
    if [ @TASKS@ == 1 ]
    then
        GRES="--gres=gpu:1"
    else
        GRES="--gres=gpu:2"
    fi
fi

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

time srun --overlap -n @TASKS@ \
    --cpus-per-task=@CPUS_PER_TASK@ \
    --gpus-per-task=1 \
    $GRES \
    @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date

echo "Done."
