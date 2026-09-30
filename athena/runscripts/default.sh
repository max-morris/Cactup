#! /bin/bash
# athena runscript (variant "default"): CUDA/CarpetX runs on the node's four
# A100s. Written 2026-09-29. It combines mdb/et-juphub's scheduler-less launch
# with mdb/qbd's GPU runscript (profiler wrapper, CACTUP_NSYS_UM, CACTUP_NCU_*,
# nvidia-smi bracket), with the srun launch replaced by mpirun.
# Measured facts it depends on: 4 x A100-SXM4-40GB; CUDA 13.3.1 (ncu 2026.2.1,
# nsys 2026.1.3) and HPC-X 2.24 Open MPI, both from meta.toml's env-setup,
# which cactup prepends right after the shebang (design §4.2/§6.1).
#
# Launch, and why it differs from et-juphub's:
#   - Always mpirun, even for one task. et-juphub runs a single task directly,
#     but under HPC-X a directly started MPI program (an Open MPI "singleton")
#     hung in MPI_Init on athena (2026-09-29, killed after 30 s), while
#     `mpirun -np 1` ran normally.
#   - `--bind-to none`: Open MPI would otherwise pin each rank to one core
#     and squeeze its OpenMP threads onto it.
#   - One GPU slice per rank, as srun --gpus-per-task gives on qbd: the
#     gpu-bind.sh wrapper below sets CUDA_VISIBLE_DEVICES per rank from Open
#     MPI's node-local rank (OMPI_COMM_WORLD_LOCAL_RANK), @GPUS_PER_TASK@
#     devices each. It slices whatever CUDA_VISIBLE_DEVICES the job was
#     started with (all four when unset), so `CUDA_VISIBLE_DEVICES=2,3 cactup
#     sim submit ...` keeps a run off GPUs 0 and 1 on this shared node. AMReX
#     would otherwise pick device (node-local rank % visible devices) itself;
#     the explicit slice makes every rank see exactly one device 0, the same
#     as on qbd, and keeps nsys/ncu attached to one GPU per process.
#     Verified with a 4-rank test: each rank landed on a distinct A100
#     (PCI 2F/30/AF/B0).
#   - Single node only (no hostfile, no inter-node ssh keys; see meta.toml).
#
# Variable renames vs. simfactory (design §6.3):
#   NUM_PROCS   -> @TASKS@
#   NUM_THREADS -> @CPUS_PER_TASK@
# Metadata dir SIMFACTORY/ -> .cactup/ (design §9.3).

echo "Preparing:"
set -x
set -e

cd @RUNDIR@-active

echo "Checking:"
pwd
hostname
date

if [ @NODES@ -gt 1 ]; then
    echo "athena runs on one node only (@NODES@ requested); see mdb/athena/meta.toml" >&2
    exit 1
fi

echo "Environment:"
export CACTUS_NUM_PROCS=@TASKS@
export CACTUS_NUM_THREADS=@CPUS_PER_TASK@
export GMON_OUT_PREFIX=gmon.out
export OMP_NUM_THREADS=@CPUS_PER_TASK@
# No OMP_PLACES (qbd sets it): with --bind-to none every rank's CPU mask is
# the whole node, and libgomp turns on thread binding once OMP_PLACES is set,
# so all ranks would pin their threads to the same first cores.
export OMP_STACKSIZE=8192
env | sort > .cactup/ENVIRONMENT

# Per-rank GPU slice (see the header).
cat > ./gpu-bind.sh <<'EOS'
#!/bin/bash
lr=${OMPI_COMM_WORLD_LOCAL_RANK:-0}
per=@GPUS_PER_TASK@
IFS=, read -r -a all <<< "${CUDA_VISIBLE_DEVICES:-0,1,2,3}"
sel=()
for ((i = 0; i < per; i++)); do
  sel+=("${all[$(( (lr * per + i) % ${#all[@@]} ))]}")
