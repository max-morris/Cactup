#! /bin/bash
# athena runscript (variant "cpu"): CPU-only runs on the `cpu` queue
# (`-q cpu`). Written 2026-09-29, mirroring mdb/qbd/runscripts/cpu.sh with its
# srun launch replaced by mpirun. Measured facts it depends on: 2 x AMD EPYC
# 7662, 128 cores, SMT off; HPC-X 2.24 Open MPI (env-setup in meta.toml,
# prepended by cactup).
#
# Binding: `--map-by slot:PE=@CPUS_PER_TASK@ --bind-to core` gives each rank
# its own block of @CPUS_PER_TASK@ consecutive cores (the mpirun counterpart
# of qbd's `srun --cpus-per-task --cpu-bind=cores`), and OMP_PLACES=cores
# with OMP_PROC_BIND=close keeps each rank's threads inside its block.
# Checked on athena 2026-09-29 with 2 ranks x 4 threads (--report-bindings:
# rank 0 on cores 0-3, rank 1 on cores 4-7, threads placed close). Unlike
# the GPU runscript, there is no single-task shortcut: the singleton hang
# noted there applies here too, so this always goes through mpirun.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -x
set -e

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date

if [ @NODES@ -gt 1 ]; then
    echo "athena runs on one node only (@NODES@ requested); see mdb/athena/meta.toml" >&2
    exit 1
fi

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

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

time mpirun -np @TASKS@ \
    --map-by slot:PE=@CPUS_PER_TASK@ --bind-to core \
    @EXECUTABLE@ -L 3 -r -b line @PARFILE@

echo "Stopping:"
date
echo "Done."
