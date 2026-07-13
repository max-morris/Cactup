# frontera TEST runscript (variant "test") — marked test = true in meta.toml
# (design §11.2); .py form of the earlier test.sh. Instead of launching a
# parfile like runscripts/default.py, it drives `make <config>-testsuite` and
# lets the Cactus flesh harness launch each test (design §11.6). The launcher
# mirrors default.py's ibrun (per-test task count via $nprocs).
#
# Testsuite-only variables (design §11.9): TESTSUITE_RESULTS_DIR (where the
# harness output must land, under test-home) and TESTSUITE_SELECT (which
# tests to run; empty = all). env-setup is NOT auto-prepended for .py
# variants (design §6.1); it is emitted right after the shebang.

script = """#! /bin/bash
{env_setup}

echo "Preparing testsuite:"
set -x                          # Output commands
set -e                          # Abort on errors

cd {sourcedir}

module list

echo "Checking:"
pwd
hostname
date

# Same environment block as runscripts/default.py.
export CACTUS_SET_THREAD_BINDINGS=1
export CXX_MAX_TASKS=500
export GMON_OUT_PREFIX=gmon.out
export OMP_MAX_TASKS=500
export OMP_NUM_THREADS={cpus_per_task}
export OMP_STACKSIZE=8192       # kByte
export PTHREAD_MAX_TASKS=500

# The flesh testsuite harness substitutes the literal placeholders
# $nprocs/$exe/$parfile into this command (design §11.6); single quotes keep
# them out of the shell's hands. The launcher mirrors runscripts/default.py.
export CCTK_TESTSUITE_RUN_PROCESSORS={tasks}
export CCTK_TESTSUITE_RUN_COMMAND='ibrun -n $nprocs $exe $parfile'

# Redirect testsuite output into test-home (design §11.6): the flesh harness
# honors TESTS_DIR and writes each test's run dirs plus summary.log under
# {configuration}/ inside it, keeping the source tree clean.
mkdir -p {results_dir}
export TESTS_DIR={results_dir}

echo "Running testsuite (selection: '{select}', empty = all):"
export CACTUS_STARTTIME=$(date +%s)

export CCTK_TESTSUITE_RUN_TESTS="{select}"
make {configuration}-testsuite PROMPT=no

echo "Stopping:"
date
echo "Done."
""".format(
    env_setup=ENV_SETUP,
    sourcedir=SOURCEDIR,
    cpus_per_task=CPUS_PER_TASK,
    tasks=TASKS,
    results_dir=TESTSUITE_RESULTS_DIR,
    select=TESTSUITE_SELECT,
    configuration=CONFIGURATION,
)

print(script)
