# cactup-tutorial submitscript (variant "default").
#
# A .py variant: cactup runs it with every template variable as a global
# (strings; `typed` has them as ints and bools), and what it prints is the job
# script. Python is needed here because some #SBATCH lines are conditional,
# which @NAME@ substitution cannot express:
#   --gpus-per-task      only on a GPU run (the gpu queue, or -g)
#   --dependency         only for the later jobs of a chained run
#   --account            only when -a or the allocation knob names one; this
#                        machine's SLURM keeps no accounts
#   --mail-*             only when -m or the mail knob gives an address
#
# One #SBATCH line per setting, with long option names, so the job script is
# easy to read. cactup's topology flags map onto them one to one:
# -q -> --partition, -w -> --time (per job, when a run is chained),
# -n -> --nodes, -T -> --ntasks, -t -> --ntasks-per-node,
# -c -> --cpus-per-task, -G -> --gpus-per-task.
#
# A request this machine cannot serve is refused with CactupError before
# anything is submitted: there is only one node.

if typed['NODES'] > 1:
    raise CactupError(
        "this machine is a single node, but this run asks for {0} nodes.\n"
        "Leave out -n (--nodes); use -T/-t and -c to shape the run on the one "
        "node.".format(NODES)
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
if CHAINED_JOB_ID:
    lines.append("#SBATCH --dependency=afterany:{0}".format(CHAINED_JOB_ID))
if ALLOCATION:
    lines.append("#SBATCH --account={0}".format(ALLOCATION))
if MAIL:
    lines.append("#SBATCH --mail-user={0}".format(MAIL))
    lines.append("#SBATCH --mail-type={0}".format(MAIL_TYPE.upper()))
lines.append("#SBATCH --output={0}".format(STDOUT_FILE))
lines.append("#SBATCH --error={0}".format(STDERR_FILE))
# A job SLURM requeues (its node rebooted) runs again from the start; append
# rather than truncate, so the log keeps the interrupted attempt too.
lines.append("#SBATCH --open-mode=append")

# env-setup is not auto-prepended for .py variants; it goes after the
# directives (this machine declares none, so it is usually empty).
if ENV_SETUP.strip():
    lines.append(ENV_SETUP.rstrip("\n"))

lines.append("")
lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} sim run {1} --installation={2} --sim-dir={3} --machine={4}"
    " --restart-id={5}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE, RESTART_ID,
    )
)

print("\n".join(lines))
