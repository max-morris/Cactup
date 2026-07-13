#! /bin/bash
# delta runscript (variant "default"), ported from simfactory2
# mdb/runscripts/delta.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

module list

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
# export CACTUS_SET_THREAD_BINDINGS=1
export CXX_MAX_TASKS=500
export GMON_OUT_PREFIX=gmon.out
export OMP_MAX_TASKS=500
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_STACKSIZE=8192       # kByte
export PTHREAD_MAX_TASKS=500
env | sort > .cactup/ENVIRONMENT
echo ${SLURM_NODELIST} > NODES

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

ulimit -c unlimited
time srun -n @TASKS@ @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date

echo "Done."
