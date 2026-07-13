#!/bin/bash
# et-cuda runscript (variant "default"), ported from simfactory2 generic.run.
# Variable renames vs simfactory (design §6.3): NUM_PROCS→@TASKS@,
# NUM_THREADS→@CPUS_PER_TASK@; metadata dir SIMFACTORY/→.cactup/ (§9.3).

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
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

if [ ${CACTUS_NUM_PROCS} = 1 ]; then
    if [ @RUNDEBUG@ -eq 0 ]; then
        @EXECUTABLE@ -L 3 @PARFILE@
    else
        gdb --args @EXECUTABLE@ -L 3 @PARFILE@
    fi
else
    mpirun -np @TASKS@ @EXECUTABLE@ -L 3 @PARFILE@
fi

echo "Stopping:"
date
echo "Done."
