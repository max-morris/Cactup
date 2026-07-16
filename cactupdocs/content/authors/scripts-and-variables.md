+++
title = "Scripts & Variables"
description = "Write submit and run scripts with template variables"
+++

# Submit and Run Scripts

**Submit scripts** generate batch job submissions (e.g., SLURM `sbatch` files). **Run scripts** generate interactive simulations or test runs. Both are templates that expand `@VAR@` tokens at runtime.

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
```

Each script is typically 50-200 lines. cactup expands variables, then executes the script.

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
#SBATCH --ntasks-per-node=@TPN@
#SBATCH --cpus-per-task=@CPUS@
#SBATCH --time=@WALLTIME@
#SBATCH --output=@SIMULATION_NAME@.out
#SBATCH --error=@SIMULATION_NAME@.err
#SBATCH --partition=@QUEUE@

# Source environment
@ENV_SETUP@

# Run the simulation
cd @SIMULATION_DIR@
@RUNDIR_INIT@
srun ./cactus_@CONFIG_NAME@ @PARFILE@
```

### Run scripts (for interactive execution)

A run script is executed directly (not submitted to a queue). It sets up the environment and runs cactus directly. Example:

```bash
#!/bin/bash

set -e
@ENV_SETUP@

cd @SIMULATION_DIR@
@RUNDIR_INIT@

# For interactive runs, optionally load a debugger
@RUNDEBUG_PREFIX@
./cactus_@CONFIG_NAME@ @PARFILE@
```

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
#SBATCH --job-name=@JOB_NAME@        →  #SBATCH --job-name=mysim
#SBATCH --nodes=@NODES@              →  #SBATCH --nodes=4
#SBATCH --cpus-per-task=@CPUS@       →  #SBATCH --cpus-per-task=2
singularity exec @UNIVERSE_WRAPPER@  →  singularity exec -B /scratch /path/to/et.sif
```

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

#SBATCH --job-name=test-@CONFIG_NAME@
#SBATCH --nodes=1
#SBATCH --ntasks=2           # Small test, low task count
#SBATCH --time=00:30:00       # 30 minutes for tests

@ENV_SETUP@

cd @SIMULATION_DIR@
@RUNDIR_INIT@

srun ./cactus_@CONFIG_NAME@ @PARFILE@
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

Scripts can be shell (`.sh`) or Python (`.py`). The extension determines the interpreter:

```
submitscripts/
  default.sh    # Executed with /bin/bash
  advanced.py   # Executed with /usr/bin/python3
```

### Python scripts

Python scripts receive variables as global variables and a typed dict:

```python
#!/usr/bin/env python3

# @VAR@ tokens are available as globals
job_name = "@JOB_NAME@"
nodes = "@NODES@"
tasks = "@TASKS@"

# Also available: a 'typed' dict with structured data
print(f"Job: {job_name}, Nodes: {nodes}")
print(f"Typed info: {typed['nodes']}")  # 'typed' has structured metadata

# Generate the scheduler script
with open(job_name + ".slurm", "w") as f:
    f.write(f"#!/bin/bash\n")
    f.write(f"#SBATCH --job-name={job_name}\n")
    f.write(f"#SBATCH --nodes={nodes}\n")
    # ... etc
```

Python is useful for complex script generation, but shell is simpler for most machines.

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
#SBATCH --time=@WALLTIME@                    # Hard job limit
@RUNDIR_INIT@ checkpoint_walltime=@CHECKPOINT_WALLTIME@  # When to checkpoint
```

Cactus reads `checkpoint_walltime` from the environment and gracefully stops before the hard wall-clock limit.

The checkpoint buffer is configured per-submission with `--checkpt-buffer` (default: `max(walltime/24, 10 min)`).

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
#SBATCH --ntasks-per-node=@TPN@
#SBATCH --cpus-per-task=@CPUS@
#SBATCH --time=@WALLTIME@
#SBATCH --output=@STDOUT_FILE@
#SBATCH --error=@STDERR_FILE@
#SBATCH --partition=@QUEUE@

@ENV_SETUP@

cd @SIMULATION_DIR@
@RUNDIR_INIT@

srun ./cactus_@CONFIG_NAME@ @PARFILE@
```

And a minimal run script:

```bash
#!/bin/bash

set -e
@ENV_SETUP@

cd @SIMULATION_DIR@
@RUNDIR_INIT@

./cactus_@CONFIG_NAME@ @PARFILE@
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

```bash
#SBATCH --gres=gpu:@GPUS_PER_TASK@
#SBATCH --cpus-per-task=@CPUS@
```

### PBS/Torque

```bash
#PBS -N @JOB_NAME@
#PBS -l nodes=@NODES@:ppn=@TPN@
#PBS -l walltime=@WALLTIME@
```

### Singularity wrapper (in run/submit scripts)

```bash
srun singularity exec @UNIVERSE_WRAPPER@ ./cactus_@CONFIG_NAME@ @PARFILE@
```

## Next steps

- [meta.toml Reference](meta-toml.html) — declare script variants
- [Machine Discovery](machine-discovery.html) — auto-detect machines
- [Porting a Cluster](porting-a-cluster.html) — complete example
