#! /bin/bash
# generic BUILD submitscript (script "default"), used by `cactup build submit`
# (design §7.9). Re-invokes cactup to build the frozen attempt; builds never
# chain, so there is nothing to wait for first.
#
# No batch scheduler: cactup's "submit" backgrounds this script and echoes its
# PID as the job id (meta.toml [scheduler].submit); status/stop use ps/pkill.
# A bare `cactup build` still builds in the foreground ([build].default-action).

cd @SOURCEDIR@ || exit 1

exec @CACTUP@ build run @CONFIGURATION@ \
    --installation=@ALIAS@ --config-dir=@CONFIG_DIR@ --machine=@MACHINE@ \
    --attempt-id=@ATTEMPT_ID@
