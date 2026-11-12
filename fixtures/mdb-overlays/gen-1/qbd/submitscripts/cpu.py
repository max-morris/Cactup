# qbd submitscript (variant "cpu"): CPU partitions single/workq/checkpt/bigmem.
#
# Same shape as qbc.loni.org's script (plain -N/-n/--cpus-per-task, no --gres,
# no --gpus-per-task): none of QB4's GPU gres rules apply on these partitions.
#
# Site policy encoded here (§6.1 CactupError): `single` is for one-node jobs.
# Multi-node runs must go to workq or checkpt. Full-node layouts on the
# multi-node partitions ask for --exclusive so the DefMemPerCPU accounting
# (3920 MB x CPUs) never caps a node below its 256 GB.

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
lines.append("#SBATCH -p {0}".format(QUEUE))
if QUEUE == "single" and typed['NODES'] > 1:
    raise CactupError(
        "partition \"single\" only takes one-node jobs, but this run asks for {0} nodes.\n"
        "Submit to workq or checkpt (-q workq) for a multi-node run, or use -n 1.".format(NODES)
    )
if QUEUE != "single" and typed['CPUS_PER_TASK'] * typed['TASKS_PER_NODE'] >= 64:
    lines.append("#SBATCH --exclusive")
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --ntasks-per-node={0}".format(TASKS_PER_NODE))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH -J {0}".format(SHORT_SIMULATION_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

lines.append(f"# INFO: CPUS_PER_TASK={CPUS_PER_TASK}, TASKS_PER_NODE={TASKS_PER_NODE}")

# env-setup is NOT auto-prepended for .py variants (design §6.1); place it
# after the directive header.
lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} sim run {1} --installation={2} --sim-dir={3} --machine={4}"
    " --restart-id={5}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID,
    )
)

print("\n".join(lines))
