#!/bin/bash
# mel5 TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). This is why a testsuite needs its OWN runscript: instead of
# mpirun-ing a parfile like runscripts/default.sh, it drives
# `make <config>-testsuite` and lets the Cactus flesh harness launch each test
# (design §11.6).
#
# Testsuite-only variables it consumes (design §11.9):
#   TESTSUITE_RESULTS_DIR  absolute path to the active results-NNNN under
#                          test-home; the harness output must land here so the
#                          Cactus source tree stays clean (design §11.5/§11.6).
#   TESTSUITE_SELECT       which tests to run, in the flesh's own format:
#                          empty = all, else space-separated `Thorn` or
#                          `Thorn/testname` entries.
# Plus the usual CONFIGURATION / TASKS / MACHINE / SOURCEDIR (design §6.3).
#
# env-setup is auto-prepended by cactup for .sh variants (design §6.1), so the
# same module/MPI environment the config was built with is already in scope.

echo "Preparing testsuite:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @SOURCEDIR@

echo "Checking:"
pwd
hostname
date

# The flesh testsuite harness (lib/sbin/RunTestUtils.pl) launches each test by
# substituting the literal placeholders $nprocs/$exe/$parfile into this command
# — it does NOT append them (design §11.6; simfactory-docs.txt §6.4). Single
# quotes keep the placeholders out of the shell's hands; RUN_PROCESSORS is the
# $nprocs default, overridable per test by its test.ccl.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='mpirun -np $nprocs $exe $parfile'

# Redirect testsuite output into test-home (design §11.6, step 6). The flesh
# harness honors the TESTS_DIR env var directly (RunTestUtils.pl; default is
# $CCTK_HOME/TEST) and writes each test's run dirs plus the final summary.log
# under $TESTS_DIR/@CONFIGURATION@/, keeping the source tree clean.
mkdir -p @TESTSUITE_RESULTS_DIR@
export TESTS_DIR=@TESTSUITE_RESULTS_DIR@

echo "Running testsuite (selection: '@TESTSUITE_SELECT@', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

# PROMPT=no runs non-interactively. The flesh's selection hook is the
# CCTK_TESTSUITE_RUN_TESTS env var: empty/unset runs every thorn's tests, else
# a space-separated list of `Thorn` / `Thorn/testname` entries.
export CCTK_TESTSUITE_RUN_TESTS="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
