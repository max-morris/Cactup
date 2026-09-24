+++
title = "Scripts & Variables"
description = "Write submit and run scripts with template variables"
+++

# Submit and Run Scripts

**Submit scripts** generate batch job submissions (e.g., SLURM `sbatch` files). **Run scripts** generate interactive simulations or test runs. **Build submit scripts** are a third, optional kind, for clusters that require compiling on a compute node rather than the login node. All three are templates that expand `@VAR@` tokens at runtime.

## Scripts directory structure

Scripts are stored alongside meta.toml:

```
<mdb>/<machine>/
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

The same engine expands submit/run scripts, build submit scripts, optionlists,
`[scheduler]` commands and `.par` parfiles, in one left-to-right pass:

- `@NAME@` — the variable's value. An unknown name is an error, never a
  silent leak (this catches the `@QEUEUE@` class of typo).
- `@@` — a literal `@`. The result is never re-scanned, so `@@NAME@@` yields
  the literal text `@NAME@`.
- A lone `@` that is neither of the above is an error.
- **Comments are skipped.** A comment is copied through verbatim: no token
  expands in it, a lone `@` (an email address, a `@NAME@` mentioned in prose)
  needs no escape, and `@@` in a comment stays `@@`. What counts as a comment
  follows the file's own syntax:
  - *Shell* (`.sh` scripts, `[scheduler]` commands, `[build].make`, universe
    `wrapper`): a `#` that begins a word, outside quotes and heredoc bodies,
    to the end of the line. A scheduler directive is **not** a comment: a
    line whose first non-blank character is a `#` glued to a word (`#SBATCH`,
    `#PBS`, `#$`) is substituted like code. A commented-out command has the
    same shape (`#module load foo`) and is scanned too, so write comments as
    `# ` with a space.
  - *Parfile* (`.par`): a `#` outside a `"…"` string, to the end of the line.
    Inside a multi-line string such as an `ActiveThorns` list, a `#` on any
    line after the first is also a comment, exactly as Cactus reads it.
  - *Optionlist* (the rendered native file): a `#` anywhere, to the end of
    the line, since Cactus strips it before reading the line.
  - `[paths]` values and universe `wrapper-argv` words have no comments;
    every character is scanned.

Two **computed** token families read values that are not variables. Each has a
required form and two optional forms:

| Token | Expands to |
|---|---|
| `@ENV(NAME)@` | environment variable `NAME`; **error** if unset or empty |
| `@ENV-OPTIONAL(NAME)@` | `NAME`, or the empty string if unset or empty |
| `@ENV-OPTIONAL(NAME, default)@` | `NAME`, or `default` if unset or empty |
| `@KNOB(name)@` | the knob `name`; **error** if unset or empty |
| `@KNOB-OPTIONAL(name)@` | the knob, or the empty string |
| `@KNOB-OPTIONAL(name, default)@` | the knob, or `default` |

Environment variable names are `UPPER_SNAKE`; knob names are kebab-case
(lowercase letters, digits after the first character, dashes inside). The
required forms take no default — that would defeat them — and blanks around
the pieces are fine.

The **default value** is written one of three ways:

```
@ENV-OPTIONAL(SCRATCH, "/tmp/scratch dir")@     double-quoted
@ENV-OPTIONAL(SCRATCH, '/tmp/scratch dir')@     single-quoted
@ENV-OPTIONAL(NTHREADS, 4)@                     bare: letters and digits only
```

Quotes are not part of the output. Inside quotes, `\"`, `\'` and `\\` stand
for the quote or backslash itself; any other backslash is kept literally. A
bare default may contain nothing but ASCII letters and digits — a space, `/`,
`_` or `.` in one is an error that tells you to quote it — and an empty
default must be spelled `""`.

**Where the value comes from.** `@ENV(…)@` reads the environment of the
process doing the substitution: the login node for a submit script or
optionlist, the compute node for a run script or parfile. `@KNOB(…)@` reads
the knobs as they were when the command ran — stored knobs, their derived
defaults, and any `-K NAME=VALUE` overlay — and that snapshot is frozen into
the restart/build/test metadata, so a compute-node run sees the same values
without touching cactup's database. Knobs are available in scripts,
optionlists and parfiles, not in `meta.toml` `[paths]` (which resolve before
any knob context exists). See [Running Simulations](../users/running-simulations.html#knobs)
for standard vs. custom knobs and `-K`.

### Variable reference

`@CACTUP@`, which every re-invocation line above uses, is the absolute path of
the exact cactup build that rendered the script — the versioned file
`~/.cactup/bin/cactup-<build>`, not the `~/.cactup/bin/cactup` link on the
user's `PATH`. A job therefore runs the build it was submitted with, even if
cactup updates itself while the job is queued (see
[Updating cactup](../users/updating.html#where-builds-live-and-why-jobs-are-safe)).
Always call cactup through `@CACTUP@` in scripts, never by a bare `cactup`.

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
native ints/bools under the **same uppercase keys**, and a `knobs` dict holding
the knob snapshot. Whatever the script writes to **stdout** becomes the produced
script — you don't open or write files yourself, and you own where env-setup
goes.

```python
import sys

# Variables are already globals (strings); `typed` has native ints/bools.
lines = [
    "#!/bin/bash",
    f"#SBATCH --job-name={JOB_NAME}",
    f"#SBATCH --nodes={NODES}",
    f"#SBATCH --ntasks={typed['TASKS']}",   # an int, not a string
    f"#SBATCH --account={knob('allocation')}",  # like @KNOB(allocation)@
    ENV_SETUP,                               # the resolved env-setup block
    f"srun {EXECUTABLE} {PARFILE}",
]
sys.stdout.write("\n".join(lines) + "\n")    # stdout is the generated script
```

`knob(name)` is the Python counterpart of `@KNOB(name)@`: an unset or empty
knob refuses the run with the same message cactup would print for the token.
`knob(name, default)` mirrors `@KNOB-OPTIONAL(name, default)@`, and the raw
`knobs` dict is there when you want `in`/`.get()`. Environment variables are
plain `os.environ`. The same conventions hold for a `.py` **parfile**.

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
