#! /bin/bash
# hal1 submitscript (variant "default"), ported from simfactory2 generic.sub.
# Changes vs simfactory (design §6.3, §8.3.1): SIMFACTORY run → @CACTUP@ sim run,
# added --installation=@ALIAS@ for the compute-node re-invocation locator.
#
# No batch scheduler: chaining is emulated by waiting for the previous job's PID
# to exit. Plain bash conditional (the NAME engine does literal substitution
# only — §6/§D7), so a .sh variant suffices.

cd @SOURCEDIR@

CHAINED_JOB_ID='@CHAINED_JOB_ID@'
if [ "${CHAINED_JOB_ID}" != '' ]; then
    while ps "${CHAINED_JOB_ID}" >/dev/null; do
        sleep 60
    done
fi

exec @CACTUP@ sim run @SIMULATION_NAME@ \
    --installation=@ALIAS@ --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ \
    --restart-id=@RESTART_ID@ @FROM_RESTART_COMMAND@
