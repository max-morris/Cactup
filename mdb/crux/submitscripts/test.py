# Crux TEST submitscript (script "test") — marked test = true in meta.toml
# (design §11.2). Same #PBS header as submitscripts/default.py but WITHOUT
# restart chaining (a testsuite is one-shot — no recovery, no chain, design
# §11.6); the payload re-invokes `cactup test run` on the compute node with
# the §11.6 locator flags (--test-dir + --results-id) instead of `sim run`.
#
# .py script (design §6.1): the account directive depends on whether an
# allocation is set, and cactup's @NAME@ engine is literal-only (D7).

lines = ["#! /bin/bash"]
lines.append("#PBS -q \"{0}\"".format(QUEUE))
lines.append("#PBS -r n")
lines.append("#PBS -l walltime={0}".format(WALLTIME))
# Account required: ALCF rejects a job without a project. The -a flag, else
# the allocation knob; with neither, cactup refuses before submitting, naming
# both.
allocation = ALLOCATION or knob("allocation", "")
if not allocation:
    raise CactupError(
        "{0} needs an allocation: pass -a ACCOUNT, or set it once with "
        "`cactup knob allocation ACCOUNT`".format(MACHINE)
    )
lines.append("#PBS -A {0}".format(allocation))
lines.append("#PBS -l select={0}:system=crux".format(NODES))
lines.append("#PBS -l place=scatter")
lines.append("#PBS -V")
lines.append("#PBS -l filesystems=home:eagle:grand")
lines.append("#PBS -N {0}".format(TEST_NAME))
lines.append("#PBS -m abe")
lines.append("#PBS -o {0}".format(STDOUT_FILE))
lines.append("#PBS -e {0}".format(STDERR_FILE))

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
