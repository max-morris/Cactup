#! /bin/bash
# saturn runscript (variant "default"): CUDA/CarpetX runs on Frank's saturn
# node. Written 2026-09-29. Launch logic from mdb/et-juphub (no scheduler:
# direct exec for one task, a local `mpirun -np @TASKS@` otherwise); the
# profiler block and the nvidia-smi bracket are carried over from
# mdb/qbd/runscripts/default.sh so the GPU test kit works unchanged.
#
# Measured facts this depends on (2026-09-29):
#   - 3 x A100 PCIe: GPU0 80 GB on NUMA 0 (cores 0-25), GPU1 40 GB on NUMA 1
#     (cores 26-51), GPU2 40 GB on NUMA 3 (cores 78-103); no NVLink.
#   - OpenMPI 4.1.8 (gcc 13.2.0): mpirun exports OMPI_COMM_WORLD_RANK and
#     OMPI_COMM_WORLD_LOCAL_RANK to each rank; it is NOT CUDA-aware, and its
#     openib BTL prints a "no CPCs for port" warning on the RoCE NIC, hence
#     OMPI_MCA_btl=^openib (shared memory within the node is all a run uses).
#   - The node is shared and has no scheduler to hand out GPUs, so this script
#     does what srun --gpus-per-task does on qbd: see "GPU selection" below.
#   - ncu/nsys come from CUDA 13.2.1 (env-setup puts them on PATH); counters
#     are open to users (RmProfilingAdminOnly: 0).
#
# Variable renames vs simfactory (design §6.3): NUM_PROCS -> @TASKS@,
# NUM_THREADS -> @CPUS_PER_TASK@; metadata dir SIMFACTORY/ -> .cactup/ (§9.3).
# env-setup is auto-prepended by cactup (design §6.1).

echo "Preparing:"
set -x
set -e

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
export OMP_PLACES=cores
export OMP_STACKSIZE=8192
export OMPI_MCA_btl=^openib

# GPU selection. With no scheduler, nothing reserves devices for this run, so:
#   - CUDA_VISIBLE_DEVICES set in the shell that ran `cactup sim submit`
#     (nohup passes the environment through) is taken as the list to use;
#   - otherwise the first @TASKS@ x @GPUS_PER_TASK@ GPUs that have no compute
#     process on them right now are chosen, in index order; if too few are
#     idle, the lowest-numbered GPUs are used anyway, with a warning.
# CACTUP_GPU_LIST is then sliced per rank by ./gpu-bind.sh (below), so every
# rank sees exactly its own @GPUS_PER_TASK@ device(s) and AMReX's device
# choice (device 0 of what it sees) is unambiguous.
NGPUS=$(( @TASKS@ * @GPUS_PER_TASK@ ))
if [ -n "${CUDA_VISIBLE_DEVICES:-}" ]; then
    CACTUP_GPU_LIST=${CUDA_VISIBLE_DEVICES}
else
    busy=$(nvidia-smi --query-compute-apps=gpu_uuid --format=csv,noheader 2>/dev/null | sort -u)
    CACTUP_GPU_LIST=$(nvidia-smi --query-gpu=index,uuid --format=csv,noheader |
        while IFS=', ' read -r idx uuid; do
            echo "${busy}" | grep -qx "${uuid}" || echo "${idx}"
        done | head -n "${NGPUS}" | paste -sd, -)
    if [ "$(echo "${CACTUP_GPU_LIST}" | tr , '\n' | grep -c .)" -lt "${NGPUS}" ]; then
        echo "WARNING: fewer than ${NGPUS} idle GPUs; sharing busy ones" >&2
        CACTUP_GPU_LIST=$(seq -s, 0 $(( NGPUS - 1 )))
    fi
fi
export CACTUP_GPU_LIST
export CACTUP_GPUS_PER_TASK=@GPUS_PER_TASK@
unset CUDA_VISIBLE_DEVICES
echo "GPUs for this run: ${CACTUP_GPU_LIST}"
nvidia-smi --query-gpu=index,name,memory.used,utilization.gpu --format=csv > nvidia-smi.gpus.txt 2>&1 || true

