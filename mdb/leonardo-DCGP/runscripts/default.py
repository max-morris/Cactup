# LEONARDO DCGP runscript (variant "default"), ported from simfactory2
# mdb/runscripts/leonardo-DCGP.run.
#
# This is a .py variant (design §6.1): the old script used computed float
# templates that the literal-only @NAME@ engine (D7) cannot express —
#   @(1.0*@NUM_PROCS@/@NODES@)@                       (MPI processes per node)
#   @(1.0*(@NUM_PROCS@*@NUM_THREADS@)/(@NODES@*@MAX_CPUS_PER_NODE@))@ (OpenMP threads per core)
#   @PPN_USED@                                        (OpenMP threads per node)
# All are computed here in Python from the typed variables and interpolated
# into the emitted bash script. Variable renames vs. simfactory (design §6.3):
# NUM_PROCS -> TASKS, NUM_THREADS -> CPUS_PER_TASK. Metadata dir SIMFACTORY/
# -> .cactup/ (design §9.3).
#
# env-setup is NOT auto-prepended for .py variants (design §6.1); it is
# emitted right after the shebang, where cactup would have placed it for a
# .sh runscript.

mpi_procs_per_node = 1.0 * typed["TASKS"] / typed["NODES"]
threads_per_core = (1.0 * (typed["TASKS"] * typed["CPUS_PER_TASK"])
                    / (typed["NODES"] * typed["MAX_CPUS_PER_NODE"]))
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

echo "Environment:"
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS={cpus_per_task}
export CACTUS_NUM_PROCS={tasks}
export CACTUS_NUM_THREADS={cpus_per_task}
env > .cactup/ENVIRONMENT

export MPI_PROCESS_PER_NODE={mpi_procs_per_node}
export MPI_PROCESS={tasks}

echo "Job setup:"
echo "   Allocated:"
echo "      Nodes:                      {nodes}"
echo "      Cores per node:             {ppn}"
echo "   SLURM setting"
echo "      SLURM_NNODES :  ${{SLURM_NNODES}}"
echo "      SLURM_NPROCS :  ${{SLURM_NPROCS}}"
echo "      SLURM_NTASKS :  ${{SLURM_NTASKS}}"
echo "      SLURM_CPUS_ON_NODE  :  ${{SLURM_CPUS_ON_NODE}}"
echo "      SLURM_CPUS_PER_TASK :  ${{SLURM_CPUS_PER_TASK}}"
echo "      SLURM_TASKS_PER_NODE:  ${{SLURM_TASKS_PER_NODE}}"
echo "   Running:"
echo "      MPI processes:              {tasks}"
echo "      OpenMP threads per process: {cpus_per_task}"
echo "      MPI processes per node:     {mpi_procs_per_node}"
echo "      OpenMP threads per core:    {threads_per_core}"
echo "      OpenMP threads per node:    {ppn_used}"

export I_MPI_DEBUG=5

ulimit -c unlimited
export decfort_dump_flag=TRUE

echo "Starting:"

srun {executable} -L 1 {parfile}

echo "Stopping:"
date

echo "Done."
""".format(
    env_setup=ENV_SETUP,
    rundir=RUNDIR,
    tasks=TASKS,
    cpus_per_task=CPUS_PER_TASK,
    nodes=NODES,
    ppn=MAX_CPUS_PER_NODE,
    mpi_procs_per_node=mpi_procs_per_node,
    threads_per_core=threads_per_core,
    ppn_used=ppn_used,
    executable=EXECUTABLE,
    parfile=PARFILE,
)

print(script)
