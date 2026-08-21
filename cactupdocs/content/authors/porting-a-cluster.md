+++
title = "Porting a Cluster"
description = "Complete walkthrough: add your HPC cluster to cactup"
+++

# Porting a Cluster to cactup

This is a complete, hands-on guide to adding your HPC cluster to cactup. We'll walk through creating a machine definition step-by-step using a concrete example.

## Prerequisites

- cactup installed on your cluster
- Access to the cluster's build system (compilers, MPI, modules, etc.)
- Administrator or power-user knowledge of your batch scheduler (SLURM, PBS, etc.)
- A test Einstein Toolkit installation on the cluster (for validation)

## Step 1: Create the machine definition

Start with an existing machine as a template. If your cluster is similar to `mike2.hpc.lsu.edu`, use it as the base:

```sh
cactup machine create myclu --from-existing mike2 --silent
```

If you're starting from scratch, use the generic machine:

```sh
cactup machine create myclu --from-existing generic --silent
```

Or let cactup auto-detect your current machine and use it as a base:

```sh
cactup machine create myclu --from-existing --silent
```

This creates `~/.cactup/machines/myclu/` with:

```
~/.cactup/machines/myclu/
  meta.toml
  optionlists/
    default.toml
  submitscripts/
    default.sh
    test.sh
  runscripts/
    default.sh
    test.sh
  discover.py
```

There's no `buildsubmitscripts/` directory yet — that one's optional, and only
needed if your cluster forbids compiling on the login node. Step 4 below
covers when and how to add it.

## Step 2: Edit meta.toml

Open `~/.cactup/machines/myclu/meta.toml` and update the machine identity:

```toml
[machine]
name = "My Cluster"
nickname = "myclu"
location = "Your Institution"
description = "Describe your cluster briefly"
status = "personal"
hostname = "login.myclu.edu"
```

### Paths section

Set the home directories for installations, simulations, and tests:

```toml
[paths]
install-home = "/home/@USER@"
simulation-home = "/scratch/@USER@/simulations"
test-home = "/scratch/@USER@/tests"
```

Replace paths to match your cluster's filesystem layout.

### Hardware section

Specify your cluster's hardware:

```toml
[hardware]
max-cpus-per-node = 128           # CPUs per compute node
memory = 262144                    # RAM per node in MB
```

Check your cluster's specs:

```sh
# SLURM example
sinfo -o "%n %c %m" | head -1  # CPUs and memory per node
```

### Build section

Set the default parallel make jobs:

```toml
[build]
make = "make -j@MAKEJOBS@"
make-jobs = 128
```

### Scheduler section

This is the most cluster-specific part. Configure commands for your batch system.

#### SLURM example:

```toml
[scheduler]
submit = "sbatch @SCRIPTFILE@"
allocation-env = "SLURM_JOB_ID"
get-status = "squeue -j @JOB_ID@"
stop = "scancel @JOB_ID@"
submit-pattern = "Submitted batch job ([0-9]+)"
status-pattern = "@JOB_ID@ "
queued-pattern = " PD "
running-pattern = " R "
holding-pattern = '\(JobHeldUser\)'
exec-host = "hostname -s"
exec-host-pattern = '(\S+)'
max-walltime = "24:00:00"
```

> **Note:** `allocation` is **not** a `[scheduler]` key — the account to charge
> is a per-user *knob* (`cactup knob allocation my_project`), not part of the
> machine definition. Any unknown key here is silently ignored.

Test each pattern against real scheduler output:

```sh
# Test submit-pattern
sbatch test.sh
# Output: "Submitted batch job 12345"
# Pattern should extract: 12345

# Test status-pattern
squeue -j 12345
# Output: "12345 default myuser  R   0:10   1 node001"
# Pattern "@JOB_ID@ " should match "12345 "

# Test running-pattern
echo " R   0:10"
# Pattern " R " should match
```

#### PBS/Torque example:

