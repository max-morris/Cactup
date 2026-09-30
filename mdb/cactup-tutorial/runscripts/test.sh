#!/bin/bash
# cactup-tutorial TEST runscript (variant "test"): drives `make
# <config>-testsuite` and lets the Cactus flesh harness launch each test. It
# runs inside the allocation `cactup test submit` requested; a foreground
# `cactup test run` outside one is refused (the machine sets allocation-env).
# The launcher is runscripts/default.sh's, flag for flag (see there for why).
#
# Testsuite-only variables: TESTSUITE_RESULTS_DIR (where the harness output
# must land, under test-home) and TESTSUITE_SELECT (which tests to run;
# empty = all).

echo "Preparing testsuite:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @SOURCEDIR@

echo "Checking:"
pwd
hostname
date

export OMP_NUM_THREADS=@CPUS_PER_TASK@

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command; single quotes keep them out of the
# shell's hands.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='mpirun -np $nprocs --bind-to none --mca pml ob1 --mca btl self,sm $exe $parfile'

# Redirect testsuite output into test-home: the flesh harness honors
# TESTS_DIR and writes each test's run dirs plus summary.log under
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
