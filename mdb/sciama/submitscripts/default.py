# Sciama submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/sciama.sub.
#
# .py rather than .sh so the #SBATCH header stays above ENV_SETUP (cactup
# auto-prepends env-setup to .sh submitscripts — design §6.1). The old script
# had NO restart-chaining directive; kept that way.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   --out/--error use STDOUT_FILE/STDERR_FILE — the old script wrote plain
#     @SIMULATION_NAME@.{out,err} (submission-cwd-relative); the canonical
#     @RUNDIR@-based defaults are where the machine's stdout/stderr commands
#     look for them.
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# Kept verbatim: --tasks-per-node=@MAX_TASKS_PER_NODE@ and the (sic) --jobname spelling.

lines = ["#! /bin/bash"]
lines.append("#SBATCH --partition {0}".format(QUEUE))
lines.append("#SBATCH --time={0}".format(WALLTIME))
lines.append("#SBATCH --nodes={0}".format(NODES))
lines.append("#SBATCH --tasks-per-node={0}".format(MAX_TASKS_PER_NODE))
lines.append("#SBATCH --export=ALL")
lines.append("#SBATCH --jobname={0}".format(SHORT_SIMULATION_NAME))
lines.append("#SBATCH --out {0}".format(STDOUT_FILE))
lines.append("#SBATCH --error {0}".format(STDERR_FILE))

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