```toml
[scheduler]
submit = "qsub @SCRIPTFILE@"
allocation-env = "PBS_JOBID"
get-status = "qstat @JOB_ID@"
stop = "qdel @JOB_ID@"
submit-pattern = "([0-9.]+)"
status-pattern = "@JOB_ID@"
queued-pattern = " Q "
running-pattern = " R "
holding-pattern = " H "
exec-host = "hostname -s"
exec-host-pattern = '(\S+)'
max-walltime = "24:00:00"
```

### Environment section

Set up the build environment (modules, paths, etc.):

```toml
[environment]
env-setup = """
module load gcc
module load openmpi
export PATH="/opt/bin:$PATH"
export LD_LIBRARY_PATH="/opt/lib:${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
"""
```

### Queues section

Define the scheduler queues/partitions available on your cluster:

```toml
[queues.default]
gpu = false
default = true
max-walltime = "24:00:00"

[queues.gpu]
gpu = true
max-walltime = "12:00:00"

[queues.long]
gpu = false
max-walltime = "72:00:00"
```

Get queue info:

```sh
# SLURM example
sinfo -o "%P %l %D" | head -5
```

### Variants section

Declare available variants:

```toml
[variants.optionlist]
variants = ["default", "cuda"]

[variants.submitscript]
"default" = { queues = ["default", "gpu"], default = true }
"test" = { queues = ["default"], test = true }

[variants.runscript]
"default" = { queues = ["default", "gpu"], default = true }
"test" = { queues = ["default"], test = true }
```

## Step 3: Create optionlists

An optionlist is a `[cactup]` header (cactup-only metadata) plus an `[options]`
table of raw Cactus `NAME = value` pairs — the same names you'd put in a
hand-written Einstein Toolkit `.cfg` optionlist. Values are strings, booleans, or
integers (floats are rejected); `VERSION` is required and is always emitted
first. See [Optionlists](optionlists.html) for the full reference.

Edit `optionlists/default.toml` with your cluster's compilers and flags:

```toml
[cactup]
compatible-queues = ["default", "long"]
default = true
description = "Default build on myclu"

[options]
VERSION = "2024-06-01"

CPP = "cpp"
CC  = "gcc"
CXX = "g++"
FPP = "cpp"
F90 = "gfortran"

CFLAGS   = "-g -std=gnu99"
CXXFLAGS = "-g -std=gnu++17"
F90FLAGS = "-g -fcray-pointer -ffixed-line-length-none"

OPTIMISE           = "yes"
C_OPTIMISE_FLAGS   = "-O2 -march=native"
CXX_OPTIMISE_FLAGS = "-O2 -march=native"
F90_OPTIMISE_FLAGS = "-O2 -march=native"

OPENMP = "yes"
MPI    = "MPICH"
```

If your cluster has CUDA, create `optionlists/cuda.toml` and mark it `gpu = true`
so it is only offered on GPU queues:

```toml
[cactup]
gpu = true
compatible-queues = ["gpu"]
description = "CUDA GPU build on myclu"

[options]
VERSION = "2024-06-01"

CC  = "gcc"
CXX = "g++"
F90 = "gfortran"

CUCC      = "nvcc"
CUCCFLAGS = "-std=c++17 -arch=sm_80"

OPTIMISE = "yes"
OPENMP   = "yes"
MPI      = "MPICH"
```

## Step 4: Update submit scripts

Edit `submitscripts/default.sh`. Here's a complete SLURM example:

```bash
#!/bin/bash

#SBATCH --job-name=@JOB_NAME@
#SBATCH --nodes=@NODES@
#SBATCH --ntasks=@TASKS@
#SBATCH --ntasks-per-node=@TASKS_PER_NODE@
#SBATCH --cpus-per-task=@CPUS_PER_TASK@
#SBATCH --time=@WALLTIME@
#SBATCH --output=@STDOUT_FILE@
#SBATCH --error=@STDERR_FILE@
#SBATCH --partition=@QUEUE@

# Source environment
@ENV_SETUP@

# Set up checkpoint walltime
export CHECKPOINT_WALLTIME=@CHECKPOINT_WALLTIME@

cd @RUNDIR@-active

srun @EXECUTABLE@ @PARFILE@
```

