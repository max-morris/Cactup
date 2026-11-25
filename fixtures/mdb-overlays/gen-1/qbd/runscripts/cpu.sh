#! /bin/bash
# qbd runscript (variant "cpu"): CPU partitions single/workq/checkpt/bigmem.
# Mirrors runscripts/default.sh minus the GPU binding flags. srun, not mpirun:
# mpirun does not work for multi-task jobs on QB4. Runs inside the allocation
# the submitscript requested; passes none of -A/-p/-N itself.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -x
set -e

cd @RUNDIR@-active

module list
echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_PLACES=cores
export OMP_PROC_BIND=close
export OMP_STACKSIZE=8192
env | sort > .cactup/ENVIRONMENT

# -r: keep stdout of MPI ranks > 0 as CCTK_Proc<n>.out next to the parfile
#     (the flesh otherwise sends it to /dev/null, which hides errors that
#     libraries such as Kadath print to stdout before abort()).
# -b line: line-buffer stdout so those files are complete after a crash.

export OMPI_MCA_pml=ucx
export OMPI_MCA_btl=^openib

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

time srun -u -n @TASKS@ \
    --cpus-per-task=@CPUS_PER_TASK@ \
    --cpu-bind=cores \
    @EXECUTABLE@ -L 3 -r -b line @PARFILE@

echo "Stopping:"
date
echo "Done."
