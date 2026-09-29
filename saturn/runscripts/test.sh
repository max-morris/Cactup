#! /bin/bash
# saturn TEST runscript (variant "test") -- marked test = true in meta.toml
# (design §11.2). Written 2026-09-29. Drives `make <config>-testsuite` and
# lets the Cactus flesh harness launch each test (design §11.6), as in
# mdb/et-juphub and mdb/qbd; the launcher is a local mpirun (no scheduler, so
# no srun as on qbd).
#
# Measured facts this depends on (2026-09-29): 3 x A100 PCIe (80/40/40 GB),
# shared with other users and with no scheduler handing out GPUs; OpenMPI
# 4.1.8 (gcc 13.2.0), whose openib BTL warns on the RoCE NIC.
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (where the
# harness output must land, under test-home) and TESTSUITE_SELECT (which tests
# to run; empty = all). env-setup is auto-prepended by cactup (design §6.1).

echo "Preparing testsuite:"
set -x
set -e

cd @SOURCEDIR@

echo "Checking:"
pwd
hostname
date

export OMP_NUM_THREADS=@CPUS_PER_TASK@
export OMP_STACKSIZE=8192
export OMPI_MCA_btl=^openib

# GPU selection, as in runscripts/default.sh: a CUDA_VISIBLE_DEVICES from the
# submitting shell wins; otherwise the first @TASKS@ x @GPUS_PER_TASK@ GPUs
# with no compute process on them. Every test rank sees the whole list and
# AMReX maps node-local rank i to device i (it does so when the rank count
# equals the visible device count; a one-rank test takes device 0). No CPU
# pinning here: OMP_PLACES stays unset so concurrent ranks cannot pile their
# threads onto the same cores.
if [ -z "${CUDA_VISIBLE_DEVICES:-}" ]; then
    NGPUS=$(( @TASKS@ * @GPUS_PER_TASK@ ))
    busy=$(nvidia-smi --query-compute-apps=gpu_uuid --format=csv,noheader 2>/dev/null | sort -u)
    CUDA_VISIBLE_DEVICES=$(nvidia-smi --query-gpu=index,uuid --format=csv,noheader |
        while IFS=', ' read -r idx uuid; do
            echo "${busy}" | grep -qx "${uuid}" || echo "${idx}"
        done | head -n "${NGPUS}" | paste -sd, -)
    if [ "$(echo "${CUDA_VISIBLE_DEVICES}" | tr , '\n' | grep -c .)" -lt "${NGPUS}" ]; then
        echo "WARNING: fewer than ${NGPUS} idle GPUs; sharing busy ones" >&2
        CUDA_VISIBLE_DEVICES=$(seq -s, 0 $(( NGPUS - 1 )))
    fi
fi
export CUDA_VISIBLE_DEVICES
echo "GPUs for this testsuite: ${CUDA_VISIBLE_DEVICES}"

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='mpirun -np $nprocs --bind-to none $exe $parfile'

# Redirect testsuite output into test-home (design §11.6): the flesh harness
# honors TESTS_DIR and writes each test's run dirs plus summary.log under
# $TESTS_DIR/@CONFIGURATION@/, keeping the source tree clean.
mkdir -p @TESTSUITE_RESULTS_DIR@
export TESTS_DIR=@TESTSUITE_RESULTS_DIR@

echo "Running testsuite (selection: '@TESTSUITE_SELECT@', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

export CCTK_TESTSUITE_RUN_TESTS="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
