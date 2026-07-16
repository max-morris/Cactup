+++
title = "Monitoring & Logs"
description = "Track, inspect, and debug running simulations and test suites"
+++

# Monitoring & Logs

This guide covers how to monitor your running simulations and test suites, view their output, and diagnose issues.

## Listing simulations

See all simulations:

```sh
cactup sim list
```

Output shows:
- Simulation name
- Attached config
- Status (running, queued, completed, etc.)
- Submission time
- Current restart checkpoint

For more detail:

```sh
cactup sim list --long
```

This shows per-restart details: restart count, output directory, completion status.

List simulations across all installations:

```sh
cactup sim list --all
```

## Viewing simulation details

Show a simulation summary:

```sh
cactup sim show mysim
```

This displays:
- Config and build universe
- Status and progress
- Job ID (if submitted to queue)
- Submission time and node allocation
- Current restart and output location

For detailed per-restart information:

```sh
cactup sim show mysim --long
```

## Output directory

Get the output directory for the current (active) restart:

```sh
OUTDIR=$(cactup sim show mysim --output-dir)
echo $OUTDIR
```

This prints the absolute path, useful for viewing output files:

```sh
ls $OUTDIR              # List output files
head $OUTDIR/output.txt # View partial output
```

Get the output directory for a specific restart (numbered from 0):

```sh
cactup sim show mysim --output-dir --restart-id 2
```

This prints the directory for restart #2 (the 3rd checkpoint/restart cycle).

## Streaming output

View the last 100 lines of stdout/stderr:

```sh
cactup sim log mysim
```

Stream output in real time (like `tail -f`):

```sh
cactup sim log mysim --follow
```

Press Ctrl-C to stop streaming. This is useful for watching a running simulation interactively.

## Output files

Simulation output is written to a directory per restart. Common output files:

- `output.txt` — merged stdout and stderr
- `output-*.txt` — thorn-specific output files
- `cactus_*.out` / `.err` — raw scheduler output
- Checkpoint files (HDF5, etc.)

The `--output-dir` flag tells you where to look:

```sh
OUTDIR=$(cactup sim show mysim --output-dir)
find $OUTDIR -name "*.h5" | head  # Find HDF5 output
```

## Monitoring test suites

List test runs:

```sh
cactup test list       # Summary
cactup test list --long    # Extended details
```

View a test run:

```sh
cactup test show mytest
```

Stream test output:

```sh
cactup test log mytest --follow
```

## Understanding simulation status

Simulations have several states:

- **Running** — actively executing on a compute node
- **Queued** — submitted to the batch queue, waiting for resources
- **Completed** — finished successfully (all restarts done)
- **Stopped** — user-initiated stop
- **Failed** — exited with an error

View the current status:

```sh
cactup sim show mysim
```

For submitted jobs, the scheduler's job status is also shown (from `squeue`, etc.).

## Checkpoints and restarts

When a simulation reaches a configured checkpoint time or hits the queue wall-clock limit, cactup:

1. Writes a checkpoint file (from which the simulation can resume)
2. Completes the current job
3. (For chained submissions) automatically submits a follow-up job with the checkpoint data

View restart history:

```sh
cactup sim show mysim --long
```

This shows each restart:
- Restart ID (0, 1, 2, ...)
- Output directory
- Completion status
- Start/stop times

## Diagnostics

If a simulation fails:

1. **Check the output**:
   ```sh
   OUTDIR=$(cactup sim show mysim --output-dir)
   tail $OUTDIR/output.txt
   ```

2. **Check the job status** (if submitted to a queue):
   ```sh
   cactup sim show mysim      # Shows job ID
   squeue -j <JOB_ID>         # (SLURM example)
   ```

3. **Check the scheduler's error log**:
   ```sh
   cat $OUTDIR/cactus_0000.err
   ```

4. **Re-run with verbose output** (if the issue is reproducible):
   ```sh
   cactup --trace sim run mysim
   ```
   The `--trace` flag prints every shell command cactup runs, useful for diagnosing scheduler issues.

## Cleaning incomplete restarts

If a job crashes mid-restart, the incomplete checkpoint data may remain. Clean it up:

```sh
cactup sim clean mysim
```

This removes aborted restart directories, freeing disk space. Completed restarts are preserved.

## Output directory structure

Under your machine's `simulation-home` (e.g., `~/.cactup/simulations/`), simulations are stored as:

```
simulation-home/
  mysim/
    metadata.toml       # Simulation metadata (parfile path, config, etc.)
    output-0000/        # Restart 0 output files
    output-0001/        # Restart 1 output files
    ...
  othersim/
    ...
  TRASH/                # Deleted simulations (can be recovered)
```

Each `output-NNNN/` directory contains the actual Cactus output files for that restart.

## Full command reference

{{cactup:cli command="sim list"}}

{{cactup:cli command="sim show"}}

{{cactup:cli command="sim log"}}

{{cactup:cli command="sim clean"}}

{{cactup:cli command="test list"}}

{{cactup:cli command="test show"}}

{{cactup:cli command="test log"}}

## Examples

### Monitor a just-submitted simulation

```sh
cactup sim submit mysim -n 4 -w 2:00:00
cactup sim show mysim
cactup sim log mysim --follow
```

### Find a specific output file from a completed restart

```sh
OUTDIR=$(cactup sim show mysim --output-dir --restart-id 1)
find $OUTDIR -name "*.h5" -newer $OUTDIR -ls
```

### Clean up and resubmit after a job failure

```sh
cactup sim clean mysim
cactup sim submit mysim  # Re-submits, starting from the last checkpoint
```

## Troubleshooting

**"Simulation not found"**: Use `cactup sim list` to see available simulations.

**"No output"**: Check the status with `cactup sim show mysim`. If still queued, wait for it to start. If running, the output directory may not exist yet.

**"Permission denied" reading output**: Output files are in your `simulation-home`, usually readable by you. Check file permissions.

**"Output directory is stale"**: Different restart directories have different numbers. Use `--restart-id` to access older restarts.

## Next steps

- [Running Simulations](running-simulations.html) — control topology, walltime, queues
- [Test Suites](test-suites.html) — validate configs before large runs
