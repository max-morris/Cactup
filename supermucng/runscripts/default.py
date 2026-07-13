# supermucng runscript (variant "default"), ported from simfactory2
# mdb/runscripts/supermucng.run; .py form of the earlier default.sh.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> TASKS
#   NUM_THREADS -> CPUS_PER_TASK
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).
#
# env-setup is NOT auto-prepended for .py variants (design §6.1); it is
# emitted right after the shebang, where cactup would have placed it for a
# .sh runscript.
#
# This .py is evaluated at run time on the compute node (design §6.1/§6.2),
# so job-environment values are read here with os.environ rather than left
# as shell expansions in the emitted script.

import os

nodelist = os.environ.get("SLURM_NODELIST", "")

script = """#! /bin/bash
{env_setup}

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd {rundir}-active

set +x -v # -x is too verbose
module load slurm_setup
set -x +v

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS={tasks}
export CACTUS_NUM_THREADS={cpus_per_task}
export CACTUS_SET_THREAD_BINDINGS=1
export CXX_MAX_TASKS=500
export GMON_OUT_PREFIX=gmon.out
export OMP_MAX_TASKS=500
export OMP_NUM_THREADS={cpus_per_task}
export OMP_STACKSIZE=8192       # kByte
export PTHREAD_MAX_TASKS=500
export I_MPI_PIN_CELL=core
export I_MPI_PIN_DOMAIN=omp:compact
env | sort > .cactup/ENVIRONMENT
echo '{nodelist}' > NODES

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)
mpiexec -n {tasks} {executable} -L 3 {parfile}

echo "Stopping:"
date

echo "Done."
""".format(
    env_setup=ENV_SETUP,
    rundir=RUNDIR,
    tasks=TASKS,
    cpus_per_task=CPUS_PER_TASK,
    nodelist=nodelist,
    executable=EXECUTABLE,
    parfile=PARFILE,
)

print(script)
