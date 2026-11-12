#! /bin/bash
# frontera TEST submitscript (script "test") — marked test = true in
# meta.toml (design §11.2). Same #SBATCH header as submitscripts/default.py
# but WITHOUT restart chaining (a testsuite is one-shot — no recovery, no
# chain, design §11.6); the payload re-invokes `cactup test run` on the
# compute node with the §11.6 locator flags (--test-dir + --results-id)
# instead of `sim run`.
#
# A .sh script works here since the header is fully static: cactup inserts
# env-setup after the leading directive/comment block (design §6.1), so the
# #SBATCH lines stay on top. (This was a .py script back when env-setup was
# inserted right after the shebang.)
# Account optional: TACC charges the user's default project when none is
# named. The allocation knob, which -a sets for one command; with none, the
# line is dropped (an empty one makes the scheduler read the next directive as
# the account).
#SBATCH -A @KNOB-OR-DROP-LINE(allocation)@
#SBATCH -p @QUEUE@
#SBATCH -t @WALLTIME@
#SBATCH -N @NODES@ -n @TASKS@
#SBATCH -J @TEST_NAME@
#SBATCH -o @STDOUT_FILE@
#SBATCH -e @STDERR_FILE@
cd @SOURCEDIR@ || exit 1
exec @CACTUP@ test run @TEST_NAME@ --installation=@ALIAS@ --test-dir=@TEST_DIR@ --machine=@MACHINE@ --results-id=@RESULTS_ID@
