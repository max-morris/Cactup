#!/bin/bash
# omnia runscript (variant "default"), ported from simfactory2
# mdb/runscripts/omnia.run.
#
# Variable renames vs simfactory (design §6.3): NUM_PROCS→@TASKS@,
# NUM_THREADS→@CPUS_PER_TASK@; metadata dir SIMFACTORY/→.cactup/ (§9.3).
#
# The original sourced /storage/users/sbrandt/omnia/Cactus/env-rocm.sh here,
# because simfactory's envsetup wrapping only covered commands simfactory ran
# itself and not this script when the submitscript exec'd it standalone. cactup
# applies env-setup to the run phase too and auto-prepends it right after the
# shebang (design §4.2/§6.1), so the ROCm/HIP + Intel MPI environment is
# already in place by the time this body runs.
#
# env-rocm.sh (machine-local, not in version control) was compared against the
# ini's envsetup: the same block, plus two exports the heredoc lacked
# (CACTUS_EXTERNALS, AMREX_INSTALL_DIR). Those now live in
# [environment].env-setup, so this script still gets everything it used to.

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

if [ ${CACTUS_NUM_PROCS} = 1 ]; then
    if [ @RUNDEBUG@ -eq 0 ]; then
        @EXECUTABLE@ -L 3 @PARFILE@
    else
        gdb --args @EXECUTABLE@ -L 3 @PARFILE@
    fi
else
    mpirun -np @TASKS@ @EXECUTABLE@ -L 3 @PARFILE@
fi

echo "Stopping:"
date
echo "Done."