# Per-rank binding: rank r (node-local, 0 for a direct exec) gets devices
# [r*GPUS_PER_TASK, (r+1)*GPUS_PER_TASK) of CACTUP_GPU_LIST and is pinned
# (taskset) to the cores of its first GPU's NUMA node, read from sysfs. That
# keeps OMP_PLACES=cores from putting every rank's threads on cores 0..N-1 of
# the whole machine, which is what it would do under `mpirun --bind-to none`.
cat > ./gpu-bind.sh <<'EOS'
#!/bin/bash
n=${CACTUP_GPUS_PER_TASK:-1}
r=${OMPI_COMM_WORLD_LOCAL_RANK:-0}
export CUDA_VISIBLE_DEVICES=$(echo "${CACTUP_GPU_LIST}" | tr , '\n' |
    sed -n "$(( r * n + 1 )),$(( r * n + n ))p" | paste -sd, -)
bus=$(nvidia-smi --query-gpu=pci.bus_id --format=csv,noheader -i "${CUDA_VISIBLE_DEVICES%%,*}" 2>/dev/null)
cpus=$(cat "/sys/bus/pci/devices/$(echo "${bus:4}" | tr A-F a-f)/local_cpulist" 2>/dev/null)
if [ -n "${cpus}" ]; then
  exec taskset -c "${cpus}" "$@@"
fi
exec "$@@"
EOS
chmod +x ./gpu-bind.sh

env | sort > .cactup/ENVIRONMENT

# Optional profiler wrapper (mixed-precision performance work), as on qbd. Set
# the variable in the shell that runs `cactup sim submit`; the nohup'd job
# inherits the environment. Unset: this block is inert.
#   CACTUP_PROFILE=nsys   one nsys report per task (CUDA + NVTX + MPI).
#                         CACTUP_NSYS_UM=1 adds unified-memory page-fault
#                         tracing on both sides (CarpetX's host-side
#                         reductions read managed memory). Off by default:
#                         on 2026-09-28 (qbd) all four traces taken with it
#                         on ended in "Errors occurred while processing the
#                         raw events" (an event-order check in nsys), and
#                         three of the four kept only the first two
#                         iterations.
#   CACTUP_PROFILE=ncu    ncu on task 0 only: CACTUP_NCU_COUNT launches
#                         (default 60) after skipping CACTUP_NCU_SKIP
#                         (default 300), counted among launches whose
#                         DEMANGLED name matches CACTUP_NCU_KERNELS (a
#                         regex; unset = every launch). Sections
#                         SpeedOfLight + MemoryWorkloadAnalysis, or the
#                         comma-separated CACTUP_NCU_METRICS instead when
#                         that is set (metrics replace the sections).
# Either way the run is bracketed by `nvidia-smi -q -d CLOCK,TEMPERATURE`
# so a timing can be checked against clock throttling afterward.
# The rank comes from OpenMPI (OMPI_COMM_WORLD_RANK) instead of qbd's
# SLURM_PROCID; a one-task run is a direct exec with no rank variable, so its
# nsys report is named profile.task0 explicitly.
if [ @TASKS@ = 1 ]; then NSYS_TASK=0; else NSYS_TASK='%q{OMPI_COMM_WORLD_RANK}'; fi
PROF_WRAPPER=
case "${CACTUP_PROFILE:-}" in
  nsys)
    PROF_WRAPPER="nsys profile --trace=cuda,nvtx,mpi --sample=none --cpuctxsw=none --cuda-memory-usage=true ${CACTUP_NSYS_UM:+--cuda-um-cpu-page-faults=true --cuda-um-gpu-page-faults=true} --force-overwrite=true -o profile.task${NSYS_TASK}" ;;
  ncu)
    cat > ./prof-ncu.sh <<'EOS'
