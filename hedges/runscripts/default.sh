#!/bin/bash
# hedges runscript (variant "default"), ported from simfactory2
# mdb/runscripts/generic-mpi.run (always launches through mpirun, unlike
# generic.run's single-process special case).
# Variable renames vs simfactory (design §6.3): NUM_PROCS→@TASKS@,
# NUM_THREADS→@CPUS_PER_TASK@.

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
# (kept from the original, which had this commented out too; the metadata dir
# is .cactup/ now — design §9.3)
#env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

if [ @RUNDEBUG@ -eq 0 ]; then
    mpirun -np @TASKS@ @EXECUTABLE@ -L 3 @PARFILE@
else
    gdb --args @EXECUTABLE@ -L 3 @PARFILE@
fi

echo "Stopping:"
date
