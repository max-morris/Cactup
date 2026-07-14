# Graham submitscript (variant "gpu" = the CUDA flavor), ported from
# simfactory2 mdb/submitscripts/graham-gpu.sub. Same as the "default" (CPU)
# submitscript plus the `--gres=gpu:p100` directive; serves the gpu queue.
#
# .py variant (design §6.1): the old script used expression templates —
#   --cpus-per-task=@(int(32/@NODE_PROCS@))@   (== CPUS_PER_TASK)
#   @("@CHAINED_JOB_ID@" != "" ? "--dependency=afterany:…" : "")@
# — cactup's @NAME@ engine is literal-only (D7); the first equals the
# canonical CPUS_PER_TASK, the conditional moves into Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@ -> TASKS
#   --output/--error use STDOUT_FILE/STDERR_FILE (same
#     @RUNDIR@/@SIMULATION_NAME@.{out,err} defaults, but they honor -o/-e)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# No partition directive: Compute Canada schedules by account (NO_QUEUE).

lines = ["#! /bin/bash"]
lines.append("#SBATCH --account={0}".format(ALLOCATION))
lines.append("#SBATCH --time={0}".format(WALLTIME))
lines.append("#SBATCH --nodes={0} --ntasks={1} --cpus-per-task={2}".format(
    NODES, TASKS, CPUS_PER_TASK))
lines.append("#SBATCH --mem {0}MB".format(MEMORY))
lines.append("#SBATCH --gres=gpu:p100:{0}".format(TASKS_PER_NODE))
if CHAINED_JOB_ID:
    lines.append("#SBATCH --dependency=afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH --export=ALL")
lines.append("#SBATCH --job-name={0}".format(SHORT_SIMULATION_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH --output={0}".format(STDOUT_FILE))
lines.append("#SBATCH --error={0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py variants (design §6.1); place it
# after the directive header.
lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} sim run {1} --installation={2} --sim-dir={3} --machine={4}"
    " --restart-id={5} {6}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID, FROM_RESTART_COMMAND,
    )
)

print("\n".join(lines))
