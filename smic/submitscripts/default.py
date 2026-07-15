# smic submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/smic.sub (PBS/Torque).
#
# .py variant (design §6.1): the old script used the chained-job ternary
# @('@CHAINED_JOB_ID@' != '' ? '-W depend=afterany:@CHAINED_JOB_ID@' : '')@ —
# cactup's @NAME@ engine is literal-only (D7), so the conditional moves into
# Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# The old `#PBS -m abe` had no -M address line; kept verbatim (PBS mails the
# submitting user).

lines = ["#! /bin/bash"]
lines.append("#PBS -A {0}".format(ALLOCATION))
lines.append("#PBS -q {0}".format(QUEUE))
lines.append("#PBS -r n")
lines.append("#PBS -l walltime={0}".format(WALLTIME))
lines.append("#PBS -l nodes={0}:ppn={1}".format(NODES, MAX_CPUS_PER_NODE))
if CHAINED_JOB_ID:
    lines.append("#PBS -W depend=afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#PBS -V")
lines.append("#PBS -N {0}".format(SHORT_SIMULATION_NAME))
lines.append("#PBS -m abe")
lines.append("#PBS -o {0}".format(STDOUT_FILE))
lines.append("#PBS -e {0}".format(STDERR_FILE))

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
