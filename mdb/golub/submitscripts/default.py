# golub submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/golub.sub (PBS directives consumed by sbatch's
# compatibility mode — see meta.toml [scheduler].submit).
#
# .py variant (design §6.1): the old script used THREE expression templates —
#   #PBS -q @(ifthen(":" in "@QUEUE@", <before-colon>, "@QUEUE@"))@
#   #SBATCH @(ifthen(":" in "@QUEUE@", "--constraint="+<after-colon>, ""))@
#   #PBS @('@CHAINED_JOB_ID@' != '' ? '-W depend=afterany:…' : '')@
# — cactup's @NAME@ engine is literal-only (D7), so the queue:feature split
# and the chaining conditional move into Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)

# Queue convention (see meta.toml): "name:feature" selects a node feature via
# an sbatch constraint.
if ":" in QUEUE:
    queue_name = QUEUE[:QUEUE.find(":")]
    constraint = QUEUE[QUEUE.find(":") + 1:]
else:
    queue_name = QUEUE
    constraint = ""

lines = ["#! /bin/bash"]
lines.append("#PBS -q {0}".format(queue_name))
lines.append("#PBS -r n")
lines.append("#PBS -l walltime={0}".format(WALLTIME))
lines.append("#PBS -A {0}".format(ALLOCATION))
lines.append("#PBS -l nodes={0}:ppn={1}".format(NODES, MAX_TASKS_PER_NODE))
if constraint:
    lines.append("#SBATCH --constraint={0}".format(constraint))
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
    " --restart-id={5} {6}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID, FROM_RESTART_COMMAND,
    )
)

print("\n".join(lines))
