#! /bin/bash
# hopper TEST submitscript (script "test"), written 2026-10-14 as a copy of
# mdb/athena's (itself from mdb/et-juphub/submitscripts/test.sh): no batch
# system in practice.
# Marked test = true in meta.toml (design §11.2). Re-invokes cactup on the
# "compute node" to run the testsuite; one-shot, no restart chaining (design
# §11.6). Mirrors mdb/mel5/submitscripts/test.sh.
#
# No batch scheduler: cactup's "submit" backgrounds this script and echoes its
# PID as the job id (meta.toml [scheduler].submit); status/stop use ps/pkill.

cd @SOURCEDIR@ || exit 1

exec @CACTUP@ test run @TEST_NAME@ \
    --installation=@ALIAS@ --test-dir=@TEST_DIR@ --machine=@MACHINE@ \
    --results-id=@RESULTS_ID@
