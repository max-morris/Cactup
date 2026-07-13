# golub TEST submitscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Same directive header as submitscripts/default.py (including
# the queue:feature split) but WITHOUT restart chaining (a testsuite is
# one-shot — no recovery, no chain, design §11.6); the payload re-invokes
# `cactup test run` on the compute node with the §11.6 locator flags
# (--test-dir + --results-id) instead of `sim run`.
#
# .py rather than .sh so the directive header stays above ENV_SETUP (cactup
# auto-prepends env-setup to .sh submitscripts — design §6.1).

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
lines.append("#PBS -V")
lines.append("#PBS -N {0}".format(TEST_NAME))
lines.append("#PBS -m abe")
lines.append("#PBS -o {0}".format(STDOUT_FILE))
lines.append("#PBS -e {0}".format(STDERR_FILE))

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
