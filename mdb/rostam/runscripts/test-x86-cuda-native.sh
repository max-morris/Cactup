#!/bin/bash
# rostam TEST runscript for flavor x86-cuda-native (universe x86-modules) —
# marked test = true in meta.toml. Drives `make <config>-testsuite`; the flesh
# harness substitutes $nprocs/$exe/$parfile into CCTK_TESTSUITE_RUN_COMMAND.
# Launcher matches runscripts/x86-cuda-native.sh: srun --mpi=pmix, no
# container.

echo "Preparing testsuite:"
set -x
set -e

cd @SOURCEDIR@

echo "Checking:"
pwd
hostname
date

export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
# Single quotes keep $nprocs/$exe/$parfile for the flesh; @GPUS_PER_TASK@ is
# substituted by cactup before this script runs.
export CCTK_TESTSUITE_RUN_COMMAND='srun -u --mpi=pmix -n $nprocs --gpus-per-task @GPUS_PER_TASK@ $exe $parfile'

mkdir -p @TESTSUITE_RESULTS_DIR@
export TESTS_DIR=@TESTSUITE_RESULTS_DIR@

echo "Running testsuite (selection: '@TESTSUITE_SELECT@', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

export CCTK_TESTSUITE_RUN_TESTS="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
