#! /bin/bash
# cosma8 runscript (variant "default"), ported from simfactory2
# mdb/runscripts/cosma8.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).
# (The original exported MPI_PROCESS_PER_NODE=NUM_PROCS — kept verbatim,
# including that oddity.)

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active


echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@

export I_MPI_DEBUG=5

env > .cactup/ENVIRONMENT

export MPI_PROCESS_PER_NODE=@TASKS@
export MPI_PROCESS=@TASKS@

export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@

echo "Starting:"
mpirun -n @TASKS@ @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date

echo "Done."
