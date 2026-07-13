#! /bin/bash
# thornyflat runscript (variant "default"), ported from simfactory2
# mdb/runscripts/thornyflat.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
#   (PPN_USED / NUM_THREADS) -> @TASKS_PER_NODE@ (they are equal by definition)
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date
cat ${PBS_NODEFILE} > .cactup/NODES

echo "Environment:"
export GMON_OUT_PREFIX=gmon.out
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export OMP_NUM_THREADS=@CPUS_PER_TASK@
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)
time mpiexec -n @TASKS@ -npernode @TASKS_PER_NODE@ @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date

echo "Done."
