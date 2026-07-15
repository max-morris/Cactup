# Frontier submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/frontier.sub.
#
# .py variant (design §6.1): the old script used the chained-job ternary
# @("@CHAINED_JOB_ID@" != "" ? "--dependency=afterany:@CHAINED_JOB_ID@" : "")@
# — cactup's @NAME@ engine is literal-only (D7), so the conditional moves
# into Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NODE_PROCS@ -> TASKS_PER_NODE; @PPN_USED@ -> TASKS_PER_NODE*CPUS_PER_TASK
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)

lines = ["#! /bin/bash"]
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))
lines.append("#SBATCH --account={0}".format(ALLOCATION))
lines.append("#SBATCH --job-name={0}".format(SHORT_SIMULATION_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH --time={0}".format(WALLTIME))
lines.append("#SBATCH --partition={0}".format(QUEUE))
lines.append("#SBATCH --nodes={0}".format(NODES))
lines.append("#SBATCH --gpus-per-node={0}".format(TASKS_PER_NODE))
lines.append("#SBATCH --gpu-bind=closest")
# Kept verbatim from the old script (a deliberately disabled directive):
# "Jobs with this option will not start"
lines.append("##SBATCH --tasks-per-node={0}".format(
    typed["TASKS_PER_NODE"] * typed["CPUS_PER_TASK"]))
lines.append("#SBATCH --cpus-per-task=1")
if CHAINED_JOB_ID:
    lines.append("#SBATCH --dependency=afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH --export=ALL")

# env-setup is NOT auto-prepended for .py variants (design §6.1); place it
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
