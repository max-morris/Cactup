#! /bin/bash
# fuchs runscript (variant "default"), ported from simfactory2
# mdb/runscripts/fuchs.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
#   (PPN_USED/NUM_THREADS) -> @TASKS_PER_NODE@ (they are equal by definition)
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3); .cactup/NODES is also
# read by the machine's exec-host command.

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@

if [ ! -z "$MPI_NODEFILE" ]; then
    cat ${MPI_NODEFILE} > .cactup/NODES
    export MPI_NODEFILE=${TMPDIR}/machines

    uniq ${MPI_NODEFILE} > PROC_NODES

    for node in $(cat PROC_NODES); do
        for (( proc=0; $proc<@TASKS_PER_NODE@; proc=$proc+1)); do
            echo ${node}
        done
    done > ${MPI_NODEFILE}
fi

env | sort > .cactup/ENVIRONMENT

export CACTUS_STARTTIME=$(date +%s)
echo "Starting:"

time srun -n @TASKS@  @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date

echo "Done."
