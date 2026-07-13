#!/bin/bash
# Crux TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Instead of launching a parfile like runscripts/default.py,
# it drives `make <config>-testsuite` and lets the Cactus flesh harness launch
# each test (design §11.6). The launcher mirrors default.py's mpiexec (per-test
# task count via $nprocs). A plain .sh suffices here — none of default.py's
# computed float output is needed.
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (where the
# harness output must land, under test-home) and TESTSUITE_SELECT (which tests
# to run; empty = all). env-setup is auto-prepended by cactup for .sh variants
# (design §6.1).

echo "Preparing testsuite:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @SOURCEDIR@

echo "Checking:"
pwd
hostname
date

# Same environment block as runscripts/default.py.
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_PROC_BIND=true
export OMP_PLACES=cores

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands. The launcher mirrors runscripts/default.py.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='mpiexec -n $nprocs --ppn @TASKS_PER_NODE@ --depth @CPUS_PER_TASK@ --cpu-bind depth --envlist OMP_NUM_THREADS,CACTUS_NUM_THREADS,CACTUS_NUM_PROCS,OMP_PROC_BIND,OMP_PLACES,LD_LIBRARY_PATH $exe $parfile'

# Redirect testsuite output into test-home (design §11.6): the flesh harness
# honors TESTS_DIR and writes each test's run dirs plus summary.log under
# $TESTS_DIR/@CONFIGURATION@/, keeping the source tree clean.
mkdir -p @TESTSUITE_RESULTS_DIR@
export TESTS_DIR=@TESTSUITE_RESULTS_DIR@

echo "Running testsuite (selection: '@TESTSUITE_SELECT@', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

export CCTK_TESTSUITE_RUN_TESTS="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
