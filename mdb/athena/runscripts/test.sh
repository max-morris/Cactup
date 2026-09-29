#! /bin/bash
# athena TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2). Written 2026-09-29. Instead of launching a parfile like
# runscripts/default.sh, it drives `make <config>-testsuite` and lets the
# Cactus flesh harness launch each test (design §11.6). Serves both queues
# (the CUDA build on `local`, the CPU build on `cpu`).
#
# The launcher matches runscripts/default.sh (see its header for the facts):
# always mpirun (a directly started HPC-X program hung in MPI_Init on
# athena), --bind-to none, and the same per-rank GPU slice. The flesh's own
# default ('mpirun -np $nprocs $exe $parfile') would work but lets Open MPI
# bind each rank to a single core, so the command is set explicitly. The
# gpu-bind.sh wrapper is harmless for a CPU build.
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (where the
# harness output must land, under test-home) and TESTSUITE_SELECT (which tests
# to run; empty = all). env-setup is auto-prepended by cactup for .sh variants
# (design §6.1).

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

# Redirect testsuite output into test-home (design §11.6): the flesh harness
# honors TESTS_DIR and writes each test's run dirs plus summary.log under
# $TESTS_DIR/@CONFIGURATION@/, keeping the source tree clean.
mkdir -p @TESTSUITE_RESULTS_DIR@
export TESTS_DIR=@TESTSUITE_RESULTS_DIR@

# Per-rank GPU slice, as in runscripts/default.sh.
cat > @TESTSUITE_RESULTS_DIR@/gpu-bind.sh <<'EOS'
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
chmod +x @TESTSUITE_RESULTS_DIR@/gpu-bind.sh

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands.
export CCTK_TESTSUITE_RUN_PROCESSORS=@TASKS@
export CCTK_TESTSUITE_RUN_COMMAND='mpirun -np $nprocs --bind-to none @TESTSUITE_RESULTS_DIR@/gpu-bind.sh $exe $parfile'

echo "Running testsuite (selection: '@TESTSUITE_SELECT@', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

export CCTK_TESTSUITE_RUN_TESTS="@TESTSUITE_SELECT@"
make @CONFIGURATION@-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
