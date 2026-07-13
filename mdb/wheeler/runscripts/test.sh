#! /bin/bash
# wheeler TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Instead of launching a parfile like runscripts/default.sh,
# it drives `make <config>-testsuite` and lets the Cactus flesh harness launch
# each test (design §11.6). The launcher mirrors default.sh's Intel MPI mpirun
# without the precomputed hostfile and -ppn (those depend on the per-run
# topology; each test's $nprocs varies).
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
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_STACKSIZE=8192
export OMP_MAX_TASKS=500
export CXX_MAX_TASKS=500
export PTHREAD_MAX_TASKS=500
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export CACTUS_SET_THREAD_BINDINGS=1

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands (the -env value expansions happen when the
# harness runs the command — the variables are exported above). The launcher
# mirrors runscripts/default.sh.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='mpirun -np $nprocs -print-rank-map -env OMP_NUM_THREADS "$OMP_NUM_THREADS" -env OMP_STACKSIZE "$OMP_STACKSIZE" -env OMP_MAX_TASKS "$OMP_MAX_TASKS" -env CXX_MAX_TASKS "$CXX_MAX_TASKS" -env PTHREAD_MAX_TASKS "$PTHREAD_MAX_TASKS" -env CACTUS_NUM_PROCS "$CACTUS_NUM_PROCS" -env CACTUS_NUM_THREADS "$CACTUS_NUM_THREADS" -env CACTUS_SET_THREAD_BINDINGS "$CACTUS_SET_THREAD_BINDINGS" $exe $parfile'

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
