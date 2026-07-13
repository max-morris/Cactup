#!/bin/bash
# holodeck TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Instead of launching a parfile like runscripts/default.sh,
# it drives `make <config>-testsuite` and lets the Cactus flesh harness launch
# each test (design §11.6). The launcher mirrors default.sh's Intel mpirun
# (per-test task count via $nprocs).
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

# Same environment block as runscripts/default.sh.
export OMP_NUM_THREADS=@CPUS_PER_TASK@
set +e
. /opt/intel/2017b/intel.sh
. /opt/intel/2017b/impi/2017.2.174/bin64/mpivars.sh
set -e
export I_MPI_PIN_DOMAIN=omp

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands. The launcher mirrors runscripts/default.sh.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='mpirun -n $nprocs -genvlist OMP_NUM_THREADS,CACTUS_NUM_THREADS,CACTUS_NUM_PROCS,LD_LIBRARY_PATH $exe $parfile'

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
