# nibi-gpu submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/nibi-gpu.sub.
#
# .py variant (design §6.1): the old script used FOUR expression templates —
#   @(@PPN_USED@ == 112 ? "--exclusive" : "")@       (whole-node jobs)
#   --mem=@(int((2044000*@PPN_USED@+@PPN_USED@-1)/@MAX_CPUS_PER_NODE@))@M
#                                    (2T/112 of RAM per used core, ceiling div)
#   --gpus-per-node=@(min(8, @NODE_PROCS@))@         (1 GPU per rank, max 8)
#   @("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@
# — cactup's @NAME@ engine is literal-only (D7); all move into Python
# (PPN_USED = TASKS_PER_NODE * CPUS_PER_TASK).
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NODE_PROCS@/@NUM_THREADS@ -> TASKS_PER_NODE/CPUS_PER_TASK
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# The old script named the job -J @SIMULATION_NAME@ (the full name); kept.

ppn_used = typed["TASKS_PER_NODE"] * typed["CPUS_PER_TASK"]
mem_mb = int((2044000 * ppn_used + ppn_used - 1) / typed["MAX_CPUS_PER_NODE"])
exclusive = "--exclusive " if ppn_used == 112 else " "

lines = ["#! /bin/bash"]
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH --account={0}".format(ALLOCATION))
lines.append("#SBATCH --partition={0}".format(QUEUE))
lines.append("#SBATCH --nodes={0}".format(NODES))
lines.append("#SBATCH {0}--mem={1}M".format(exclusive, mem_mb))
lines.append("#SBATCH --ntasks-per-node={0}".format(TASKS_PER_NODE))
lines.append("#SBATCH --cpus-per-task={0}".format(CPUS_PER_TASK))
lines.append("#SBATCH --gpus-per-node={0}".format(min(8, typed["TASKS_PER_NODE"])))
lines.append("#SBATCH --export=ALL")
lines.append("#SBATCH -J {0}".format(SIMULATION_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
lines.append("#SBATCH --no-requeue")
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH -o {0}".format(STDOUT_FILE))
lines.append("#SBATCH -e {0}".format(STDERR_FILE))

# env-setup is NOT auto-prepended for .py variants (design §6.1); place it
# after the directive header.
lines.append(ENV_SETUP)

lines.append("cd {0}".format(SOURCEDIR))
lines.append(
    "exec {0} sim run {1} --installation={2} --sim-dir={3} --machine={4}"
    " --restart-id={5} {6}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID, FROM_RESTART_COMMAND,
    )
)

print("\n".join(lines))
