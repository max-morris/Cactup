#!/bin/bash
# smic TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Instead of launching a parfile like runscripts/default.sh,
# it drives `make <config>-testsuite` and lets the Cactus flesh harness launch
# each test (design §11.6). The launcher mirrors default.sh's openmpi mpirun
# without the precomputed hostfile (that depends on the per-run topology; each
# test's $nprocs varies).
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
export MPICHDIR=/usr/local/packages/openmpi/3.1.5/tr7ckfes
export MV2_SRQ_SIZE=4000        # ???
export LD_LIBRARY_PATH=/usr/local/compilers/Intel/parallel_studio_xe_2019.5/compilers_and_libraries_2019.5.281/linux/compiler/lib/intel64_lin:/usr/local/compilers/Intel/parallel_studio_xe_2019.5/compilers_and_libraries/linux/mkl/lib/intel64:/usr/local/packages/gcc/9.3.0/5jmpgadg/lib64:${LD_LIBRARY_PATH}

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands. The launcher mirrors runscripts/default.sh.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='${MPICHDIR}/bin/mpirun -n $nprocs -mca I_MPI_OFA_USE_XRC 1 -mca coll_fca_enable 0 /bin/env MV2_ENABLE_AFFINITY=0 OMP_NUM_THREADS=@CPUS_PER_TASK@ $exe $parfile'

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
