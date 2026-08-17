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
#   --gpus-per-task now carries the §8.5 GPUS_PER_TASK variable. qbd sets
#     `default-gpus-per-task = 1`, so it is 1 unless the user overrides it —
#     including the half-node gpu2 case, which asks for the one GPU its
#     --gres=gpu:1 actually reserves.
#
# --gres and --gpus-per-task answer different questions here: --gres is what
# the job RESERVES on each node (CPU-derived, per QB4's rules) and
# --gpus-per-task is how those are BOUND to ranks. They can disagree, and QB4
# will not tell you why, so the two guards below refuse the run instead (§6.1).
#   @SIMFACTORY@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)

lines = ["#! /bin/bash"]
lines.append("#SBATCH -A {0}".format(ALLOCATION))
if QUEUE:
    lines.append("#SBATCH -p {0}".format(QUEUE))
if QUEUE == "gpu4":
    g_res = 4  # On gpu4, we always want the entire node
elif typed['CPUS_PER_TASK'] * typed['TASKS_PER_NODE'] == 32:
    g_res = 1  # We can request half-nodes on gpu2 if we only use a total of 32 cpus per node
else:
    g_res = 2  # Requesting full nodes on gpu2

# QB4's two hard rules, checked before anything is submitted (§6.1
# CactupError). Both are properties of the --gres request above, which is
# derived from the CPU layout, so cactup's own §8.5 GPU check cannot see them.
gpus_wanted = typed['GPUS_PER_TASK'] * typed['TASKS_PER_NODE']
if gpus_wanted > g_res:
    # What actually fixes it depends on which side is too big. At 1 GPU/task
    # there is nothing left to lower, so only the CPU layout can help — and on
    # gpu2 the lever is counter-intuitive: asking for MORE CPUs per node widens
    # --gres, because the half-node branch triggers on exactly 32.
    if typed['GPUS_PER_TASK'] > 1:
        fix = "Use --gpus-per-task {0}, or spread the ranks over more nodes.".format(
            max(1, g_res // max(1, typed['TASKS_PER_NODE']))
        )
    elif QUEUE == "gpu2" and typed['CPUS_PER_TASK'] * typed['TASKS_PER_NODE'] == 32:
        fix = ("Every rank already takes one GPU, so widen the reservation instead: gpu2 only "
               "reserves 1 GPU when a node uses exactly 32 CPUs. Raise --cpus (e.g. --cpus 32 "
               "for 2 ranks/node), or drop to --tpn 1.")
    else:
        fix = "Every rank already takes one GPU, so use --tpn {0} or fewer.".format(g_res)
    raise CactupError(
        "this layout binds {0} GPUs per node ({1} per task x {2} tasks/node) but QB4's rules "
        "make {3} reserve only --gres=gpu:{4} for it.\n"
        "On QB4 only GPUs asked for with --gres count, so the ranks would fight over {4}.\n"
        "{5}".format(gpus_wanted, GPUS_PER_TASK, TASKS_PER_NODE, QUEUE, g_res, fix)
    )
# gpu2 caps CPUs per gres-requested GPU at 32; exceeding it is rejected at
# submit time by the site, with a message that does not say why.
cpus_wanted = typed['CPUS_PER_TASK'] * typed['TASKS_PER_NODE']
if QUEUE == "gpu2" and cpus_wanted > 32 * g_res:
    raise CactupError(
        "gpu2 allows at most 32 CPUs per requested GPU, but this layout wants {0} CPUs per node "
        "({1} per task x {2} tasks/node) against --gres=gpu:{3}.\n"
        "Lower --cpus/--tpn, or submit to gpu4.".format(
            cpus_wanted, CPUS_PER_TASK, TASKS_PER_NODE, g_res,
        )
    )

lines.append("#SBATCH --gres=gpu:{0}".format(g_res))
lines.append("#SBATCH --gpus-per-task {0}".format(GPUS_PER_TASK))
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
