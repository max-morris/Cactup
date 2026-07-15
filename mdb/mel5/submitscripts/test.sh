#! /bin/bash
# mel5 TEST submitscript (variant "test") — marked test = true in meta.toml
# (design §11.2). It re-invokes cactup on the "compute node" to run the
# testsuite, analogous to submitscripts/default.sh but for `cactup test run`
# and WITHOUT restart chaining: a testsuite is one-shot (no recovery, no chain,
# no restart bookkeeping — design §11.6), so the PID-wait chaining loop the
# normal submitscript uses is absent here.
#
# Compute-node locator flags (design §11.6): --test-dir + --results-id fully
# identify the result set without consulting the global cactup database or the
# per-installation registry. TEST_DIR / TEST_NAME / RESULTS_ID are the
# testsuite-only variables from design §11.9.
#
# No batch scheduler on mel5: cactup's "submit" just backgrounds this script and
# echoes its PID (see meta.toml [scheduler].submit); status/stop use ps/kill.

cd @SOURCEDIR@ || exit 1

exec @CACTUP@ test run @TEST_NAME@ \
    --installation=@ALIAS@ --test-dir=@TEST_DIR@ --machine=@MACHINE@ \
    --results-id=@RESULTS_ID@
