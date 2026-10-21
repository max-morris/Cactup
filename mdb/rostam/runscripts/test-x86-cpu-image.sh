#!/bin/bash
# rostam TEST runscript for flavor x86-cpu-image — marked test = true in
# meta.toml (design §11.2). Drives `make <config>-testsuite`; the flesh
# harness substitutes $nprocs/$exe/$parfile into CCTK_TESTSUITE_RUN_COMMAND.
# Launcher matches runscripts/x86-cpu-image.sh: srun --mpi=pmi2 outside
# Apptainer.

echo "Preparing testsuite:"
set -x
set -e

cd @SOURCEDIR@

echo "Checking:"
pwd
hostname
date

export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
# MPICH's UCX netmod uses /proc/<peer-pid>/fd and SysV shm; Apptainer's
# PID namespace makes that fail (Permission denied / shmat Invalid argument).
export UCX_POSIX_USE_PROC_LINK=n
export UCX_MEMTYPE_CACHE=n
# CMA uses process_vm_readv, which Apptainer rejects (Operation not permitted).
export UCX_TLS=^cma,^knem,^xpmem
# Single quotes keep $nprocs/$exe/$parfile for the flesh.
export CCTK_TESTSUITE_RUN_COMMAND='srun -u --mpi=pmi2 -n $nprocs apptainer exec --no-pid --bind /work --bind /dev/shm docker://stevenrbrandt/cactus $exe $parfile'

mkdir -p @TESTSUITE_RESULTS_DIR@
export TESTS_DIR=@TESTSUITE_RESULTS_DIR@

echo "Running testsuite (selection: '@TESTSUITE_SELECT@', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

export CCTK_TESTSUITE_RUN_TESTS="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
