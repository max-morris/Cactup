+++
title = "Running Simulations"
description = "Create, submit, and run Cactus simulations with flexible topology, checkpoint, and restart options"
+++

# Running Simulations

This guide covers everything you need to know about creating, submitting, and running Einstein Toolkit simulations with cactup.

## Creating a simulation

A simulation is created from a **parfile** (`.par`) or computed parfile (`.py`):

```sh
cactup sim create mysim mysim.par
```

This creates a simulation named `mysim` using the active config. You can also specify a config:

```sh
cactup sim create mysim mysim.par --config myconfig
```

By default, cactup stores simulations under your machine's `simulation-home`. You can specify a custom location:

```sh
cactup sim create mysim mysim.par --sim-dir /scratch/mysim
```

To replace an existing simulation:

```sh
cactup sim create mysim mysim.par --force
```

### Parfile substitution

A `.par` parfile is a template: cactup expands `@NAME@` tokens in it when the
restart starts, using the same variables submit and run scripts see. A copy of
your parfile is kept as the master; each restart gets its own expanded
`<name>.par` next to its output.

```
# Every variable from the variable reference is available:
IO::out_dir           = "@RUNDIR@"
Cactus::cctk_run_title = "@SIMULATION_NAME@ restart @RESTART_ID@"

# Read a knob (see Knobs below). The plain form is REQUIRED: submitting with
# the knob unset is an error. -OPTIONAL falls back to empty or a default.
kadathimporter::filename = "@KNOB(kadath-initial-data)@"
IO::checkpoint_dir       = "@KNOB-OPTIONAL(checkpoint-root, "checkpoints")@"

# Read an environment variable — resolved on the compute node, not where you
# typed `submit`.
IO::out_dir = "@ENV(SCRATCH)@/@SIMULATION_NAME@"

# A literal @ in a value is written @@. Comments are copied through as they
# are, so this line's @ and the @NAME@ above need nothing: email me@example.org
ADMBase::comment = "email me@@example.org"
```

`cactup sim submit` checks the parfile before anything is queued: a stray `@`,
an unknown variable or a required knob that is unset fail the submit on the
spot. Environment variables are the one thing it cannot check — their values
belong to the compute node — so `@ENV(NAME)@` is only judged when the job
runs.

A `.py` parfile is the escape hatch for computed parfiles: cactup runs it with
`python3` and its stdout becomes the parfile. Every variable is a Python
global, and knobs are available as the `knobs` dict and the
`knob(name, default)` helper. See
[Scripts & Variables](../authors/scripts-and-variables.html) for the complete
token grammar, the variable reference, and the Python conventions.

## Running simulations

### Quick run (interactive)

Run a simulation interactively on your current machine or allocation, bypassing the batch queue:

```sh
cactup sim run mysim
```

This runs in the foreground. Press Ctrl-C to stop it gracefully (cactup sends a TERMINATE signal).

### Batch submission

Submit a simulation to the batch queue:

```sh
cactup sim submit mysim mysim.par
```

This implicitly creates the simulation and submits it in one command. If the simulation already exists:

```sh
cactup sim submit mysim
```

## Implicit creation with submit/run

Both `sim submit` and `sim run` can create the simulation if it doesn't exist:

```sh
cactup sim submit mysim mysim.par -n 4 -w 1:00:00
```

This creates `mysim` and submits it in one step. Use `--overwrite` to replace an existing simulation:

```sh
cactup sim submit mysim mysim.par --overwrite
```

## Topology and resource control

Control how the simulation runs on compute nodes with topology flags. These apply to both `sim submit` and `sim run`:

### Nodes and tasks

```sh
cactup sim submit mysim -n 4        # 4 nodes
cactup sim submit mysim -T 128      # 128 MPI tasks total
cactup sim submit mysim -t 32       # 32 tasks per node
```

