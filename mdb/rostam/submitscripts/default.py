# rostam submitscript for every partition.
# --gres is per node (TASKS_PER_NODE × GPUS_PER_TASK). --mpi=pmi2 is an
# srun option (this cluster's sbatch does not accept it) and lives on
# the runscript launch line.
#
# .py variant (design §6.1): chaining and the mail / allocation directives
# are conditionals (cactup's @NAME@ engine is literal-only, D7).

lines = ["#! /bin/bash"]
if ALLOCATION:
    lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
lines.append("#SBATCH --gpus-per-task {0}".format(GPUS_PER_TASK))
lines.append("#SBATCH --gres=gpu:{0}".format(
    typed["TASKS_PER_NODE"] * typed["GPUS_PER_TASK"]))
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH -J {0}".format(SHORT_SIMULATION_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} sim run {1} --installation={2} --sim-dir={3} --machine={4}"
    " --restart-id={5}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID,
    )
)

print("\n".join(lines))
