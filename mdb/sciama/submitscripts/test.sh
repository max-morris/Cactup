#! /bin/bash
# Sciama TEST submitscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Same #SBATCH header as submitscripts/default.sh; the payload
# re-invokes `cactup test run` on the compute node with the §11.6 locator
# flags (--test-dir + --results-id) instead of `sim run`.
#
# A .sh variant works here since the header is fully static: cactup inserts
# env-setup after the leading directive/comment block (design §6.1), so the
# #SBATCH lines stay on top. (This was a .py variant back when env-setup was
# inserted right after the shebang.)
#SBATCH --partition @QUEUE@
#SBATCH --time=@WALLTIME@
#SBATCH --nodes=@NODES@
#SBATCH --tasks-per-node=@MAX_CPUS_PER_NODE@
#SBATCH --export=ALL
#SBATCH --jobname=@TEST_NAME@
#SBATCH --out @STDOUT_FILE@
#SBATCH --error @STDERR_FILE@
cd @SOURCEDIR@ || exit 1
exec @CACTUP@ test run @TEST_NAME@ --installation=@ALIAS@ --test-dir=@TEST_DIR@ --machine=@MACHINE@ --results-id=@RESULTS_ID@
