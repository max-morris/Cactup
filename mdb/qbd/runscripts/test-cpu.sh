#! /bin/bash
# qbd TEST runscript for the CPU partitions (script "test-cpu") — marked
# test = true in meta.toml (design §11.2). runscripts/test.sh with the CPU
# launcher of runscripts/cpu.sh: srun with cores bound and no GPU flags
# (mpirun does not work for multi-task jobs on QB4). --overlap as in test.sh:
# a `test run` inside an interactive allocation already holds its resources
# in the `srun --pty` step, so each test's own srun must share them. It drives
# `make <config>-testsuite` and lets the Cactus flesh harness launch each
# test (design §11.6).
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (where the
# harness output must land, under test-home) and TESTSUITE_SELECT (which tests
# to run; empty = all). env-setup is auto-prepended by cactup for .sh scripts
# (design §6.1).

echo "Preparing testsuite:"
set -x
set -e

cd @SOURCEDIR@

module list
echo "Checking:"
pwd
hostname
date

export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_PLACES=cores
export OMP_PROC_BIND=close
export OMP_STACKSIZE=8192
export OMPI_MCA_pml=ucx
export OMPI_MCA_btl=^openib

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='srun --overlap -n $nprocs --cpus-per-task=@CPUS_PER_TASK@ --cpu-bind=cores $exe $parfile'

# Redirect testsuite output into test-home (design §11.6).
mkdir -p @TESTSUITE_RESULTS_DIR@
export TESTS_DIR=@TESTSUITE_RESULTS_DIR@

echo "Running testsuite (selection: '@TESTSUITE_SELECT@', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

export CCTK_TESTSUITE_RUN_TESTS="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
