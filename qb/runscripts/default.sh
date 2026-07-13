#! /bin/bash
# qb runscript (variant "default"), ported from simfactory2
# mdb/runscripts/qb-mvapich2.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
#   (PPN_USED/NUM_THREADS) -> @TASKS_PER_NODE@ (they are equal by definition)
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date
cat ${PBS_NODEFILE} > .cactup/NODES

echo "Environment:"
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export MPD_NODEFILE=mpd_nodefile
export MV2_SRQ_SIZE=4000        # ???
#export MV2_USE_RING_STARTUP=0
export MPI_NODEFILE=mpi_nodefile
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
uniq ${PBS_NODEFILE} > ${MPD_NODEFILE}
for node in $(cat ${MPD_NODEFILE}); do
    for ((proc=0; $proc<@TASKS_PER_NODE@; proc=$proc+1)); do
        echo ${node}
    done
done > ${MPI_NODEFILE}
export CACTUS_STARTTIME=$(date +%s)

if [ @RUNDEBUG@ -eq 0 ]; then
    time mpirun -np @TASKS@ -hostfile ${MPI_NODEFILE} /bin/env MV2_ENABLE_AFFINITY=0 OMP_NUM_THREADS=@CPUS_PER_TASK@ @EXECUTABLE@ -L 3 @PARFILE@
else
	export MV2_ENABLE_AFFINIT=0
	export OMP_NUM_THREADS=@CPUS_PER_TASK@
	if [ @DEBUGGER@ == "totalview" ]; then
	    eval `module load totalview/8.12.1`
	elif [ @DEBUGGER@ == "ddt" ]; then
	    DDTDIR=/usr/local/packages/license/allinea/4.2.2/bin
	    eval `module load ddt/4.2.2`
	    export TOTALVIEW=${DDTDIR}/ddt-debugger-mps
	fi

	mpirun -tv -np @TASKS@ -hostfile ${MPI_NODEFILE} @EXECUTABLE@ -L 3 @PARFILE@
fi

echo "Stopping:"
date

echo "Done."
