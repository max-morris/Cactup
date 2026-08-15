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

The machine's queue configuration determines what GPU resources are allocated.

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

**"Config not found"**: Build the config first with `cactup build myconfig`.

**"Incompatible queue"**: The optionlist variant isn't compatible with the chosen queue. Use `cactup machine show --variants` to see which variants work with which queues. Or use `--force-queue` to override (use carefully).

**"Queue not found"**: Check available queues with `cactup machine show` and pick a valid `-q` name.

**Simulation doesn't start**: Check the queue status with your scheduler (e.g., `squeue` on SLURM). Use `cactup sim log mysim` to see errors.

## Next steps

- [Monitoring & Logs](monitoring.html) — track running simulations
- [Test Suites](test-suites.html) — validate your config before large runs
