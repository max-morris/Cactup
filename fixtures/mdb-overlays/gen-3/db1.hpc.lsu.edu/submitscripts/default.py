# db1.hpc.lsu.edu submitscript (script "default"), ported from simfactory2
# mdb/submitscripts/db-new.sub (upstream's native rewrite).
#
# .py script (design §6.1): the old script used simfactory's ternary
# expression @("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@
# — cactup's @NAME@ engine is literal-only (D7), so the conditional moves into
# Python, alongside the computed --gres directive and the `if EMAIL:` mail
# guard below.
#
# Calling convention: cactup binds every §6.3 variable as a module global and
# this script prints the final submit script to stdout.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@/@NUM_THREADS@ -> TASKS/CPUS_PER_TASK
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
#   --gpus-per-task takes the §8.5 GPUS_PER_TASK topology variable instead of a
#     hardcoded 1, so `--gpus-per-task N` works.
#   --gres is per node: tasks per node × GPUs per task. The old script asked
#     for NODES × TASKS, which is the whole job's GPU count on every node (a
#     2-node, 4-task job asked each node for 8 of its 2 GPUs).

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
lines.append("#SBATCH --gpus-per-task {0}".format(GPUS_PER_TASK))
lines.append("#SBATCH --gres=gpu:{0}".format(typed["TASKS_PER_NODE"] * typed["GPUS_PER_TASK"]))
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH -J {0}".format(SHORT_SIMULATION_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py scripts (design §6.1); place it
# after the directive header.
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
