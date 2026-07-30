# qbd submitscript (variant "default"), ported from simfactory2.
#
# NOTE: upstream simfactory-configure.py flipped this machine's scheduler
# SLURM->PBS when it regenerated the mdb on 2026-07-07. We do not follow that
# flip: qbd (LSU/LONI Queen Bee 4) runs SLURM, matching its sibling LONI
# machine qbc.loni.org, so these directives are #SBATCH.
#
# .py variant (design §6.1): the `-p @QUEUE@` directive is guarded by
# `if QUEUE:` and restart chaining is `-d afterany` guarded by CHAINED_JOB_ID
# — cactup's @NAME@ engine is literal-only (D7), so both conditionals move
# into Python.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @NUM_PROCS@/@NUM_THREADS@ -> TASKS/CPUS_PER_TASK
#   -o/-e use STDOUT_FILE/STDERR_FILE (same @RUNDIR@/@SIMULATION_NAME@.{out,err}
#     defaults, but they honor cactup's -o/-e flags)
#   mail directives guarded by `if EMAIL:` (the old script emitted them always)
#   `-p @QUEUE@` kept guarded by `if QUEUE:` (belt-and-braces; meta.toml now
#     defines the real gpu2/gpu4 partitions, so QUEUE is always set)
#   --gres=gpu:{4,2} restored from the old qbd.sub (the upstream regeneration
#     dropped it): QB4 caps CPUs per gres-requested GPU (32 on gpu2) and only
#     counts GPUs asked for via --gres, so every job must request the node's
#     full GPU complement — 4 on gpu4, else 2
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
if QUEUE:
    lines.append("#SBATCH -p {0}".format(QUEUE))
if QUEUE == "gpu4":
    g_res = 4  # On gpu4, we always want the entire node
else if typed['CPUS_PER_TASK'] * typed['TASKS_PER_NODE'] == 32:
    g_res = 1  # We can request half-nodes on gpu2 if we only use a total of 32 cpus per node
else:
    g_res = 2  # Requesting full nodes on gpu2
lines.append("#SBATCH --gres=gpu:{0}".format(g_res))
lines.append("#SBATCH -t {0}".format(WALLTIME))
lines.append("#SBATCH -N {0} -n {1}".format(NODES, TASKS))
lines.append("#SBATCH --cpus-per-task {0}".format(CPUS_PER_TASK))
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
    " --restart-id={5}".format(
        CACTUP, SIMULATION_NAME, ALIAS, SIMULATION_DIR, MACHINE,
        RESTART_ID,
    )
)

print("\n".join(lines))
