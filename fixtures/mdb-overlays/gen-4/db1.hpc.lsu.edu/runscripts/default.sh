#! /bin/bash
# db1.hpc.lsu.edu runscript (script "default"), ported from simfactory2
# mdb/runscripts/db-new.run (upstream's native rewrite — no more Singularity
# image; module-loads gcc/cuda/mpich and srun-launches the binary directly).
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).
#
# The srun below binds GPUs with --gpus-per-task=@GPUS_PER_TASK@ inside the
# allocation the submitscript reserved with --gres; it passes no --gres of its
# own. (Upstream branched on an unexported $QUEUE to add one, which never ran.)

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

module purge
module load gcc/9.3.0
module load cuda/12.4.0 mpich/3.3.2/intel-19.1.3
module list

echo "Checking:"
pwd
hostname
date

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_PLACES=cores
export OMP_STACKSIZE=8192
env | sort > .cactup/ENVIRONMENT

#if [ $(((@TASKS@*@CPUS_PER_TASK@)%48)) != 0 -a $(((@TASKS@*@CPUS_PER_TASK@))) != 24 ]
#then
#    echo "Deep Bayou requires you either use half a node (24 cores),"
#    echo "or multiple whole nodes (multiples of 48 cores)."
#    echo "Please adjust your call to simfactory accordingly."
#    exit 2
#fi

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

# GPU clocks and temperatures before and after, on the node the batch script
# runs on (the first allocated node), so a timing run can be checked for
# thermal or power throttling after the fact.
nvidia-smi -q -d CLOCK,TEMPERATURE > nvidia-smi.before.txt 2>&1 || true

time srun --overlap -n @TASKS@ \
    --cpus-per-task=@CPUS_PER_TASK@ \
    --gpus-per-task=@GPUS_PER_TASK@ \
    @EXECUTABLE@ -L 3 @PARFILE@

nvidia-smi -q -d CLOCK,TEMPERATURE > nvidia-smi.after.txt 2>&1 || true

echo "Stopping:"
date

echo "Done."
