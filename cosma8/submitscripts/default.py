# cosma8 submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/cosma8.sub.
#
# .py variant (design §6.1): the old script used the chained-job ternary
# @("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@
# (literal-only @NAME@ engine, D7), and cactup would auto-prepend env-setup
# above the #SBATCH header of a .sh variant.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@/@NODE_PROCS@ -> TASKS/TASKS_PER_NODE
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# The old payload re-loaded three modules after cd @SOURCEDIR@; kept verbatim.

lines = ["#! /bin/bash"]
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --ntasks-per-node={0}".format(TASKS_PER_NODE))
lines.append("#SBATCH -J {0}".format(SHORT_SIMULATION_NAME))
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterany:{0}".format(CHAINED_JOB_ID))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))
lines.append("#SBATCH -A {0}".format(ALLOCATION))

# env-setup is NOT auto-prepended for .py variants (design §6.1); place it
# after the directive header.
lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append("module load openmpi/5.0.3")
lines.append("module load fftw/3.3.10")
lines.append("module load gnu_comp/14.1.0")
lines.append(
    "exec {0} sim run {1} --installation={2} --sim-dir={3} --machine={4}"
    " --restart-id={5} {6}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID, FROM_RESTART_COMMAND,
    )
)

print("\n".join(lines))
