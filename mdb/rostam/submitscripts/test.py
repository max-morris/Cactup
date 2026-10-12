# rostam TEST submitscript — marked test = true in meta.toml.
# Same #SBATCH header as submitscripts/default.py but no restart chaining
# (a testsuite is one-shot, design §11.6). Payload re-invokes
# `cactup test run` on the compute node.

lines = ["#! /bin/bash"]
if ALLOCATION:
    lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
if typed["GPU"]:
    lines.append("#SBATCH --gpus-per-task {0}".format(GPUS_PER_TASK))
    lines.append("#SBATCH --gres=gpu:{0}".format(
        typed["TASKS_PER_NODE"] * typed["GPUS_PER_TASK"]))
lines.append("#SBATCH -J {0}".format(TEST_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} test run {1} --installation={2} --test-dir={3} --machine={4}"
    " --results-id={5}".format(
        CACTUP, TEST_NAME, ALIAS, TEST_DIR, MACHINE, RESULTS_ID,
    )
)

print("\n".join(lines))
