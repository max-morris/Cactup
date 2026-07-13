# LEONARDO DCGP submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/leonardo-DCGP.sub.
#
# .py variant (design §6.1): the old script used expression templates —
#   --ntasks-per-node @(@NUM_PROCS@//@NODES@)@   (== TASKS_PER_NODE)
#   @("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@
# — cactup's @NAME@ engine is literal-only (D7); the first equals the
# canonical TASKS_PER_NODE, the conditional moves into Python. cactup would
# also auto-prepend env-setup above the #SBATCH header of a .sh variant.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_THREADS@ -> CPUS_PER_TASK
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   @SIMFACTORY@ run -> @CACTUP@ sim run --installation/--sim-dir/--machine
#     (the compute-node re-invocation locator)
# Kept verbatim: the hardcoded -p dcgp_usr_prod... actually emitted from
# @QUEUE@ (the machine's sole queue IS dcgp_usr_prod, so -q keeps working),
# --mem 494000MB, and the `sleep 10` before the payload.

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH --mem 494000MB")
lines.append("#SBATCH --nodes {0}".format(NODES))
lines.append("#SBATCH --ntasks-per-node {0}".format(TASKS_PER_NODE))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH -J {0}".format(SHORT_SIMULATION_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py variants (design §6.1); place it
# after the directive header.
lines.append(ENV_SETUP)

lines.append("sleep 10")
lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} sim run {1} --installation={2} --sim-dir={3} --machine={4}"
    " --restart-id={5} {6}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID, FROM_RESTART_COMMAND,
    )
)

print("\n".join(lines))