If you don't specify:
- Tasks per node (`-t`) defaults to filling the node: `floor(MAX_CPUS_PER_NODE / CPUS_PER_TASK)` (one task per CPU when `-c` is 1)
- Total tasks (`-T`) defaults to nodes × tasks-per-node (or the script variant's `tasks` setting when it declares one)

For example, a 4-node submission with 32 CPUs per node and no `-T` or `-t` creates 4 × 32 = 128 tasks.

### CPUs per task

Control thread count per MPI rank:

```sh
cactup sim submit mysim -c 2        # 2 CPUs (threads) per task
```

This is useful for hybrid MPI+OpenMP runs. Default is 1 CPU per task.

### GPU support

Enable GPUs (if your machine supports them):

```sh
cactup sim submit mysim --gpu
```

You usually don't need this: a queue marked `gpu = true` in the machine
database turns GPUs on by itself.

Control how many GPUs each MPI task gets:

```sh
cactup sim submit mysim -G 2       # 2 GPUs per task
```

If you don't specify it, you get **1 GPU per task** — the right answer almost
everywhere, and it stays right when you change the rest of the layout. A machine
whose partitions want something else declares `default-gpus-per-task`.

Unlike CPUs, GPUs can't be oversubscribed. If a layout needs more GPUs per node
than the queue has, cactup refuses it up front rather than submitting a job that
will never schedule:

```
$ cactup sim submit mysim -q gpu2 -c 16
error: this layout needs 4 GPUs per node (1 per task × 4 tasks/node) but queue
"gpu2" has 2 (§8.5); lower --tpn/--tasks, or raise --cpus so fewer ranks land on
a node
```

Some machines enforce extra rules of their own and will refuse a request with a
message explaining what to change — those come from the machine's submit script,
not from cactup itself.

`--gpus-per-task` applies only to GPU runs; passing it when the run isn't using
GPUs is an error rather than being silently ignored. On a non-GPU run the
`@GPUS_PER_TASK@` script variable is `0`.

## Walltime and checkpointing

### Walltime

Specify the total runtime for the entire simulation (including restarts):

```sh
cactup sim submit mysim -w 10:00:00        # 10 hours
cactup sim submit mysim -w 2-12:00:00      # 2 days, 12 hours
```

The walltime format is `(DD-)?HH:MM:SS`. Leading fields may be elided, so `SS`,
`MM:SS`, and `HH:MM:SS` are all valid (`30:00` = 30 minutes). Fields do **not**
need zero-padding — `6:00:00` and `06:00:00` are equivalent.

If the simulation needs more time than a single queue job allows, cactup **chains** jobs: it automatically submits follow-up jobs at configured checkpoints, with restart data from the previous job. You only specify the total walltime; cactup handles the rest.

### Checkpoint buffer

cactup reserves time at the end of each job window for graceful shutdown and checkpoint writing:

```sh
cactup sim submit mysim --checkpt-buffer 00:15:00   # 15-minute buffer
```

By default, the buffer is `max(walltime / 24, 10 minutes)`, ensuring cactup has time to write a checkpoint before the hard wall-clock limit. The checkpoint becomes `@CHECKPOINT_WALLTIME@ = hard walltime − buffer` in submit scripts.

## Queue and allocation control

### Queue selection

Specify which batch queue to submit to:

```sh
cactup sim submit mysim -q gpu       # GPU queue
cactup sim submit mysim -q cpu       # CPU queue
```

If not specified, cactup uses the machine's default queue.

### Allocation and account

Specify an account or allocation to charge:

```sh
cactup sim submit mysim -a my_project
```

(The default allocation comes from the `allocation` knob if you've set one.)

### Job name

By default, the job name is the simulation name. Override it:

```sh
cactup sim submit mysim --job-name my-run-123
```

## Knobs

A **knob** is a value cactup remembers for you on this machine. The
**standard knobs** — `allocation`, `queue`, `mail`, `mail-type`, `user`,
`email`, `wisdom-frequency`, `wisdom-kind` — are the defaults behind the
flags above: `-a` beats the `allocation` knob, which beats the machine's
default.

```sh
cactup knob                        # show every knob
cactup knob allocation             # show one
cactup knob allocation hpc_xxx     # set one
```

### Custom knobs

You can also define your own. A custom knob is a named value for your
parfiles, scripts and optionlists to read through `@KNOB(name)@` — a path to
initial data, a resolution tag, anything you would otherwise edit into the
parfile by hand on every machine. Names are kebab-case: lowercase letters,
digits after the first character, dashes inside.

```sh
# The first time, -c/--custom says "yes, make a new knob":
cactup knob -c kadath-initial-data /work/me/ID/BHNS.info

# From then on, set it like any other knob:
cactup knob kadath-initial-data /work/me/ID/BHNS_v2.info
```

Setting a name that is neither standard nor an existing custom knob is an
error, so a typo cannot quietly create a knob nothing reads. `cactup knob`
lists standard and custom knobs separately. To get rid of a custom knob:

```sh
cactup knob delete kadath-initial-data
```

On a standard knob, `delete` only clears the stored value: the knob goes back
to its default (standard knobs always exist).

### Overriding a knob for one command: `-K`

`-K NAME=VALUE` (or `-K NAME VALUE`) is a global flag that overlays a knob for
the duration of one command. Nothing is stored; the value applies everywhere
the command would have read the knob — the topology defaults, and every
`@KNOB(name)@` in the scripts, optionlist or parfile it processes. The knob
need not exist yet.

```sh
cactup -K queue=gpu sim submit mysim                       # like -q gpu
cactup sim submit bhns bhns.par -K kadath-initial-data /work/me/ID/other.info
cactup -K allocation=hpc_yyy -K mail-type=none sim submit mysim
```

Flags placed after the subcommand work too, and the flag repeats. The
effective knob values — overlay included — are frozen into the restart when
you submit, so the job sees exactly what you saw, even if you change a knob
while it waits in the queue.

## Notifications

Receive email updates:

```sh
cactup sim submit mysim --mail user@example.com       # Send email to...
cactup sim submit mysim --mail-type all               # ...on all events (default)
```

Mail type options:

```sh
--mail-type all         # Begin, end, fail, requeue
--mail-type begin,end   # Only at start and finish
--mail-type fail        # Only on failure
```

## Output redirection

Control where stdout/stderr go:

```sh
cactup sim submit mysim -o sim.out -e sim.err
```

## Universe control

Override the build universe for this submission (advanced):

```sh
cactup sim submit mysim --universe et-sif     # Force a specific universe
cactup sim submit mysim --no-universe          # Force native (host) execution
```

By default, the simulation runs in the universe it was built in.

## Forcing submission

To bypass safety checks:

```sh
cactup sim submit mysim --force              # Bypass all checks
cactup sim submit mysim --force-queue        # Bypass queue compatibility check
```

Use `--force` only if you know what you're doing — it skips warnings about incompatible queues and unresolved parfiles.

## Stopping and cleaning

Stop a running or queued simulation:

```sh
cactup sim stop mysim              # Graceful stop (sends TERMINATE signal)
cactup sim stop mysim --force      # Kill the job (scheduler `stop` command)
```

Clean up aborted restart data:

```sh
cactup sim clean mysim
```

This removes incomplete restart checkpoints, useful if a job crashed mid-restart.

## Managing simulations

See all simulations:

```sh
cactup sim list                    # Summary
cactup sim list --long             # Extended details
cactup sim list --all              # Across all installations
```

View one simulation:

```sh
cactup sim show mysim              # Summary
cactup sim show mysim --long       # Full details with per-restart info
cactup sim show mysim --output-dir          # Print the current output directory
cactup sim show mysim --output-dir --restart-id 2  # Print the 3rd restart output dir
```

`--long` also prints the paths of the optionlist and thornlist this simulation's
executable was built from. Those are copies taken when the simulation was
created, so they still answer "which thorns and compiler flags produced this
run?" long after the config itself has been rebuilt or deleted. Simulations
created by an older cactup show `(not recorded)`.

## Viewing output

Stream simulation output:

```sh
cactup sim log mysim               # Last 100 lines
cactup sim log mysim --follow      # Live split-pane view of stdout/stderr
```

See [Monitoring & Logs](monitoring.html) for the follow-pane keybindings
and the `--follow-out`/`--follow-err` single-stream variants.

Get the output directory:

```sh
OUTDIR=$(cactup sim show mysim --output-dir)
ls $OUTDIR          # See all output files
```

## Delete and trash

Move a simulation to trash (keeps data):

```sh
cactup sim delete mysim
```

Permanently delete:

```sh
cactup sim delete mysim --force    # Permanent deletion
```

Data in trash can be recovered from your `simulation-home/TRASH/` directory.

## Full command reference

{{cactup:cli command="sim create"}}

{{cactup:cli command="sim submit"}}

{{cactup:cli command="sim run"}}

{{cactup:cli command="sim stop"}}

{{cactup:cli command="sim clean"}}

{{cactup:cli command="sim delete"}}

{{cactup:cli command="sim list"}}

{{cactup:cli command="sim show"}}

{{cactup:cli command="sim log"}}

## Examples

### Single-node interactive run with all defaults

```sh
cactup sim run mysim
```

### 4-node, 2-hour batch submission with email notifications

```sh
cactup sim submit mysim mysim.par -n 4 -w 2:00:00 \
  --mail me@example.com --mail-type all
```

### GPU-accelerated run on 2 nodes with 4 CPUs per task

```sh
cactup sim submit mysim -n 2 --gpu -c 4 -w 4:00:00 -q gpu
```

### Large simulation with automatic job chaining (24-hour total time)

```sh
cactup sim submit bigrun bigrun.par -n 64 -w 24:00:00
```

This automatically chains jobs and restarts as needed to reach 24 hours total runtime.

## Troubleshooting

**"Parfile not found"**: Check the path to your `.par` file.

**"knob NAME is unset or empty"**: The parfile reads `@KNOB(NAME)@` and the
knob has no value on this machine. Set it once with `cactup knob NAME VALUE`
(`-c` first if it is a new custom knob), or pass `-K NAME=VALUE` to this
command.

**"stray '@'"**: A `.par` parfile is a template, so a literal `@` outside a
`#` comment must be written `@@`. The message names the line.

**"Config not found"**: Build the config first with `cactup build myconfig`.

**"Incompatible queue"**: The optionlist variant isn't compatible with the chosen queue. Use `cactup machine show --variants` to see which variants work with which queues. Or use `--force-queue` to override (use carefully).

**"Queue not found"**: Check available queues with `cactup machine show` and pick a valid `-q` name.

**Simulation doesn't start**: Check the queue status with your scheduler (e.g., `squeue` on SLURM). Use `cactup sim log mysim` to see errors.

## Next steps

- [Monitoring & Logs](monitoring.html) — track running simulations
- [Test Suites](test-suites.html) — validate your config before large runs