If your cluster uses modules or special configurations, adjust accordingly.

For GPU jobs, you may need:

```bash
#SBATCH --gres=gpu:1                     # no per-task GPU-count token; hard-code it
#SBATCH --cpus-per-task=@CPUS_PER_TASK@
```

Create `submitscripts/test.sh` for test submissions (smaller, faster):

```bash
#!/bin/bash

#SBATCH --job-name=test-@CONFIGURATION@
#SBATCH --nodes=1
#SBATCH --ntasks=2
#SBATCH --time=00:30:00
#SBATCH --output=@STDOUT_FILE@
#SBATCH --error=@STDERR_FILE@
#SBATCH --partition=@QUEUE@

@ENV_SETUP@

cd @RUNDIR@-active

srun @EXECUTABLE@ @PARFILE@
```

### Build submit scripts: does your cluster need one?

Most clusters let you compile on the login node, and don't need anything
here — skip this subsection entirely. Some clusters (often GPU clusters with
a strict login-node policy) forbid it, the same way they'd forbid running a
simulation there. If yours is one of them, add a fourth script directory
alongside the three above:

```
~/.cactup/machines/myclu/
  buildsubmitscripts/
    default.sh
```

It's declared in meta.toml exactly like `submitscript`/`runscript`, as its
own variant table:

```toml
[variants.buildsubmitscript]
"default" = { queues = ["default", "gpu"], default = true }
```

And in `[build]`, tell cactup an unqualified `cactup build` should go to the
queue rather than trying (and failing) to compile on the login node:

```toml
[build]
default-action = "submit"
queue          = "default"    # the build job's own queue/walltime/shape —
walltime       = "2:00:00"    # all optional, and independent of a run's
nodes          = 1
tasks          = 1
cpus-per-task  = 32
```

The script itself is structurally identical to `submitscripts/default.sh` —
same `#SBATCH` directives, same `@ENV_SETUP@` — except the final line
re-invokes cactup to build instead of to run:

```bash
#!/bin/bash

#SBATCH --job-name=@JOB_NAME@
#SBATCH --nodes=@NODES@
#SBATCH --ntasks=@TASKS@
#SBATCH --cpus-per-task=@CPUS_PER_TASK@
#SBATCH --time=@WALLTIME@
#SBATCH --output=@STDOUT_FILE@
#SBATCH --error=@STDERR_FILE@
#SBATCH --partition=@QUEUE@

@ENV_SETUP@

cd @SOURCEDIR@

exec @CACTUP@ build run @CONFIGURATION@ \
    --installation=@ALIAS@ --config-dir=@CONFIG_DIR@ --machine=@MACHINE@ \
    --attempt-id=@ATTEMPT_ID@
```

`@CONFIG_DIR@` and `@ATTEMPT_ID@` are the build analogues of the run
submitscript's `@SIMULATION_DIR@`/`@RESTART_ID@` (see
[Scripts & Variables](scripts-and-variables.html)): they let the compute node
locate exactly which build attempt to run without touching cactup's global
state on this machine at all. Don't hand-write these two — they come from
cactup itself when it generates the script, not from anything you configure.

If your cluster needs the same GPU-reservation logic your run submitscript
has (see the qbd example in [Scripts & Variables](scripts-and-variables.html)),
write `buildsubmitscripts/default.py` instead of `.sh` — the calling
convention is identical to a `submitscript` `.py` variant.

## Step 5: Update run scripts

Edit `runscripts/default.sh`:

```bash
#!/bin/bash

set -e

@ENV_SETUP@

cd @RUNDIR@-active

if [ @RUNDEBUG@ -eq 0 ]; then
    @EXECUTABLE@ @PARFILE@
else
    @DEBUGGER@ --args @EXECUTABLE@ @PARFILE@   # launched with --debug
fi
```

Edit `runscripts/test.sh` for interactive test runs:

```bash
#!/bin/bash

set -e

@ENV_SETUP@

cd @RUNDIR@-active

@EXECUTABLE@ @PARFILE@
```

