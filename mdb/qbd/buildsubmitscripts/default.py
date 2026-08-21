# qbd BUILD submitscript (variant "default").
#
# QB4 makes you compile on a compute node. That used to be done by wrapping
# `make` in a [universes.compute] shell snippet that sbatch'ed itself, tailed
# its own log and scraped sacct for an exit status; cactup now submits builds
# through the scheduler like any other job, so all of that is gone and this
# file is just the batch header plus the compute-node re-invocation.
#
# .py rather than .sh for one reason: the --gres derivation below. cactup's
# @NAME@ engine is literal-only (D7), and QB4 counts only GPUs asked for with
# --gres against its CPUs-per-GPU cap, so the reservation has to be computed.

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))

# GPUs to reserve. A build needs none of them to compile, but on gpu2 the cap
# is 32 CPUs per *requested* GPU, so a full-node `make -j64` has to hold both
# of the node's GPUs to be allowed its cores at all. Same rule the run
# submitscript applies, collapsed to the build's fixed one-task shape.
cpus = typed['CPUS_PER_TASK']
if QUEUE == "gpu4":
    g_res = 4                      # gpu4 is only ever taken whole
elif cpus <= 32:
    g_res = 1                      # half a gpu2 node
else:
    g_res = 2                      # a whole gpu2 node

if QUEUE == "gpu2" and cpus > 32 * g_res:
    raise CactupError(
        "gpu2 allows at most 32 CPUs per requested GPU, so a build reserving "
        "--gres=gpu:{0} may use at most {1} cores, but this one asks for {2}.\n"
        "Lower --cpus (it defaults to the machine's make-jobs), or build on "
        "gpu4.".format(g_res, 32 * g_res, cpus)
    )

lines.append("#SBATCH --gres=gpu:{0}".format(g_res))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
lines.append("#SBATCH -J {0}".format(JOB_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py variants (design §6.1); place it
# after the directive header. The build itself re-applies the build-phase
# env-setup from its own frozen script, so this only has to be enough to
# reach the cactup binary.
lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} build run {1} --installation={2} --config-dir={3} --machine={4}"
    " --attempt-id={5}".format(
        CACTUP, CONFIGURATION, ALIAS, CONFIG_DIR, MACHINE, ATTEMPT_ID,
    )
)

print("\n".join(lines))
