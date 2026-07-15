# wheeler submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/wheeler.sub.
#
# .py variant (design §6.1): the old script used the chained-job ternary
# @("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@ AND the
# arithmetic expression -n @(@NUM_PROCS@*@NUM_THREADS@)@ (Wheeler allocates
# one SLURM task per *core*, not per MPI rank) — cactup's @NAME@ engine is
# literal-only (D7), so both move into Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@*@NUM_THREADS@ -> typed["TASKS"] * typed["CPUS_PER_TASK"]
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   -A guarded by `if ALLOCATION:` (the old ini declared no allocation)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# No -p directive, as upstream (the old `queue = NOQUEUE` was a placeholder;
# SLURM uses the cluster's default partition).

alloc_tasks = typed["TASKS"] * typed["CPUS_PER_TASK"]

lines = ["#! /bin/bash"]
if ALLOCATION:
    lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, alloc_tasks))
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
    " --restart-id={5}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID,
    )
)

print("\n".join(lines))
