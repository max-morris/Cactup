#! /bin/bash
# qbc.loni.org runscript (variant "default"), ported from simfactory2
# mdb/runscripts/mike.run.
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).
#
# The srun branch runs the Cactus binary inside the Singularity image on the
# compute nodes; the mpirun fallback (no srun on PATH) is kept verbatim from
# the original, including its halved thread counts.

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
export SPACK_ENV=$SPACK_ROOT/var/spack/environments/cpu
export PATH=$SPACK_ENV/.spack-env/view/bin:$PATH
export LD_LIBRARY_PATH=$SPACK_ENV/.spack-env/view/libn:$LD_LIBRARY_PATH

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

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)
if which srun 2>/dev/null
then
time srun -u -A @ALLOCATION@ -p checkpt \
    -N @NODES@ -n @TASKS@ \
    --cpus-per-task @CPUS_PER_TASK@ \
    singularity exec --bind /var/spool --bind /project --bind /ddnA/project --bind /etc/ssh/ssh_known_hosts --bind /ddnA/work --bind /work --bind /scratch /work/sbrandt/images/etworkshop-cpu.simg @EXECUTABLE@ -L 3 @PARFILE@
else
for e in $(env | grep -i slurm | cut -d= -f1); do unset $e; done
export OMP_NUM_THREADS=$(( @CPUS_PER_TASK@ / 2))
export CACTUS_NUM_THREADS=$(( @CPUS_PER_TASK@ / 2))
time mpirun -np $TESTSUITE_NPROCS @EXECUTABLE@ -L 3 @PARFILE@
fi
echo "Stopping:"
date

echo "Done."
