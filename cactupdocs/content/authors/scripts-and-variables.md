+++
title = "Scripts & Variables"
description = "Write submit and run scripts with template variables"
+++

# Submit and Run Scripts

**Submit scripts** generate batch job submissions (e.g., SLURM `sbatch` files). **Run scripts** generate interactive simulations or test runs. **Build submit scripts** are a third, optional kind, for clusters that require compiling on a compute node rather than the login node. All three are templates that expand `@VAR@` tokens at runtime.

## Scripts directory structure

Scripts are stored alongside meta.toml:

```
<mdb>/machines/<machine>/
  submitscripts/
    default.sh          # Submit variant for queued jobs
    test.sh             # Special variant used by `cactup test submit`
  runscripts/
    default.sh          # Run variant for interactive execution
    test.sh             # Special variant used by `cactup test run`
  buildsubmitscripts/    # OPTIONAL — only if the cluster forbids
    default.sh           # login-node compiling; used by `cactup build submit`
```

Each script is typically 50-200 lines. cactup expands variables, then executes the script. `buildsubmitscripts/` is the one directory of the three that can be entirely absent: most clusters let you compile on the login node and never need it. See [Porting a Cluster](porting-a-cluster.html) for a worked example, and [meta.toml Reference](meta-toml.html) for the `[build]` keys that control when it's used.

## Script types

### Submit scripts (for queued jobs)

A submit script is passed to the scheduler (e.g., `sbatch @SCRIPTFILE@`). It must:

1. **Set scheduler directives** at the top (SLURM `#SBATCH`, PBS `#PBS`, etc.)
2. **Print the job ID** (parsed by `submit-pattern` in meta.toml)
3. **Call cactus_<config>** to run the simulation

Example SLURM submit script:

```bash
#!/bin/bash

#SBATCH --job-name=@JOB_NAME@
#SBATCH --nodes=@NODES@
#SBATCH --ntasks=@TASKS@
#SBATCH --ntasks-per-node=@TASKS_PER_NODE@
#SBATCH --cpus-per-task=@CPUS_PER_TASK@
#SBATCH --time=@WALLTIME@
#SBATCH --output=@SIMULATION_NAME@.out
#SBATCH --error=@SIMULATION_NAME@.err
#SBATCH --partition=@QUEUE@

# Source environment
@ENV_SETUP@

# Run the simulation
cd @RUNDIR@-active
srun @EXECUTABLE@ @PARFILE@
```

### Run scripts (for interactive execution)

A run script is executed directly (not submitted to a queue). It sets up the environment and runs cactus directly. Example:

```bash
#!/bin/bash

set -e
@ENV_SETUP@

cd @RUNDIR@-active

# @RUNDEBUG@ is 1 when the run was launched with --debug, 0 otherwise;
# @DEBUGGER@ is the debugger launch command (e.g. `gdb`).
if [ @RUNDEBUG@ -eq 0 ]; then
    @EXECUTABLE@ @PARFILE@
else
    @DEBUGGER@ --args @EXECUTABLE@ @PARFILE@
fi
```

### Build submit scripts (for queue-submitted builds)

A build submit script is the third kind, structurally a sibling of a
(simulation) submit script: same scheduler-directive header, same
`submit-pattern` job-id parsing. The difference is the last line — instead of
re-invoking `cactup sim run`, it re-invokes `cactup build run`:

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

`@CONFIG_DIR@`/`@ATTEMPT_ID@` are the build-submit-only variables (see below)
that let the compute node locate exactly which build to run, without
touching cactup's global state on this machine — the same role
`@SIMULATION_DIR@`/`@RESTART_ID@` play for a simulation re-invocation. Most
machines never need a build submit script at all; it exists only for
clusters whose login-node policy forbids compiling there. See
[Porting a Cluster](porting-a-cluster.html) for when to add one and
[meta.toml Reference](meta-toml.html) for the `[build]` keys that control
when it's used, and [Building Configs](../users/building-configs.html) for
the user-facing `cactup build`/`build run`/`build submit` split this feeds.

## Template variables

cactup expands `@VAR@` tokens in scripts at runtime. Variables include:

### Substitution rules

- `@NAME@` — substituted with the variable value
- `@@` — escapes to a literal `@` in output
- `@ENV(VARNAME)@` — reads the environment variable `VARNAME`
- Unknown `@NAME@` tokens cause an error

### Variable reference

{{cactup:template-vars}}

### Examples of variable expansion

Given:
- `job_name = "mysim"`
- `nodes = 4`
- `cpus_per_task = 2`
- `universe = "et-sif"`

These expansions occur:

```bash
#SBATCH --job-name=@JOB_NAME@              →  #SBATCH --job-name=mysim
#SBATCH --nodes=@NODES@                    →  #SBATCH --nodes=4
#SBATCH --cpus-per-task=@CPUS_PER_TASK@    →  #SBATCH --cpus-per-task=2
mpirun -np @TASKS@ @EXECUTABLE@ @PARFILE@  →  mpirun -np 8 /path/to/cactus_sim /path/to/sim.par
```