#!/bin/bash
if [ "${OMPI_COMM_WORLD_RANK:-0}" = 0 ]; then
  # Kernel names are matched on the DEMANGLED symbol. ncu's default name
  # base ("function") sees only `launch_global` for every AMReX-launched
  # kernel (CarpetX has no __global__ of its own), so a regex like
  # "ParallelFor" or "prolongate" profiles nothing under the default
  # (2026-09-09: "==WARNING== No kernels were profiled"); with the demangled
  # base the template arguments (element type, centering, order) are part
  # of the name and the regex can select them. --launch-skip/--launch-count
  # then count only launches that match the regex. No regex = every launch.
  # CACTUP_NCU_METRICS (comma-separated) replaces the two sections when set.
  # CACTUP_NCU_NVTX (regex) restricts profiling to launches inside a matching
  # NVTX range, e.g. ODESolvers::Solve: without it --launch-skip has to count
  # past the initial-data prolongations (18k on a 2lev run) to reach the
  # evolution, and a fixed skip lands wherever it lands (2026-09-29, athena).
  if [ -n "${CACTUP_NCU_METRICS:-}" ]; then
    NCU_WHAT="--metrics ${CACTUP_NCU_METRICS}"
  else
    NCU_WHAT="--section SpeedOfLight --section MemoryWorkloadAnalysis"
  fi
  exec ncu --target-processes all --kernel-name-base demangled \
    --launch-skip "${CACTUP_NCU_SKIP:-300}" --launch-count "${CACTUP_NCU_COUNT:-60}" \
    ${CACTUP_NCU_KERNELS:+--kernel-name "regex:${CACTUP_NCU_KERNELS}"} \
    ${CACTUP_NCU_NVTX:+--nvtx --nvtx-include "regex:${CACTUP_NCU_NVTX}"} \
    ${NCU_WHAT} \
    --force-overwrite -o profile.ncu "$@@"
else
  exec "$@@"
fi
EOS
    chmod +x ./prof-ncu.sh
    PROF_WRAPPER=./prof-ncu.sh ;;
esac

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

# GPU clocks and temperatures before and after, for this run's GPUs only, so
# a timing run can be checked for thermal or power throttling after the fact.
nvidia-smi -q -d CLOCK,TEMPERATURE -i "${CACTUP_GPU_LIST}" > nvidia-smi.before.txt 2>&1 || true

# --bind-to none: gpu-bind.sh does the pinning (to each GPU's NUMA node),
# which OpenMPI's own mapping cannot express.
if [ @TASKS@ = 1 ]; then
    if [ @RUNDEBUG@ -eq 0 ]; then
        time ./gpu-bind.sh ${PROF_WRAPPER} @EXECUTABLE@ -L 3 @PARFILE@
    else
        ./gpu-bind.sh gdb --args @EXECUTABLE@ -L 3 @PARFILE@
    fi
else
    time mpirun -np @TASKS@ --bind-to none \
        ./gpu-bind.sh ${PROF_WRAPPER} @EXECUTABLE@ -L 3 @PARFILE@
fi

nvidia-smi -q -d CLOCK,TEMPERATURE -i "${CACTUP_GPU_LIST}" > nvidia-smi.after.txt 2>&1 || true

# Text summaries next to the binary reports, so a results bundle can be read
# without the profiler installed: per-kernel, per-API-call and memory-transfer
# totals (CSV) for each nsys report, and the ncu sections as text.
case "${CACTUP_PROFILE:-}" in
  nsys) for rep in profile.task*.nsys-rep; do
          [ -f "$rep" ] || continue
          nsys stats --report cuda_gpu_kern_sum,cuda_api_sum,cuda_gpu_mem_time_sum,cuda_gpu_mem_size_sum,nvtx_sum,nvtx_kern_sum \
               --format csv --force-export=true -o "${rep%.nsys-rep}" "$rep" >/dev/null 2>&1 || echo "nsys stats failed for $rep"
        done ;;
  ncu)  [ -f profile.ncu.ncu-rep ] && { ncu --import profile.ncu.ncu-rep --page details > prof-ncu-details.txt 2>&1
                                       ncu --import profile.ncu.ncu-rep --page raw --csv > prof-ncu-raw.csv 2>/dev/null; } ;;
esac

echo "Stopping:"
date
echo "Done."
