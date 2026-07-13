# thornyflat TEST submitscript (variant "test") — marked test = true in
# meta.toml (design §11.2). Same #PBS header as submitscripts/default.py but
# WITHOUT restart chaining (a testsuite is one-shot — no recovery, no chain,
# design §11.6); the payload re-invokes `cactup test run` on the compute node
# with the §11.6 locator flags (--test-dir + --results-id) instead of
# `sim run`.
#
# .py rather than .sh so the #PBS header stays above ENV_SETUP (cactup
# auto-prepends env-setup to .sh submitscripts — design §6.1).

lines = ["#! /bin/bash"]
lines.append("#PBS -r n")
lines.append("#PBS -l walltime={0}".format(WALLTIME))
lines.append("#PBS -q {0}".format(QUEUE))
lines.append("#PBS -l nodes={0}:ppn={1}".format(NODES, MAX_TASKS_PER_NODE))
lines.append("#PBS -N {0}".format(TEST_NAME))
if EMAIL:
    lines.append("#PBS -M {0}".format(EMAIL))
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
