# perlmutter-p1 TEST submitscript (variant "test") — marked test = true in
# meta.toml (design §11.2). Same #SBATCH header as submitscripts/default.py
# but WITHOUT restart chaining (a testsuite is one-shot — no recovery, no
# chain, design §11.6); the payload re-invokes `cactup test run` on the
# compute node with the §11.6 locator flags (--test-dir + --results-id)
# instead of `sim run`.
#
# .py variant (design §6.1): the mail directives are guarded by `if EMAIL:`
# (cactup's @NAME@ engine is literal-only, D7, so the conditional moves into
# Python).

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}_g".format(ALLOCATION))
lines.append("#SBATCH -C gpu")
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1} -c {2}".format(NODES, TASKS, CPUS_PER_TASK))
lines.append("#SBATCH --ntasks-per-node {0}".format(TASKS_PER_NODE))
lines.append("#SBATCH --gpus-per-task {0}".format(GPUS_PER_TASK))
lines.append("#SBATCH --gpu-bind=map_gpu:0,1,2,3")
lines.append("#SBATCH -J {0}".format(TEST_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

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
