#! /bin/bash
# Sciama submitscript (variant "default"), ported from simfactory2
# mdb/submitscripts/sciama.sub. The old script had NO restart-chaining
# directive; kept that way.
#
# A .sh variant works here since the header is fully static: cactup inserts
# env-setup after the leading directive/comment block (design §6.1), so the
# #SBATCH lines stay on top. (This was a .py variant back when env-setup was
# inserted right after the shebang.)
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   --out/--error use STDOUT_FILE/STDERR_FILE — the old script wrote plain
#     @SIMULATION_NAME@.{out,err} (submission-cwd-relative); the canonical
#     @RUNDIR@-based defaults are where the machine's stdout/stderr commands
#     look for them.
#   @@SIMFACTORY@@ run --basedir=… -> @CACTUP@ sim run --installation/--sim-dir/
#     --machine (the compute-node re-invocation locator)
# Kept verbatim: --tasks-per-node=@MAX_CPUS_PER_NODE@ and the (sic) --jobname spelling.
#SBATCH --partition @QUEUE@
#SBATCH --time=@WALLTIME@
#SBATCH --nodes=@NODES@
#SBATCH --tasks-per-node=@MAX_CPUS_PER_NODE@
#SBATCH --export=ALL
#SBATCH --jobname=@SHORT_SIMULATION_NAME@
#SBATCH --out @STDOUT_FILE@
#SBATCH --error @STDERR_FILE@
cd @SOURCEDIR@ || exit 1
exec @CACTUP@ sim run @SIMULATION_NAME@ --installation=@ALIAS@ --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ --restart-id=@RESTART_ID@
