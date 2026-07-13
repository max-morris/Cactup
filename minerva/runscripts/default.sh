#! /bin/bash
# minerva runscript (variant "default"), ported from simfactory2
# mdb/runscripts/minerva.run.
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
env | sort > .cactup/ENVIRONMENT
echo ${SLURM_NODELIST} > NODES

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

export I_MPI_FABRICS=shm:tmi

mpiexec.hydra -genv MALLOC_MMAP_MAX_=0 -genv MALLOC_TRIM_THRESHOLD_=536870912 -n @TASKS@ @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date

echo "Done."
