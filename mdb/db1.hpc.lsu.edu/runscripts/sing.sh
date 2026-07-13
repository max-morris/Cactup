#! /bin/bash
# db1.hpc.lsu.edu runscript (variant "sing"), ported from simfactory2
# mdb/runscripts/db-sing-nv.run. Serves the Singularity build flavors (the
# sing-nv and sing-cpu queues; upstream db-sing-cpu reused db-sing-nv's
# runscript, so the launcher keeps --nv either way). It does its OWN per-rank
# `srun … singularity exec` launch (§4.8's MPI boundary), which is why the
# sing-* optionlists set coerce-run-universe = false.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).
#
# The srun branch runs the Cactus binary inside the Singularity image
# (--nv for the NVIDIA GPUs) on the compute nodes; the mpirun fallback
# (no srun on PATH, i.e. already inside the container) is kept verbatim
# from the original, including its CACTUS_CUDA_ROUND_ROBIN setting.

echo "Preparing:"
set -x                          # Output commands
set -e                          # Abort on errors

cd @RUNDIR@-active

if which srun 2>/dev/null
then module purge
fi
#module load mvapich2
#module load gcc/9.3.0

export SPACK_ROOT=/usr/cactus/spack-root
export SPACK_SYSTEM_CONFIG_PATH=/usr/cactus/.spack
export SPACK_SKIP_MODULES=1
export SPACK_ENV=$SPACK_ROOT/var/spack/environments/gpu
export PATH=$SPACK_ENV/.spack-env/view/bin:$PATH
export LD_LIBRARY_PATH=$SPACK_ENV/.spack-env/view/lib:$LD_LIBRARY_PATH

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
export TESTSUITE_NPROCS=@TASKS@
env | sort > .cactup/ENVIRONMENT

if [ $(((@TASKS@*@CPUS_PER_TASK@)%48)) != 0 -a $(((@TASKS@*@CPUS_PER_TASK@))) != 24 ]
then
    echo "Deep Bayou requires you either use half a node (24 cores),"
    echo "or multiple whole nodes (multiples of 48 cores)."
    echo "Please adjust your call to simfactory accordingly."
    exit 2
fi
if [ $((@TASKS@*@CPUS_PER_TASK@)) -le 24 ]
then
   GRES=1
else
   GRES=2
fi

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)
if which srun 2>/dev/null
then
time srun -u -A @ALLOCATION@ -p gpu \
    -N @NODES@ -n @TASKS@ \
    --cpus-per-task @CPUS_PER_TASK@ \
    --gres=gpu:$GRES \
    --gpus-per-task 1 \
    singularity exec --nv --bind /var/spool --bind /project --bind /ddnA/project --bind /etc/ssh/ssh_known_hosts --bind /ddnA/work --bind /work --bind /scratch /work/sbrandt/images/etworkshop2.simg @EXECUTABLE@ -L 3 @PARFILE@
else
# This is for the testsuite
for e in $(env | grep -i slurm | cut -d= -f1); do unset $e; done
export CACTUS_CUDA_ROUND_ROBIN=2
time mpirun -np $TESTSUITE_NPROCS @EXECUTABLE@ -L 3 @PARFILE@
fi
echo "Stopping:"
date

echo "Done."
