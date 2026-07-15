# tianhe1a submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/tianhe1a.sub (SLURM-style directives).
#
# NOTE: the old ini's submit/getstatus/stop commands were EMPTY, so this
# script was never actually submitted by simfactory; ported for reference
# (see the header comment in meta.toml).
#
# .py variant (design §6.1): the old script used the chained-job ternary
# @("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@ — cactup's
# @NAME@ engine is literal-only (D7), so the conditional moves into Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@ -> TASKS
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# Kept verbatim: --mail-type=ALL with no --mail-user (the old script mailed
# the submitting user unconditionally).

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH -J {0}".format(SHORT_SIMULATION_NAME))
lines.append("#SBATCH --mail-type=ALL")
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py variants (design §6.1); place it
# after the directive header (empty for this machine — no [environment]).
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
