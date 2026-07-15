#! /bin/bash
# slurmjupyter TEST submitscript (variant "test") — marked test = true in
# meta.toml (design §11.2). Same #SBATCH header as submitscripts/default.py
# but WITHOUT restart chaining (a testsuite is one-shot — no recovery, no
# chain, design §11.6); the payload re-invokes `cactup test run` on the
# compute node with the §11.6 locator flags (--test-dir + --results-id)
# instead of `sim run`.
#
# A .sh variant works here since the header is fully static: cactup inserts
# env-setup after the leading directive/comment block (design §6.1), so the
# #SBATCH lines stay on top. (This was a .py variant back when env-setup was
# inserted right after the shebang.)
#SBATCH -t @WALLTIME@
#SBATCH -N @NODES@ -n @TASKS@
#SBATCH --ntasks-per-node=@TASKS_PER_NODE@
#SBATCH -J @TEST_NAME@
#SBATCH -o @STDOUT_FILE@
#SBATCH -e @STDERR_FILE@
cd @SOURCEDIR@ || exit 1
exec @CACTUP@ test run @TEST_NAME@ --installation=@ALIAS@ --test-dir=@TEST_DIR@ --machine=@MACHINE@ --results-id=@RESULTS_ID@
