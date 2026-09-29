#! /bin/bash
# saturn TEST submitscript (variant "test") -- marked test = true in meta.toml
# (design §11.2). Written 2026-09-29; copied from
# mdb/et-juphub/submitscripts/test.sh. Re-invokes cactup on the node to run
# the testsuite; one-shot, no restart chaining (design §11.6).
#
# No batch scheduler: cactup's "submit" backgrounds this script and echoes its
# PID as the job id (meta.toml [scheduler].submit); status/stop use ps/pkill.

cd @SOURCEDIR@ || exit 1

exec @CACTUP@ test run @TEST_NAME@ \
    --installation=@ALIAS@ --test-dir=@TEST_DIR@ --machine=@MACHINE@ \
    --results-id=@RESULTS_ID@
