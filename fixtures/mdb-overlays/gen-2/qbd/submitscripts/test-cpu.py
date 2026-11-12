# qbd TEST submitscript for the CPU partitions (script "test-cpu") — marked
# test = true in meta.toml (design §11.2). submitscripts/test.py minus the GPU
# reservation: none of QB4's --gres rules apply on single/workq/checkpt/bigmem
# (see submitscripts/cpu.py). One-shot like every testsuite job — no restart
# chaining (design §11.6); the payload re-invokes `cactup test run` on the
# compute node with the §11.6 locator flags.
#
# .py script (design §6.1): the mail directives are guarded by `if EMAIL:`,
# which cactup's literal-only @NAME@ engine (D7) cannot express.

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
if QUEUE == "single" and typed['NODES'] > 1:
    raise CactupError(
        "partition \"single\" only takes one-node jobs, but this testsuite asks for {0} nodes.\n"
        "Submit to workq or checkpt (-q workq) for a multi-node run, or use -n 1.".format(NODES)
    )
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
lines.append("#SBATCH -J {0}".format(TEST_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py scripts (design §6.1).
lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} test run {1} --installation={2} --test-dir={3} --machine={4}"
    " --results-id={5}".format(
        CACTUP, TEST_NAME, ALIAS, TEST_DIR, MACHINE, RESULTS_ID,
    )
)

print("\n".join(lines))
