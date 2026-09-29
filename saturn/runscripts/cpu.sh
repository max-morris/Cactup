#! /bin/bash
# saturn runscript (variant "cpu"): CPU-only runs on Frank's saturn node
# through the `cpu` queue (`cactup sim submit ... -q cpu`). Written
# 2026-09-29. Mirrors mdb/qbd/runscripts/cpu.sh (OMP_PLACES=cores,
# OMP_PROC_BIND=close, -r -b line), launched with a local mpirun instead of
# srun because Frank has no scheduler; a single task is a direct exec, as in
# mdb/et-juphub.
#
# Measured facts this depends on (2026-09-29): 4 x Xeon Platinum 8367HC,
# 104 cores, SMT 2, 4 NUMA nodes; OpenMPI 4.1.8 (gcc 13.2.0).
# `mpirun --map-by slot:PE=8 --bind-to core` was checked there: each rank got
# 8 whole cores (both hardware threads of each), ranks on consecutive cores.

echo "Preparing:"
set -x
set -e

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
export OMP_PLACES=cores
export OMP_PROC_BIND=close
export OMP_STACKSIZE=8192
export OMPI_MCA_btl=^openib
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

# -r: keep stdout of MPI ranks > 0 as CCTK_Proc<n>.out next to the parfile
#     (the flesh otherwise sends it to /dev/null, which hides errors that
#     libraries such as Kadath print to stdout before abort()).
# -b line: line-buffer stdout so those files are complete after a crash.
if [ @TASKS@ = 1 ]; then
    if [ @RUNDEBUG@ -eq 0 ]; then
        time @EXECUTABLE@ -L 3 -r -b line @PARFILE@
    else
        gdb --args @EXECUTABLE@ -L 3 -r -b line @PARFILE@
    fi
else
    time mpirun -np @TASKS@ --map-by slot:PE=@CPUS_PER_TASK@ --bind-to core \
        @EXECUTABLE@ -L 3 -r -b line @PARFILE@
fi

echo "Stopping:"
date
echo "Done."
