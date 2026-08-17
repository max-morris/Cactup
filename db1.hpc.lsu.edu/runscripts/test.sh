#! /bin/bash
# db1.hpc.lsu.edu TEST runscript (variant "test") — marked test = true in
# meta.toml (design §11.2). Instead of launching a parfile like
# runscripts/default.sh, it drives `make <config>-testsuite` and lets the
# Cactus flesh harness launch each test (design §11.6). The launcher mirrors
# default.sh: srun --overlap with a GPU per task.
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (where the
# harness output must land, under test-home) and TESTSUITE_SELECT (which tests
# to run; empty = all). env-setup is auto-prepended by cactup for .sh variants
# (design §6.1).

echo "Preparing testsuite:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @SOURCEDIR@

module purge
module load gcc/9.3.0
module load cuda/12.4.0 mpich/3.3.2/intel-19.1.3
module list

echo "Checking:"
pwd
hostname
date

export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_PLACES=cores

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands. The launcher mirrors runscripts/default.sh
# (its GRES branch depends on $QUEUE, which is unset here, so it is omitted).
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='srun --overlap -n $nprocs --cpus-per-task=@CPUS_PER_TASK@ --gpus-per-task=@GPUS_PER_TASK@ $exe $parfile'

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
