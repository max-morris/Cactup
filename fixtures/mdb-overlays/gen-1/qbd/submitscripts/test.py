# qbd TEST submitscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Same #SBATCH header as submitscripts/default.py but WITHOUT
# restart chaining (a testsuite is one-shot — no recovery, no chain, design
# §11.6); the payload re-invokes `cactup test run` on the compute node with
# the §11.6 locator flags (--test-dir + --results-id) instead of `sim run`.
#
# See submitscripts/default.py for why qbd stays on SLURM (#SBATCH) rather
# than following the upstream 2026-07-07 SLURM->PBS flip.
#
# .py variant (design §6.1): the `-p @QUEUE@` and mail directives are guarded
# by `if QUEUE:` and `if EMAIL:` — cactup's @NAME@ engine is literal-only
# (D7), so both conditionals move into Python.

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
if QUEUE:
    lines.append("#SBATCH -p {0}".format(QUEUE))
# --gres restored from the old qbd.sub — see submitscripts/default.py for
# QB4's CPUs-per-gres-GPU rules. A testsuite always takes the full node's GPUs
# (it runs a couple of ranks, so the half-node gpu2 branch never applies).
g_res = 4 if QUEUE == "gpu4" else 2
gpus_wanted = typed['GPUS_PER_TASK'] * typed['TASKS_PER_NODE']
if gpus_wanted > g_res:
    raise CactupError(
        "this testsuite layout binds {0} GPUs per node ({1} per task x {2} tasks/node) but "
        "{3} reserves only --gres=gpu:{4}, and on QB4 only --gres GPUs count.\n"
        "Lower --gpus-per-task or --tpn.".format(
            gpus_wanted, GPUS_PER_TASK, TASKS_PER_NODE, QUEUE, g_res,
        )
    )
lines.append("#SBATCH --gres=gpu:{0}".format(g_res))
lines.append("#SBATCH --gpus-per-task {0}".format(GPUS_PER_TASK))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
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
