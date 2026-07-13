#! /bin/bash
# graham TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Instead of launching a parfile like runscripts/default.sh,
# it drives `make <config>-testsuite` and lets the Cactus flesh harness launch
# each test (design §11.6). The launcher is default.sh's mpiexec without the
# precomputed hostfile/binding setup (those depend on the per-run topology;
# each test's $nprocs varies).
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (where the
# harness output must land, under test-home) and TESTSUITE_SELECT (which tests
# to run; empty = all). env-setup is auto-prepended by cactup for .sh variants
# (design §6.1).

echo "Preparing testsuite:"
set -euxo pipefail

cd @SOURCEDIR@

echo "Checking:"
date
hostname
pwd

module list

# Same environment block as runscripts/default.sh.
export "CACTUS_MAX_MEMORY=$(( @MEMORY@ * 1024 ))" # Byte
export "CACTUS_SET_THREAD_BINDINGS=1"
export "GLIBCXX_FORCE_NEW=1"
export "GMON_OUT_PREFIX=gmon.out"
export "OMP_DISPLAY_ENV=TRUE"
export "OMP_NUM_THREADS=@CPUS_PER_TASK@"
export "OMP_PLACES=cores"       # threads, cores, sockets
export "OMP_PROC_BIND=FALSE"    # false, true, master, close, spread
export "OMP_STACKSIZE=8192"     # kByte

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='mpiexec --n $nprocs $exe $parfile'

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
