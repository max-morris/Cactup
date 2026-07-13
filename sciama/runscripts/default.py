# Sciama runscript (variant "default"), ported from simfactory2
# mdb/runscripts/sciama.run.
#
# This is a .py variant (design §6.1): the old script used computed float
# templates that the literal-only @NAME@ engine (D7) cannot express —
#   @(1.0*@NUM_PROCS@/@NODES@)@                       (MPI processes per node)
#   @(1.0*(@NUM_PROCS@*@NUM_THREADS@)/(@NODES@*@MAX_TASKS_PER_NODE@))@ (OpenMP threads per core)
# Those are computed here in Python from the typed variables. Other rewrites
# (design §6.3): NUM_PROCS -> TASKS, NUM_THREADS -> CPUS_PER_TASK,
# (PPN_USED/NUM_THREADS) -> TASKS_PER_NODE (equal by definition),
# @ENV(MPIROOT)@ -> the shell's ${MPIROOT} (runtime environment read),
# metadata dir SIMFACTORY/ -> .cactup/ (§9.3; .cactup/NODES is also read by
# the machine's exec-host command).
#
# env-setup is NOT auto-prepended for .py variants (design §6.1); it is
# emitted right after the shebang, where cactup would have placed it for a
# .sh runscript.

mpi_procs_per_node = 1.0 * typed["TASKS"] / typed["NODES"]
threads_per_core = (1.0 * (typed["TASKS"] * typed["CPUS_PER_TASK"])
                    / (typed["NODES"] * typed["MAX_TASKS_PER_NODE"]))
ppn_used = typed["TASKS_PER_NODE"] * typed["CPUS_PER_TASK"]

script = """#! /bin/bash
{env_setup}
echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd {rundir}-active

echo "Checking:"
pwd
hostname
date
cat ${{PBS_NODEFILE}} > .cactup/NODES

echo "Environment:"
module load intel_comp/2019.2
module load openmpi/4.0.1
echo "Environment:"
export CACTUS_NUM_PROCS={tasks}
export CACTUS_NUM_THREADS={cpus_per_task}
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS={cpus_per_task}
export MPIDIR=${{MPIROOT}}
export MPD_NODEFILE=mpd_nodefile
export MPI_NODEFILE=mpi_nodefile

echo "Starting:"
uniq ${{PBS_NODEFILE}} > ${{MPD_NODEFILE}}
for node in $(cat ${{MPD_NODEFILE}}); do
    for ((proc=0; $proc<{tasks_per_node}; proc=$proc+1)); do
        echo ${{node}}
    done
done > ${{MPI_NODEFILE}}
export CACTUS_STARTTIME=$(date +%s)

env | sort > .cactup/ENVIRONMENT

echo "Job setup:"
echo "   Allocated:"
echo "      Nodes:                      {nodes}"
echo "      Cores per node:             {ppn}"
echo "   Running:"
echo "      MPI processes:              {tasks}"
echo "      OpenMP threads per process: {cpus_per_task}"
echo "      MPI processes per node:     {mpi_procs_per_node}"
echo "      OpenMP threads per core:    {threads_per_core}"
echo "      OpenMP threads per node:    {ppn_used}"

if [ {rundebug} -eq 0 ]
then
	time ${{MPIDIR}}/bin/mpirun -v --mca btl openib,self --mca mpi_leave_pinned 0 -np {tasks} -npernode {tasks_per_node} {executable} -L 3 {parfile}
fi

echo "Stopping:"
date

echo "Done."
""".format(
    env_setup=ENV_SETUP,
    rundir=RUNDIR,
    tasks=TASKS,
    cpus_per_task=CPUS_PER_TASK,
    tasks_per_node=TASKS_PER_NODE,
    nodes=NODES,
    ppn=MAX_TASKS_PER_NODE,
    mpi_procs_per_node=mpi_procs_per_node,
    threads_per_core=threads_per_core,
    ppn_used=ppn_used,
    rundebug=RUNDEBUG,
    executable=EXECUTABLE,
    parfile=PARFILE,
)

print(script)
