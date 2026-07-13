# Sciama TEST submitscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Same #SBATCH header as submitscripts/default.py; the payload
# re-invokes `cactup test run` on the compute node with the §11.6 locator
# flags (--test-dir + --results-id) instead of `sim run`.
#
# .py rather than .sh so the #SBATCH header stays above ENV_SETUP (cactup
# auto-prepends env-setup to .sh submitscripts — design §6.1).

lines = ["#! /bin/bash"]
lines.append("#SBATCH --partition {0}".format(QUEUE))
lines.append("#SBATCH --time={0}".format(WALLTIME))
lines.append("#SBATCH --nodes={0}".format(NODES))
lines.append("#SBATCH --tasks-per-node={0}".format(MAX_TASKS_PER_NODE))
lines.append("#SBATCH --export=ALL")
lines.append("#SBATCH --jobname={0}".format(TEST_NAME))
lines.append("#SBATCH --out {0}".format(STDOUT_FILE))
lines.append("#SBATCH --error {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py variants (design §6.1).
lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} test run {1} --installation={2} --test-dir={3} --machine={4}"
    " --results-id={5}".format(
        CACTUP, TEST_NAME, ALIAS, TEST_DIR, MACHINE, RESULTS_ID,
    )
)

print("\n".join(lines))
