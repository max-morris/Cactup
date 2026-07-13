#!/bin/bash
# holodeck runscript (variant "default"), ported from simfactory2
# mdb/runscripts/holodeck.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).
# (The original had no `cd` into the run directory; kept as-is — the -L 1
# log level and the Intel environment sourcing are also verbatim.)

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
export OMP_NUM_THREADS=@CPUS_PER_TASK@

set +e
. /opt/intel/2017b/intel.sh
. /opt/intel/2017b/impi/2017.2.174/bin64/mpivars.sh
set -e
export I_MPI_PIN_DOMAIN=omp
env | sort > .cactup/ENVIRONMENT
echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)
which mpirun
if [ @RUNDEBUG@ -eq 0 ]; then
    time mpirun -n @TASKS@ -genvlist OMP_NUM_THREADS,CACTUS_NUM_THREADS,CACTUS_NUM_PROCS,LD_LIBRARY_PATH @EXECUTABLE@ -L 1 @PARFILE@
else
    gdb --args @EXECUTABLE@ -L 1 @PARFILE@
fi

echo "Stopping:"
date
