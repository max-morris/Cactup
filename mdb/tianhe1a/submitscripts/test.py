# tianhe1a TEST submitscript (script "test") — marked test = true in meta.toml
# (design §11.2). Same #SBATCH header as submitscripts/default.py but WITHOUT
# restart chaining (a testsuite is one-shot — no recovery, no chain, design
# §11.6); the payload re-invokes `cactup test run` on the compute node with
# the §11.6 locator flags (--test-dir + --results-id) instead of `sim run`.
#
# .py script (design §6.1): the account directive depends on whether an
# allocation is set, and cactup's @NAME@ engine is literal-only (D7).

lines = ["#! /bin/bash"]
# Account optional: not known to be required here. The -a flag, else the
# allocation knob; with neither, the line is left out (an empty one makes the
# scheduler read the next directive as the account).
allocation = ALLOCATION or knob("allocation", "")
if allocation:
    lines.append("#SBATCH -A {0}".format(allocation))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH -J {0}".format(TEST_NAME))
lines.append("#SBATCH --mail-type=ALL")
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py scripts (design §6.1); place it
# after the directive header.
lines.append(ENV_SETUP)

lines.append("cd {0} || exit 1".format(SOURCEDIR))
lines.append(
    "exec {0} test run {1} --installation={2} --test-dir={3} --machine={4}"
    " --results-id={5}".format(
        CACTUP, TEST_NAME, ALIAS, TEST_DIR, MACHINE, RESULTS_ID,
    )
)

print("\n".join(lines))
