# db1.hpc.lsu.edu BUILD submitscript (script "default"), chosen by the gpu
# queue for every flavor.
#
# Builds on a compute node rather than the shared login node: a whole node's
# 48 cores instead of a polite -j8. A Singularity flavor's build universe
# wraps make in its own srun, which runs as a step of this job. Mirrors qbd's
# buildsubmitscripts/gpu-partition.py, minus its --gres derivation: the gpu
# partition bills only GPUs and caps nothing per GPU, so a build reserves
# none.

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
lines.append("#SBATCH -J {0}".format(JOB_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py scripts (design §6.1); place it
# after the directive header. The build itself re-applies the build-phase
# env-setup (its build universe's) from its own frozen script, so this only has
# to be enough to reach the cactup binary.
lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} build run {1} --installation={2} --config-dir={3} --machine={4}"
    " --attempt-id={5}".format(
        CACTUP, CONFIGURATION, ALIAS, CONFIG_DIR, MACHINE, ATTEMPT_ID,
    )
)

print("\n".join(lines))
