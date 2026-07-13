# fuchs submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/fuchs.sub.
#
# .py variant (design §6.1): the old script used the chained-job ternary
# @("@CHAINED_JOB_ID@" != "" ? "-d afterok:@CHAINED_JOB_ID@" : "")@ — note
# afterok, not afterany, kept — (literal-only @NAME@ engine, D7), and cactup
# would auto-prepend env-setup above the #SBATCH header of a .sh variant.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@/@NUM_THREADS@ -> TASKS/CPUS_PER_TASK
#   --output/--error use STDOUT_FILE/STDERR_FILE (same
#     @RUNDIR@/@SIMULATION_NAME@.{out,err} defaults, but they honor -o/-e)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# Kept verbatim: the hardcoded --partition=parallel (the old script did not
# use @QUEUE@), --constraint=dual, --mem-per-cpu=2600, and the bare
# --mail-type=ALL with no --mail-user (SLURM mails the submitting user).

lines = ["#!/bin/bash"]
lines.append("#SBATCH --partition=parallel")
lines.append("#SBATCH --constraint=dual")
lines.append("#SBATCH --time={0}".format(WALLTIME))
lines.append("#SBATCH --ntasks={0}".format(TASKS))
lines.append("#SBATCH --cpus-per-task={0}".format(CPUS_PER_TASK))
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterok:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH --job-name={0}".format(SHORT_SIMULATION_NAME))
lines.append("#SBATCH --mem-per-cpu=2600")
lines.append("#SBATCH --mail-type=ALL")
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
