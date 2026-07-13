#! /bin/bash
# perlmutter-p1 runscript (variant "default"), ported from simfactory2
# mdb/runscripts/perlmutter-p1.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

module list

echo "Checking:"
pwd
hostname
date
# TODO: This does not work (upstream comment — PBS_NODES on a SLURM machine)
cat ${PBS_NODES} > .cactup/NODES

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_PLACES=cores # TODO: maybe use threads when smt is used?
export SLURM_CPU_BIND="cores"
env | sort > .cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)
srun @EXECUTABLE@ -L 3 @PARFILE@
echo "Stopping:"
date

echo "Done."
