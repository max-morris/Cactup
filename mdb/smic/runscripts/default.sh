#!/bin/bash
# smic runscript (variant "default"), ported from simfactory2
# mdb/runscripts/smic-openmpi.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
#   (PPN_USED//NUM_THREADS) -> @TASKS_PER_NODE@ (they are equal by definition)
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
export MPICHDIR=/usr/local/packages/openmpi/3.1.5/tr7ckfes
export MPD_NODEFILE=mpd_nodefile
export MV2_SRQ_SIZE=4000        # ???
export LD_LIBRARY_PATH=/usr/local/compilers/Intel/parallel_studio_xe_2019.5/compilers_and_libraries_2019.5.281/linux/compiler/lib/intel64_lin:/usr/local/compilers/Intel/parallel_studio_xe_2019.5/compilers_and_libraries/linux/mkl/lib/intel64:/usr/local/packages/gcc/9.3.0/5jmpgadg/lib64:${LD_LIBRARY_PATH}
export MPI_NODEFILE=mpi_nodefile
env > .cactup/ENVIRONMENT

echo "Starting:"
uniq ${PBS_NODEFILE} > ${MPD_NODEFILE}
for node in $(cat ${MPD_NODEFILE}); do
    for ((proc=0; $proc<@TASKS_PER_NODE@; proc=$proc+1)); do
        echo ${node}
    done
done > ${MPI_NODEFILE}
export CACTUS_STARTTIME=$(date +%s)

if [ @RUNDEBUG@ -eq 0 ]; then
    time ${MPICHDIR}/bin/mpirun -n @TASKS@ -hostfile ${MPI_NODEFILE} -mca I_MPI_OFA_USE_XRC 1 -mca coll_fca_enable 0 /bin/env MV2_ENABLE_AFFINITY=0 OMP_NUM_THREADS=@CPUS_PER_TASK@ @EXECUTABLE@ -L 3 @PARFILE@
else
	export MV2_ENABLE_AFFINIT=0
	export OMP_NUM_THREADS=@CPUS_PER_TASK@
	eval `soft-dec sh add +totalview-8.10.1`
	${MPICHDIR}/bin/mpirun -tv -n @TASKS@ -f ${MPI_NODEFILE} @EXECUTABLE@ -L 3 @PARFILE@
fi

echo "Stopping:"
date

echo "Done."
