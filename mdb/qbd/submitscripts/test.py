# qbd TEST submitscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Same #PBS header as submitscripts/default.py but WITHOUT
# restart chaining (a testsuite is one-shot — no recovery, no chain, design
# §11.6); the payload re-invokes `cactup test run` on the compute node with
# the §11.6 locator flags (--test-dir + --results-id) instead of `sim run`.
#
# .py variant (design §6.1): the `-q @QUEUE@` and mail directives are guarded
# by `if QUEUE:` and `if EMAIL:` — cactup's @NAME@ engine is literal-only
# (D7), so both conditionals move into Python.

lines = ["#! /bin/bash"]
lines.append("#PBS -l walltime={0}".format(WALLTIME))
lines.append("#PBS -l nodes={0}:ppn={1}".format(NODES, TASKS_PER_NODE))
if QUEUE:
    lines.append("#PBS -q {0}".format(QUEUE))
lines.append("#PBS -A {0}".format(ALLOCATION))
lines.append("#PBS -N {0}".format(TEST_NAME))
lines.append("#PBS -j oe")
if EMAIL:
    lines.append("#PBS -m abe")
    lines.append("#PBS -M {0}".format(EMAIL))
lines.append("#PBS -o {0}".format(STDOUT_FILE))

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
