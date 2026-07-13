#! /bin/bash
# expanse runscript (variant "default"), ported from simfactory2
# mdb/runscripts/expanse.run.
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
# Use infiniband
export OMPI_MCA_btl_openib_if_include="mlx5_2:1"

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

time srun --mpi=pmi2 -n @TASKS@ @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date

echo "Done."