> **Note:** there is no `@UNIVERSE_WRAPPER@` token. When a config's build/run
> universe declares a wrapper, cactup wraps the *entire* command your script
> runs (see [Universes](meta-toml.html#universes)) — the script itself just
> invokes `@EXECUTABLE@` and cactup applies the wrapper around it.

## Variants in scripts

Submit and run scripts can have multiple **variants**. Each variant is a separate file in `submitscripts/` or `runscripts/`:

```
submitscripts/
  default.sh    # Normal simulation submission
  test.sh       # Used by `cactup test submit`
runscripts/
  default.sh    # Normal simulation run
  test.sh       # Used by `cactup test run`
```

Declare variants in meta.toml:

```toml
[variants.submitscript]
"default" = { queues = ["cpu", "gpu"], default = true }
"test" = { queues = ["cpu", "gpu"], test = true }

[variants.runscript]
"default" = { queues = ["cpu", "gpu"], default = true }
"test" = { queues = ["cpu", "gpu"], test = true }
```

### Test variants

The `test = true` flag marks a variant as the default for `cactup test run` and `cactup test submit`. Test runs typically use fewer nodes and tasks (e.g., single node, 2 MPI ranks).

Example test-specific script:

```bash
#!/bin/bash
# runscripts/test.sh — optimized for quick test validation

#SBATCH --job-name=test-@CONFIGURATION@
#SBATCH --nodes=1
#SBATCH --ntasks=2           # Small test, low task count
#SBATCH --time=00:30:00       # 30 minutes for tests

@ENV_SETUP@

cd @RUNDIR@-active

srun @EXECUTABLE@ @PARFILE@
```

## Script variants with different universes

If your machine supports both native and Singularity builds, you may need **different run/submit scripts** for each:

```
runscripts/
  native.sh      # Native build (no container)
  singularity.sh # Singularity-wrapped run
```

In meta.toml, associate scripts with build universes:

```toml
[variants.runscript]
"native" = { queues = ["cpu"], build-universes = ["host"], default = true }
"singularity" = { queues = ["cpu"], build-universes = ["et-sif"] }
```

Then cactup automatically selects the right script based on the config's build universe.

The `build-universes` key gates purely on the universe a config was **built**
in (never the universe you run or submit in). An omitted key means "compatible
with every build universe"; an explicitly empty list is a validation error.

### Gating queues by build universe

Queues accept the same `build-universes` key, so a machine that unifies several
upstream clusters behind one set of scheduler queues can restrict a queue to the
build flavors it actually serves:

```toml
[queues.gpu]
gpu = true
build-universes = ["host"]      # only native (host-built) configs

[queues.gpu-container]
gpu = true
name = "gpu"                    # same real partition
build-universes = ["et-sif"]    # only Singularity-built configs
```

When `-q` is omitted, cactup's default-queue pick is restricted to the queues
compatible with the config's build universe; naming an incompatible queue
explicitly is a hard error (there is no `--force-queue` escape, since the gate
is structural like script-variant compatibility).

## Shell vs Python scripts

A variant file is either shell (`.sh`) or Python (`.py`), and the two use
**different calling conventions**:

```
submitscripts/
  default.sh    # @VAR@-substituted, then env-setup auto-prepended after the header
  advanced.py   # run with python3; variables arrive as globals; STDOUT is the script
```

### Shell (`.sh`)

cactup substitutes every `@NAME@` token in the file, auto-prepends the phase's
env-setup (after the shebang for runscripts, after the leading `#`-directive
block for submitscripts), and that expanded text *is* the script.

### Python (`.py`)

`.py` variants are **not** `@VAR@`-substituted. cactup runs the file with
`python3`, handing it the variable set as JSON on stdin; a prelude binds every
variable as a **module global in canonical string form** (named exactly like the
tokens — `JOB_NAME`, `NODES`, `TASKS`, uppercase), plus a `typed` dict carrying
native ints/bools under the **same uppercase keys**. Whatever the script writes
to **stdout** becomes the produced script — you don't open or write files
yourself, and you own where env-setup goes.

```python
import sys

# Variables are already globals (strings); `typed` has native ints/bools.
lines = [
    "#!/bin/bash",
    f"#SBATCH --job-name={JOB_NAME}",
    f"#SBATCH --nodes={NODES}",
    f"#SBATCH --ntasks={typed['TASKS']}",   # an int, not a string
    ENV_SETUP,                               # the resolved env-setup block
    f"srun {EXECUTABLE} {PARFILE}",
]
sys.stdout.write("\n".join(lines) + "\n")    # stdout is the generated script
```

Python is useful for complex script generation, but shell is simpler for most machines.

#### Refusing a request

Some clusters have rules cactup cannot know about — a partition that only grants
GPUs in certain multiples, a combination of directives the scheduler silently
mangles. When the variables you were handed describe a job your machine cannot
actually run, `raise CactupError(...)`:

```python
gpus = typed['GPUS_PER_TASK'] * typed['TASKS_PER_NODE']
if gpus > gres:
    raise CactupError(
        f"this layout binds {gpus} GPUs per node but {QUEUE} only reserves {gres}.\n"
        f"Raise --cpus, or drop to --tpn 1."
    )
```

cactup prints your message and stops. Nothing is submitted, and no restart
directory is left behind:

```
$ cactup sim submit mysim -c 16 --tpn 2
error: this layout binds 2 GPUs per node but gpu2 only reserves 1.
Raise --cpus, or drop to --tpn 1. (/path/to/submitscripts/default.py)
```

Write the message for the person who typed the command: say which flag to
change, not just which invariant broke. Multi-line messages are preserved.

`CactupError` is for *policy* — a request that is legitimately impossible here.
Any other exception is treated as a bug in your script and reported with its
full traceback, so a typo stays debuggable instead of looking like a site rule.

## Environment setup: @ENV_SETUP@

The `@ENV_SETUP@` variable expands to shell commands that set up the build environment:

```bash
export PATH="/opt/bin:$PATH"
module load gcc
export LD_LIBRARY_PATH="/opt/lib:${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
```

This comes from the machine's `[environment]` section in meta.toml, with optional overrides from `[universes.*]`.

## Output and stdout: @STDOUT_FILE@, @STDERR_FILE@

These variables are the paths where scheduler output goes:

```bash
#SBATCH --output=@STDOUT_FILE@
#SBATCH --error=@STDERR_FILE@
```

On workstations with no batch system, these default to:

```bash
@STDOUT_FILE@  →  @RUNDIR@/@SIMULATION_NAME@.out
@STDERR_FILE@  →  @RUNDIR@/@SIMULATION_NAME@.err
```

Users can override with `-o FILE` / `-e FILE` flags.

## Checkpoint handling: @CHECKPOINT_WALLTIME@

For long simulations that chain across multiple jobs, the checkpoint walltime tells Cactus when to stop and write a checkpoint:

```bash
#SBATCH --time=@WALLTIME@                            # Hard job limit
@EXECUTABLE@ @PARFILE@ --walltime @CHECKPOINT_WALLTIME_HOURS@   # Stop & checkpoint before the hard limit
```

`@CHECKPOINT_WALLTIME@` is the deadline in canonical `HH:MM:SS` form; the
companion tokens `@CHECKPOINT_WALLTIME_HOURS@` and
`@CHECKPOINT_WALLTIME_SECONDS@` give the same deadline as a number, for whatever
form your Cactus invocation expects. Pass it so the run stops gracefully before
the hard wall-clock limit.

The checkpoint deadline is the hard walltime minus a buffer, set per-submission
with `--checkpt-buffer` (default: `max(walltime/24, 10 min)`).

## Debugging scripts

To see the expanded script without running it, use `--trace`:

```sh
cactup --trace sim submit mysim mysim.par -n 4 -w 1:00:00
```

The `--trace` flag prints every command cactup runs, including the script content.

## Writing a minimal submit script

Here's a minimal SLURM submit script that handles most use cases:

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

@ENV_SETUP@

cd @RUNDIR@-active

srun @EXECUTABLE@ @PARFILE@
```

And a minimal run script:

```bash
#!/bin/bash

set -e
@ENV_SETUP@

cd @RUNDIR@-active

@EXECUTABLE@ @PARFILE@
```

## Script validation

Test your scripts with a small test run:

```sh
cactup build testconfig
cactup test run --config testconfig -n 1
```

If the script has errors, cactup will show them.

## Common patterns

### SLURM with GPU

`@GPU@` is `true`/`false` for the selected queue's GPU flag; there is no
per-task GPU-count token, so hard-code the `--gres` count your queue provides
(one per node here):

```bash
#SBATCH --gres=gpu:1
#SBATCH --cpus-per-task=@CPUS_PER_TASK@
```

### PBS/Torque

```bash
#PBS -N @JOB_NAME@
#PBS -l nodes=@NODES@:ppn=@TASKS_PER_NODE@
#PBS -l walltime=@WALLTIME@
```

### Running under a container

Your script does **not** wrap the executable in `singularity`/`apptainer`
itself. Declare a `[universes.*]` entry with a `wrapper-argv` (or `wrapper`) in
meta.toml and point the build/run at it; cactup wraps the whole command your
script runs. The script stays container-agnostic:

```bash
srun @EXECUTABLE@ @PARFILE@
```

## Next steps

- [meta.toml Reference](meta-toml.html) — declare script variants
- [Machine Discovery](machine-discovery.html) — auto-detect machines
- [Porting a Cluster](porting-a-cluster.html) — complete example
