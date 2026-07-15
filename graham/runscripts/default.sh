#! /bin/bash
# graham runscript (variant "default"), ported from simfactory2
# mdb/runscripts/graham.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NODE_PROCS  -> @TASKS_PER_NODE@
#   NUM_THREADS -> @CPUS_PER_TASK@
#   PPN_USED               -> $(( @TASKS_PER_NODE@ * @CPUS_PER_TASK@ ))
#   (MEMORY * 1024) template -> $(( @MEMORY@ * 1024 ))
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -euxo pipefail

cd @RUNDIR@-active

echo "Job setup:"
echo "   Allocated:"
echo "      Nodes:                      @NODES@"
echo "      Cores per node:             @MAX_CPUS_PER_NODE@"
echo "   Running:"
echo "      MPI processes:              @TASKS@"
echo "      OpenMP threads per process: @CPUS_PER_TASK@"
echo "      MPI processes per node:     @TASKS_PER_NODE@"
echo "      OpenMP threads per core:    @THREADS_PER_CPU@"
echo "      OpenMP threads per node:    $(( @TASKS_PER_NODE@ * @CPUS_PER_TASK@ ))"

echo "Checking:"
date
env
hostname
pwd

module list

scontrol show hostnames
hostfile=".cactup/NODES"
scontrol show hostnames |
    awk '{ print $1, "slots=@TASKS_PER_NODE@"; }' >"$hostfile"

/sbin/ifconfig || true

ompi_info

case @CPUS_PER_TASK@ in
    (1) bind_to=core; map_by=ppr:1:core;;
    (2) bind_to=socket; map_by=ppr:8:socket;;
    (4) bind_to=socket; map_by=ppr:4:socket;;
    (8) bind_to=socket; map_by=ppr:2:socket;;
    (16) bind_to=socket; map_by=ppr:1:socket;;
    (32) bind_to=none; map_by=ppr:1:node;;
    (*) bind_to=none; map_by=node;;
esac

echo "Environment:"
export "SIMULATION_ID=@SIMULATION_ID@"
export "CACTUS_MAX_MEMORY=$(( @MEMORY@ * 1024 ))" # Byte
export "CACTUS_NUM_PROCS=@TASKS@"
export "CACTUS_NUM_THREADS=@CPUS_PER_TASK@"
export "CACTUS_SET_THREAD_BINDINGS=1"
export "GLIBCXX_FORCE_NEW=1"
export "GMON_OUT_PREFIX=gmon.out"
export "OMP_DISPLAY_ENV=TRUE"
export "OMP_NUM_THREADS=@CPUS_PER_TASK@"
export "OMP_PLACES=cores"       # threads, cores, sockets
export "OMP_PROC_BIND=FALSE"    # false, true, master, close, spread
export "OMP_STACKSIZE=8192"     # kByte
env | sort >".cactup/ENVIRONMENT"

# ulimit -c unlimited

echo "Starting:"
date
export CACTUS_STARTTIME=$(date +%s)

time						\
    mpiexec					\
    --hostfile "${hostfile}"			\
    --n @TASKS@				\
    --map-by "${map_by}"			\
    --display-map				\
    --bind-to "${bind_to}"			\
    --report-bindings				\
    "@EXECUTABLE@"				\
    -L 3					\
    "@PARFILE@"

echo "Stopping:"
date

echo "Done."
