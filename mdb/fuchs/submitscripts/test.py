# fuchs TEST submitscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Same #SBATCH header as submitscripts/default.py but WITHOUT
# restart chaining (a testsuite is one-shot — no recovery, no chain, design
# §11.6); the payload re-invokes `cactup test run` on the compute node with
# the §11.6 locator flags (--test-dir + --results-id) instead of `sim run`.
#
# .py rather than .sh so the #SBATCH header stays above ENV_SETUP (cactup
# auto-prepends env-setup to .sh submitscripts — design §6.1).

lines = ["#!/bin/bash"]
lines.append("#SBATCH --partition=parallel")
lines.append("#SBATCH --constraint=dual")
lines.append("#SBATCH --time={0}".format(WALLTIME))
lines.append("#SBATCH --ntasks={0}".format(TASKS))
lines.append("#SBATCH --cpus-per-task={0}".format(CPUS_PER_TASK))
lines.append("#SBATCH --job-name={0}".format(TEST_NAME))
lines.append("#SBATCH --mem-per-cpu=2600")
lines.append("#SBATCH --mail-type=ALL")
lines.append("#SBATCH --output={0}".format(STDOUT_FILE))
lines.append("#SBATCH --error={0}".format(STDERR_FILE))

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