done
export CUDA_VISIBLE_DEVICES=$(IFS=,; echo "${sel[*]}")
exec "$@@"
EOS
chmod +x ./gpu-bind.sh

# Optional profiler wrapper (mixed-precision performance work), as on qbd.
# Set the variable in the shell that runs `cactup sim submit`; the nohup'd
# job inherits that environment. Unset: this block is inert.
#   CACTUP_PROFILE=nsys   one nsys report per task (CUDA + NVTX + MPI),
#                         profile.task<rank>.nsys-rep as on qbd.
#                         CACTUP_NSYS_UM=1 adds unified-memory page-fault
#                         tracing on both sides (CarpetX's host-side
#                         reductions read managed memory). Off by default:
#                         on qbd (2026-09-28) all four traces taken with it
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
# The rank comes from OMPI_COMM_WORLD_RANK where qbd reads SLURM_PROCID.
# Performance counters are open to users on athena, so ncu needs no admin.
# Either way the run is bracketed by `nvidia-smi -q -d CLOCK,TEMPERATURE`
# so a timing can be checked against clock throttling afterward.
PROF_WRAPPER=
case "${CACTUP_PROFILE:-}" in
  nsys)
    cat > ./prof-nsys.sh <<'EOS'
#!/bin/bash
exec nsys profile --trace=cuda,nvtx,mpi --sample=none --cpuctxsw=none --cuda-memory-usage=true \
  ${CACTUP_NSYS_UM:+--cuda-um-cpu-page-faults=true --cuda-um-gpu-page-faults=true} \
  --force-overwrite=true -o "profile.task${OMPI_COMM_WORLD_RANK:-0}" "$@@"
EOS
    chmod +x ./prof-nsys.sh
    PROF_WRAPPER=./prof-nsys.sh ;;
  ncu)
    cat > ./prof-ncu.sh <<'EOS'
#!/bin/bash
if [ "${OMPI_COMM_WORLD_RANK:-0}" = 0 ]; then
  # Kernel names are matched on the DEMANGLED symbol. ncu's default name
  # base ("function") sees only `launch_global` for every AMReX-launched
  # kernel (CarpetX has no __global__ of its own), so a regex like
  # "ParallelFor" or "prolongate" profiles nothing under the default
  # (qbd 2026-09-09: "==WARNING== No kernels were profiled"); with the
  # demangled base the template arguments (element type, centering, order)
  # are part of the name and the regex can select them. --launch-skip and
  # --launch-count then count only launches that match the regex. No regex
  # means every launch. CACTUP_NCU_METRICS (comma-separated) replaces the
  # two sections when set.
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

# HPC-X picks its transports itself (UCX for pml; vader/smcuda on the node).
# qbd's OMPI_MCA_pml=ucx / OMPI_MCA_btl=^openib are not carried over: that
# was for qbd's own Open MPI build. hcoll is switched off in env-setup.

echo "Starting:"
export CACTUS_STARTTIME=$(date +%s)

# GPU clocks and temperatures before and after, so a timing run can be
# checked for thermal or power throttling after the fact.
nvidia-smi -q -d CLOCK,TEMPERATURE > nvidia-smi.before.txt 2>&1 || true

if [ @RUNDEBUG@ -eq 0 ]; then
    time mpirun -np @TASKS@ --bind-to none \
        ./gpu-bind.sh ${PROF_WRAPPER} @EXECUTABLE@ -L 3 @PARFILE@
else
    # No terminal under nohup, so gdb runs in batch mode and prints a
    # backtrace for each rank that stops.
    time mpirun -np @TASKS@ --bind-to none \
        ./gpu-bind.sh gdb -batch -ex run -ex bt --args @EXECUTABLE@ -L 3 @PARFILE@
fi

nvidia-smi -q -d CLOCK,TEMPERATURE > nvidia-smi.after.txt 2>&1 || true

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
