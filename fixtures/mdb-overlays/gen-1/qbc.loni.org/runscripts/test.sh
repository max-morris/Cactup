#! /bin/bash
# qbc.loni.org TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Instead of launching a parfile like runscripts/default.sh,
# it drives `make <config>-testsuite` and lets the Cactus flesh harness launch
# each test (design §11.6). The launcher matches default.sh: srun into the
# Singularity image on the compute nodes.
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (where the
# harness output must land, under test-home) and TESTSUITE_SELECT (which tests
# to run; empty = all). env-setup is auto-prepended by cactup for .sh variants
# (design §6.1).

echo "Preparing testsuite:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @SOURCEDIR@

# Same environment block as runscripts/default.sh.
if which srun 2>/dev/null
then module purge
fi

export SPACK_ROOT=/usr/cactus/spack-root
export SPACK_SYSTEM_CONFIG_PATH=/usr/cactus/.spack
export SPACK_SKIP_MODULES=1
export SPACK_ENV=$SPACK_ROOT/var/spack/environments/cpu
export PATH=$SPACK_ENV/.spack-env/view/bin:$PATH
export LD_LIBRARY_PATH=$SPACK_ENV/.spack-env/view/libn:$LD_LIBRARY_PATH

echo "Checking:"
pwd
hostname
date

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands. The launcher mirrors runscripts/default.sh.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='srun -u -A @ALLOCATION@ -p checkpt -n $nprocs singularity exec --bind /var/spool --bind /project --bind /ddnA/project --bind /etc/ssh/ssh_known_hosts --bind /ddnA/work --bind /work --bind /scratch /work/sbrandt/images/etworkshop-cpu.simg $exe $parfile'

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
