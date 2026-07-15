#! /bin/bash
# wheeler runscript (variant "default"), ported from simfactory2
# mdb/runscripts/wheeler-intel.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
#   NODE_PROCS  -> @TASKS_PER_NODE@
#   (PPN_USED / NUM_THREADS) -> @TASKS_PER_NODE@ (equal by definition)
#   PPN_USED -> $(( @TASKS_PER_NODE@ * @CPUS_PER_TASK@ )) — the "threads per
#     node" echo below uses double quotes (upstream single) so the shell
#     arithmetic expands.
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date

echo '[BEGIN NODES]'
# Generate host list
echo $SLURM_NODELIST
hostfile=".cactup/NODES"
for i in $(seq 1 $SLURM_JOB_CPUS_PER_NODE); do
    scontrol show hostnames "$SLURM_NODELIST"
done | sort >${hostfile}
echo '[END NODES]'

echo '[BEGIN IFCONFIG]'
/sbin/ifconfig || true
echo '[END IFCONFIG]'

echo 'Job setup:'
echo '   Allocated:'
echo '      Nodes:                      @NODES@'
echo '      Cores per node:             @MAX_CPUS_PER_NODE@'
echo '   Running:'
echo '      MPI processes:              @TASKS@'
echo '      OpenMP threads per process: @CPUS_PER_TASK@'
echo '      MPI processes per node:     @TASKS_PER_NODE@'
echo '      OpenMP threads per core:    @THREADS_PER_CPU@'
echo "      OpenMP threads per node:    $(( @TASKS_PER_NODE@ * @CPUS_PER_TASK@ ))"

echo "Environment:"
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_STACKSIZE=8192
export OMP_MAX_TASKS=500
export CXX_MAX_TASKS=500
export PTHREAD_MAX_TASKS=500
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export CACTUS_SET_THREAD_BINDINGS=1
env | sort >.cactup/ENVIRONMENT

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

time                                            \
    mpirun                                      \
    -np @TASKS@                                 \
    -hostfile "${hostfile}"                     \
    -ppn @TASKS_PER_NODE@                       \
    -print-rank-map                             \
    -env OMP_NUM_THREADS "$OMP_NUM_THREADS"     \
    -env OMP_STACKSIZE "$OMP_STACKSIZE"         \
    -env OMP_MAX_TASKS "$OMP_MAX_TASKS"         \
    -env CXX_MAX_TASKS "$CXX_MAX_TASKS"         \
    -env PTHREAD_MAX_TASKS "$PTHREAD_MAX_TASKS" \
    -env CACTUS_NUM_PROCS "$CACTUS_NUM_PROCS"   \
    -env CACTUS_NUM_THREADS "$CACTUS_NUM_THREADS" \
    -env CACTUS_SET_THREAD_BINDINGS "$CACTUS_SET_THREAD_BINDINGS" \
    -env CACTUS_STARTTIME "$CACTUS_STARTTIME"   \
    @EXECUTABLE@ -L 3 @PARFILE@

echo "Stopping:"
date

echo "Done."
