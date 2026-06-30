#! /bin/bash
# mel5 submitscript (variant "default"), ported from simfactory2 mdb/submitscripts/mel5.sub.
#
# Changes vs. simfactory (design §6.3, §8.3.1):
#   @SIMFACTORY@ run ... -> @CACTUP@ sim run ...
#   added --installation=@ALIAS@ so the compute-node re-invocation can locate
#   the simulation without consulting the global cactup database.
#
# No batch scheduler on mel5: chaining is emulated by waiting for the previous
# job's PID to exit before exec-ing the run. This is plain bash conditional
# logic (the @NAME@ engine does only literal substitution — design §6/§D7), so a
# .sh variant suffices; no .py escape hatch is needed here.

cd @SOURCEDIR@

CHAINED_JOB_ID='@CHAINED_JOB_ID@'
if [ "${CHAINED_JOB_ID}" != '' ]; then
    while ps "${CHAINED_JOB_ID}" >/dev/null; do
        sleep 60
    done
fi

exec @CACTUP@ sim run @SIMULATION_NAME@ \
    --installation=@ALIAS@ --basedir=@BASEDIR@ --machine=@MACHINE@ \
    --restart-id=@RESTART_ID@ @FROM_RESTART_COMMAND@
