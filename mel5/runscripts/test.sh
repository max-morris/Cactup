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
#   TESTSUITE_SELECT       which tests to run ("all", or a selector list).
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

# The flesh testsuite harness launches each test with this command (it appends
# the executable and parfile for each test itself) and this processor count
# (design §11.6; simfactory-docs.txt §6.4). A plain mpirun suffices on mel5.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND="mpirun -np @TASKS@"

# Redirect testsuite output into test-home (design §11.6, step 6). cactup keeps
# the source tree clean by pointing the harness's results dir at the active
# results-NNNN under test-home. When the flesh testsuite target exposes no
# output-directory option, the portable fallback is to symlink the in-tree
# results dir at TESTSUITE_RESULTS_DIR so the harness writes straight into
# test-home and only a symlink is left behind in configs/<config>/.
# (The exact hook is flesh-version-dependent — see the §11.6 ASSUMPTION.)
mkdir -p @TESTSUITE_RESULTS_DIR@
rm -rf configs/@CONFIGURATION@/TEST
ln -s @TESTSUITE_RESULTS_DIR@ configs/@CONFIGURATION@/TEST

echo "Running testsuite (selection: @TESTSUITE_SELECT@):"
export CACTUS_STARTTIME=$(date +%s)

# PROMPT=no runs non-interactively. With the default selection ("all") this runs
# every thorn's tests; a narrower selection is passed through for the flesh
# harness to honor (selection hook is flesh-version-dependent — design §11.6).
export CCTK_TESTSUITE_SELECTION="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
