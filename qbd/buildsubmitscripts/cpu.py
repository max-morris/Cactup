# qbd BUILD submitscript (variant "cpu"): compile a CPU-only config on one of
# the CPU partitions (`cactup build submit <cfg> -q single`). QB4 makes you
# compile on a compute node; on these partitions no GPU reservation is needed,
# so this is the GPU script without the --gres derivation. Batch header plus
# the compute-node re-invocation, nothing else.

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