## Step 6: Write discover.py

Create `discover.py` to auto-detect your machine:

```python
def is_machine(hostname):
    """Return True if this host is on myclu."""
    return hostname.startswith("login") and hostname.endswith(".myclu.edu")
```

Or use a more robust check:

```python
import os

def is_machine(hostname):
    # Check hostname pattern AND filesystem marker
    if not hostname.startswith("compute"):
        return False
    return os.path.exists("/etc/myclu-marker")
```

## Step 7: Test the machine definition

Verify your machine is recognized:

```sh
cactup machine show myclu
```

This should show:

```
Machine: My Cluster
Nickname: myclu
CPUs per node: 128
Simulation home: /scratch/username/simulations
Test home: /scratch/username/tests
Queues: default (gpu=no), gpu (gpu=yes), long (gpu=no)
```

Test machine discovery:

```sh
cactup machine forget       # Clear cache
cactup machine show         # Should auto-detect myclu
```

## Step 8: Test with a build and small simulation

Install a release on your cluster:

```sh
cactup install ET_2025_05 --silent
```

Build a test config:

```sh
cactup build testconfig --variant default
```

Run the test suite:

```sh
cactup test run --config testconfig -n 1
```

If tests pass, try a small simulation:

```sh
cactup sim run testsim testsim.par --config testconfig -n 1
```

## Step 9: Test batch submission (if applicable)

If you added `buildsubmitscripts/` in Step 4, submit a build to the queue
first — it exercises the buildsubmitscript, the job-id parsing, and the
compute-node re-invocation before you've committed to a full run:

```sh
cactup build submit testconfig2 --follow
cactup build show testconfig2
```

`--follow` streams `make`'s output until the job finishes, so you'll see a
compile error immediately rather than having to go looking for it.

Submit a test to the queue:

```sh
cactup test submit --config testconfig -n 1 -w 1:00:00
```

Monitor:

```sh
cactup test show              # Status
cactup test log --follow      # Output
```

Submit a small simulation:

```sh
cactup sim submit testsim testsim.par -n 2 -w 1:00:00
```

## Step 10: Validate common operations

Test the key workflows:

```sh
# Build with different options
cactup build config1 --optimize
cactup build config2 --debug
cactup build config3 --variant cuda

# Run simulations
cactup sim submit mysim mysim.par -n 4 -w 2:00:00 -q default
cactup sim submit mysim mysim.par -n 2 -w 1:00:00 -q gpu --gpu
cactup sim show mysim --long
cactup sim log mysim --follow

# Monitor
cactup sim list --long
cactup sim delete mysim
```

## Troubleshooting

### Machine not detected

Check discover.py logic:

```python
# Test your function
def is_machine(hostname):
    return hostname == "login.myclu.edu"

# At the Python prompt:
import socket
hostname = socket.getfqdn()
print(f"Current FQDN: {hostname}")
print(f"is_machine result: {is_machine(hostname)}")
```

Use `cactup --machine myclu` to force selection while debugging.

### Build fails

Check the environment setup:

```sh
cactup --trace build testconfig
```

This prints all commands. Look for missing modules or compiler errors.

### Scheduler commands don't work

Test patterns manually:

```sh
sbatch test.sh > /tmp/out.txt
cat /tmp/out.txt
# Compare output to submit-pattern in meta.toml
```

### Submit scripts fail

Check the generated script:

```sh
cactup --trace sim submit mysim mysim.par -n 1 -w 1:00:00
```

This shows the full script before submission. Run it manually to debug.

## Distributing your machine

Once your machine is working:

1. **Share with your team**: Copy `~/.cactup/machines/myclu/` to colleagues
2. **Merge to system MDB**: Submit your machine definition to the cactup project (GitHub)
3. **Document**: Add a README or notes about your cluster setup

## Next steps

- [Machine Discovery](machine-discovery.html) — refine your discover.py
- [meta.toml Reference](meta-toml.html) — advanced configuration options
- [Scripts & Variables](scripts-and-variables.html) — customize submit/run scripts for special cases
