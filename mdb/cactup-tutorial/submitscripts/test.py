# cactup-tutorial TEST submitscript (variant "test"), used by `cactup test
# submit`. The same #SBATCH header as submitscripts/default.py, without
# chaining (a testsuite run is a single job). The job re-invokes `cactup test
# run` inside the allocation, where SLURM_JOB_ID (the machine's
# allocation-env) is set, so it is allowed to start.

if typed['NODES'] > 1:
    raise CactupError(
        "this machine is a single node, but this test run asks for {0} "
        "nodes.\nLeave out -n (--nodes).".format(NODES)
    )

lines = ["#!/bin/bash"]
lines.append("#SBATCH --job-name={0}".format(JOB_NAME))
lines.append("#SBATCH --partition={0}".format(QUEUE))
lines.append("#SBATCH --time={0}".format(WALLTIME))
lines.append("#SBATCH --nodes={0}".format(NODES))
lines.append("#SBATCH --ntasks={0}".format(TASKS))
lines.append("#SBATCH --ntasks-per-node={0}".format(TASKS_PER_NODE))
lines.append("#SBATCH --cpus-per-task={0}".format(CPUS_PER_TASK))
if typed['GPU']:
    lines.append("#SBATCH --gpus-per-task={0}".format(GPUS_PER_TASK))
if ALLOCATION:
    lines.append("#SBATCH --account={0}".format(ALLOCATION))
if MAIL:
    lines.append("#SBATCH --mail-user={0}".format(MAIL))
    lines.append("#SBATCH --mail-type={0}".format(MAIL_TYPE.upper()))
lines.append("#SBATCH --output={0}".format(STDOUT_FILE))
lines.append("#SBATCH --error={0}".format(STDERR_FILE))

# env-setup is not auto-prepended for .py variants.
if ENV_SETUP.strip():
    lines.append(ENV_SETUP.rstrip("\n"))

lines.append("")
lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} test run {1} --installation={2} --test-dir={3} --machine={4}"
    " --results-id={5}".format(
        CACTUP, TEST_NAME, ALIAS, TEST_DIR, MACHINE, RESULTS_ID,
    )
)

print("\n".join(lines))
