#! /bin/bash
# Frontier runscript (variant "default"), ported from simfactory2
# mdb/runscripts/frontier.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NODE_PROCS  -> @TASKS_PER_NODE@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Old computed templates became shell arithmetic (cactup's NAME engine is
# literal-only, D7):
#   (PPN_USED * NUM_SMT) -> $(( @TASKS_PER_NODE@ * @CPUS_PER_TASK@ * @THREADS_PER_CPU@ ))
#   (MEMORY * 1024)      -> $(( @MEMORY@ * 1024 ))
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -euxo pipefail

cd @RUNDIR@-active

echo 'Job setup:'
echo '   Allocated:'
echo '      Nodes:                      @NODES@'
echo '      Cores per node:             @MAX_CPUS_PER_NODE@'
echo '   Running:'
echo '      MPI processes:              @TASKS@'
echo '      OpenMP threads per process: @CPUS_PER_TASK@'
echo '      MPI processes per node:     @TASKS_PER_NODE@'
echo '      OpenMP threads per core:    @THREADS_PER_CPU@'
echo "      OpenMP threads per node:    $(( @TASKS_PER_NODE@ * @CPUS_PER_TASK@ * @THREADS_PER_CPU@ ))"

echo "Checking:"
date
env
hostname
pwd

module list

echo "Environment:"
export 'SIMULATION_ID=@SIMULATION_ID@'
export CACTUS_MAX_MEMORY=$(( @MEMORY@ * 1024 )) # Byte
export 'CACTUS_NUM_PROCS=@TASKS@'
export 'CACTUS_NUM_THREADS=@CPUS_PER_TASK@'
export 'CACTUS_SET_THREAD_BINDINGS=1'
export 'GLIBCXX_FORCE_NEW=1'
export 'GMON_OUT_PREFIX=gmon.out'
export 'OMP_DISPLAY_ENV=FALSE'  # false, true
export 'OMP_NUM_THREADS=@CPUS_PER_TASK@'
export 'OMP_PLACES=cores'       # threads, cores, sockets
export 'OMP_PROC_BIND=FALSE'    # false, true, master, close, spread
export 'OMP_STACKSIZE=8192'     # kByte
env | sort >'.cactup/ENVIRONMENT'

echo "Starting:"
date
export CACTUS_STARTTIME=$(date +%s)

time                                            \
    srun                                        \
    --ntasks=@TASKS@                            \
    --ntasks-per-node=@TASKS_PER_NODE@          \
    --gpus=@TASKS@                              \
    --gpus-per-node=@TASKS_PER_NODE@            \
    --gpu-bind=closest                          \
    "@EXECUTABLE@"                              \
    -L 3                                        \
    "@PARFILE@"                                 \
    >stdout.txt                                 \
    2>stderr.txt

echo "Stopping:"
date

echo "Done."
