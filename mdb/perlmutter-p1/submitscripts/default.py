# perlmutter-p1 submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/perlmutter-p1.sub.
#
# .py variant (design §6.1): the old script used the chained-job ternary
# @("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@ — cactup's
# @NAME@ engine is literal-only (D7), so the conditional moves into Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@/@NODE_PROCS@/@NUM_THREADS@ -> TASKS/TASKS_PER_NODE/CPUS_PER_TASK
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# Kept verbatim: the NERSC GPU-account suffix (-A <allocation>_g), -C gpu,
# --gpus-per-task 1, and --gpu-bind=map_gpu:0,1,2,3.

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}_g".format(ALLOCATION))
lines.append("#SBATCH -C gpu")
lines.append("#SBATCH -p {0}".format(QUEUE))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1} -c {2}".format(NODES, TASKS, CPUS_PER_TASK))
lines.append("#SBATCH --ntasks-per-node {0}".format(TASKS_PER_NODE))
lines.append("#SBATCH --gpus-per-task 1")
lines.append("#SBATCH --gpu-bind=map_gpu:0,1,2,3")
if CHAINED_JOB_ID:
    lines.append("#SBATCH -d afterany:{0}".format(CHAINED_JOB_ID))
lines.append("#SBATCH -J {0}".format(SHORT_SIMULATION_NAME))
if EMAIL:
    lines.append("#SBATCH --mail-type=ALL")
    lines.append("#SBATCH --mail-user={0}".format(EMAIL))
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
