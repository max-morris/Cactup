#! /bin/bash
# saturn submitscript (variant "cpu"). Written 2026-09-29; copied from
# mdb/et-juphub/submitscripts/default.sh (simfactory2 generic.sub), because
# Frank has no batch scheduler: cactup's "submit" backgrounds this script with
# nohup and records its PID as the job id (meta.toml [scheduler]). Nothing
# here depends on the hardware; the run itself is shaped by runscripts/cpu.sh.
#
# Chaining is emulated by waiting for the previous job's PID to exit. Plain
# bash conditional (the NAME engine does literal substitution only -- §6/§D7),
# so a .sh variant suffices.

cd @SOURCEDIR@ || exit 1

CHAINED_JOB_ID='@CHAINED_JOB_ID@'
if [ "${CHAINED_JOB_ID}" != '' ]; then
    while ps "${CHAINED_JOB_ID}" >/dev/null; do
        sleep 60
    done
fi

exec @CACTUP@ sim run @SIMULATION_NAME@ \
    --installation=@ALIAS@ --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ \
    --restart-id=@RESTART_ID@
