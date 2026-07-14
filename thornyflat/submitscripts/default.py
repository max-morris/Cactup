# thornyflat submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/thornyflat.sub (PBS/Torque).
#
# .py variant (design §6.1): the old script used the chained-job ternary
# @('@CHAINED_JOB_ID@' != '' ? '-hold_jid @CHAINED_JOB_ID@' : '')@ — note the
# SGE-style -hold_jid dependency type, kept — cactup's @NAME@ engine is
# literal-only (D7), so the conditional moves into Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# The old script passed no -A allocation directive; kept that way.

lines = ["#! /bin/bash"]
lines.append("#PBS -r n")
lines.append("#PBS -l walltime={0}".format(WALLTIME))
lines.append("#PBS -q {0}".format(QUEUE))
lines.append("#PBS -l nodes={0}:ppn={1}".format(NODES, MAX_TASKS_PER_NODE))
if CHAINED_JOB_ID:
    lines.append("#PBS -hold_jid {0}".format(CHAINED_JOB_ID))
lines.append("#PBS -N {0}".format(SHORT_SIMULATION_NAME))
if EMAIL:
    lines.append("#PBS -M {0}".format(EMAIL))
    lines.append("#PBS -m abe")
lines.append("#PBS -o {0}".format(STDOUT_FILE))
lines.append("#PBS -e {0}".format(STDERR_FILE))

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
