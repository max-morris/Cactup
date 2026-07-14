# qbc.loni.org submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/mike.sub.
#
# .py variant (design §6.1): the old script used simfactory's ternary
# expression @("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@
# — cactup's @NAME@ engine is literal-only (D7), so the conditional moves into
# Python, alongside the `if EMAIL:` mail guard below.
#
# Calling convention: cactup binds every §6.3 variable as a module global and
# this script prints the final submit script to stdout.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@/@NUM_THREADS@ -> TASKS/CPUS_PER_TASK
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
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

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} sim run {1} --installation={2} --sim-dir={3} --machine={4}"
    " --restart-id={5} {6}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID, FROM_RESTART_COMMAND,
    )
)

print("\n".join(lines))
