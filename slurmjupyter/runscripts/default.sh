#! /bin/bash
# slurmjupyter runscript (variant "default"), ported from simfactory2
# mdb/runscripts/slurmjupyter.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

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
export OMP_PLACES=cores # TODO: maybe use threads when smt is used?
# https://github.com/open-mpi/ompi/issues/4948
export OMPI_MCA_btl_vader_single_copy_mechanism=none
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)
time srun -n ${CACTUS_NUM_PROCS} --cpu-bind=none --overlap @EXECUTABLE@ -L 3 @PARFILE@
echo "Stopping:"
date

echo "Done."
