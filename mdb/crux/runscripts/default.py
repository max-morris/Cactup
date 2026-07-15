# Crux runscript (variant "default"), ported from simfactory2
# mdb/runscripts/crux.run.
#
# This is a .py variant (design §6.1): the old script used computed float
# templates that the literal-only @NAME@ engine (D7) cannot express —
#   @(1.0*(@NODE_PROCS@*@NUM_THREADS@)/@MAX_CPUS_PER_NODE@)@   (OpenMP threads per core)
#   @(@NODE_PROCS@*@NUM_THREADS@)@               (OpenMP threads per node)
# Both are computed here in Python from the typed variables and interpolated
# into the emitted bash script. Variable renames vs. simfactory (design §6.3):
# NUM_PROCS -> TASKS, NODE_PROCS -> TASKS_PER_NODE, NUM_THREADS ->
# CPUS_PER_TASK. (The original wrote `env | sort > ENVIRONMENT` — no
# SIMFACTORY/ dir — kept verbatim.)
#
# env-setup is NOT auto-prepended for .py variants (design §6.1); it is
# emitted right after the preamble, where cactup would have placed it for a
# .sh runscript.

threads_per_core = (1.0 * typed["TASKS_PER_NODE"] * typed["CPUS_PER_TASK"]
                    / typed["MAX_CPUS_PER_NODE"])
threads_per_node = typed["TASKS_PER_NODE"] * typed["CPUS_PER_TASK"]

script = """#!/bin/bash
{env_setup}
cd {rundir}-active

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS={tasks}
export CACTUS_NUM_THREADS={cpus_per_task}
export OMP_NUM_THREADS={cpus_per_task}
env | sort > ENVIRONMENT

export OMP_PROC_BIND=true
export OMP_PLACES=cores

echo "Job setup:"
echo "   Allocated:"
echo "      Nodes:                      {nodes}"
echo "      Cores per node:             {ppn}"
echo "   Running:"
echo "      MPI processes:              {tasks}"
echo "      OpenMP threads per process: {cpus_per_task}"
echo "      MPI processes per node:     {tasks_per_node}"
echo "      OpenMP threads per core:    {threads_per_core}"
echo "      OpenMP threads per node:    {threads_per_node}"

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

if [ {rundebug} -eq 0 ]; then
    mpiexec -n {tasks} --ppn {tasks_per_node} --depth {cpus_per_task} --cpu-bind depth --envlist OMP_NUM_THREADS,CACTUS_NUM_THREADS,CACTUS_NUM_PROCS,OMP_PROC_BIND,OMP_PLACES,LD_LIBRARY_PATH {executable} -L 3 {parfile}
else
    gdb --args {executable} -L 3 {parfile}
fi

echo "Stopping:"
date
""".format(
    env_setup=ENV_SETUP,
    rundir=RUNDIR,
    tasks=TASKS,
    cpus_per_task=CPUS_PER_TASK,
    tasks_per_node=TASKS_PER_NODE,
    nodes=NODES,
    ppn=MAX_CPUS_PER_NODE,
    threads_per_core=threads_per_core,
    threads_per_node=threads_per_node,
    rundebug=RUNDEBUG,
    executable=EXECUTABLE,
    parfile=PARFILE,
)

print(script)
