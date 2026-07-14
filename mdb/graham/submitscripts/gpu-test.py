# Graham TEST submitscript (variant "gpu-test") — marked test = true in meta.toml
# (design §11.2). Same #SBATCH header as submitscripts/gpu.py but WITHOUT
# restart chaining (a testsuite is one-shot — no recovery, no chain, design
# §11.6); the payload re-invokes `cactup test run` on the compute node with
# the §11.6 locator flags (--test-dir + --results-id) instead of `sim run`.
#
# .py variant (design §6.1): the mail directives are guarded by `if EMAIL:`
# (cactup's @NAME@ engine is literal-only, D7, so the conditional moves into
# Python).

lines = ["#! /bin/bash"]
lines.append("#SBATCH --account={0}".format(ALLOCATION))
lines.append("#SBATCH --time={0}".format(WALLTIME))
lines.append("#SBATCH --nodes={0} --ntasks={1} --cpus-per-task={2}".format(
    NODES, TASKS, CPUS_PER_TASK))
lines.append("#SBATCH --mem {0}MB".format(MEMORY))
lines.append("#SBATCH --gres=gpu:p100:{0}".format(TASKS_PER_NODE))
lines.append("#SBATCH --export=ALL")
lines.append("#SBATCH --job-name={0}".format(TEST_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
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
