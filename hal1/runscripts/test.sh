#!/bin/bash
# hal1 TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Drives `make <config>-testsuite` and lets the Cactus flesh
# harness launch each test (design §11.6); see mdb/mel5/runscripts/test.sh for
# the annotated reference version.
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (active
# results-NNNN under test-home), TESTSUITE_SELECT (flesh format: empty = all,
# else space-separated `Thorn` / `Thorn/testname` entries).

echo "Preparing testsuite:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @SOURCEDIR@

echo "Checking:"
pwd
hostname
date

# The flesh testsuite harness (lib/sbin/RunTestUtils.pl) substitutes the
# literal placeholders $nprocs/$exe/$parfile into CCTK_TESTSUITE_RUN_COMMAND.
# On a generic workstation we deliberately do NOT set it: the flesh picks its
# own default ('mpirun -np $nprocs $exe $parfile' when MPI is built, plain
# '$exe $parfile' otherwise), which is exactly the right machine-agnostic
# behavior. Only the processor count is pinned to the requested topology.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@

# Redirect testsuite output into test-home (design §11.6, step 6): the flesh
# honors TESTS_DIR (default $CCTK_HOME/TEST) and writes each test's run dirs
# plus summary.log under $TESTS_DIR/@CONFIGURATION@/.
mkdir -p @TESTSUITE_RESULTS_DIR@
export TESTS_DIR=@TESTSUITE_RESULTS_DIR@

echo "Running testsuite (selection: '@TESTSUITE_SELECT@', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

export CCTK_TESTSUITE_RUN_TESTS="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
