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

Follow a running simulation with `-f`/`--follow`. This opens a full-screen,
side-by-side terminal UI: stdout live-tailing in the left pane, stderr in
the right.

```sh
cactup sim log mysim --follow
```

Each pane scrolls independently:

- `Tab` — switch focus between panes
- Arrow keys or `j`/`k`, `PgUp`/`PgDn` — scroll the focused pane
- `Left`/`Right` or `h`/`l` — pan long lines horizontally
- `Home`/`g` — jump to the top; `End`/`G` — jump to the bottom
- Mouse wheel also works (your terminal's own wheel-to-arrow handling while
  the TUI leaves the mouse alone, the TUI's own once `m` grabs it)

Scrolling up pauses auto-follow for that pane, so new output doesn't yank
you away while you're reading. Scroll back to the bottom (or press `End`)
to resume following. Press `q` or `Ctrl-C` to quit; `Esc` also quits, unless
a search is active, in which case it clears the search first.

New output is picked up within about 50ms while the run is actively
writing; once the log goes quiet, cactup backs off and polls less
frequently, to be kind to shared filesystems like Lustre/NFS.

### Copying

A full-screen view can take away the one thing a plain `tail -f` gets for
free from the terminal: selecting text with the mouse. This one doesn't.

By default the TUI does not capture the mouse, so everything your terminal
already does keeps working inside the panes: double-click a word,
triple-click a line, drag out a block, and copy it with whatever your
terminal uses (`Ctrl-Shift-C`, `⌘C`, middle-click paste). This route needs
nothing from cactup and works in every terminal. Press `m` to hand the
mouse to the TUI instead — then the wheel scrolls, clicking focuses a pane,
and click-drag selects whole lines — and `m` again to give it back.

The TUI can also copy for you, through the system clipboard:

- `y` in normal mode copies exactly what's on screen in the focused pane —
  the quick "copy what I'm looking at." `Ctrl-Shift-C` does the same, in
  terminals that let cactup see that chord rather than keeping it for their
  own copy or sending it as a plain `Ctrl-C`; when it's available the footer
  lists it next to `y`.
- `Y` copies the pane's whole retained buffer (up to 10,000 lines), not just
  the visible slice.
- `v` starts vim-style line-visual selection in the focused pane, anchored
  on the newest visible line (and pausing that pane's follow). Extend it
  with `j`/`k`, the arrow keys, `PgUp`/`PgDn`, or `g`/`G`; `y` (or
  `Ctrl-Shift-C`) copies the selected span; `Esc` or `v` cancels without
  copying. `q` still quits even mid-selection.
- Click-drag with the mouse (while it's captured) makes the same kind of
  selection — a plain click just focuses the pane, as before. The selection
  survives releasing the mouse button, so `y` afterwards copies it.

All of these go out via the terminal's OSC 52 escape sequence, which is
what lets copying work over SSH and through tmux/screen — the bytes ride
the same connection back to your local terminal, which is the one actually
holding the clipboard.

Inside tmux this needs no configuration: tmux understands OSC 52 itself and
at its default `set-clipboard external` forwards the sequence out to the
real terminal. Setting `set -g set-clipboard on` additionally drops the
copied text into tmux's own paste buffer, so `prefix ]` pastes it without
involving the outer terminal at all — worth turning on, but not required.
(`allow-passthrough` is *not* involved; cactup deliberately doesn't use
tmux's DCS passthrough, which would bypass exactly the handling that makes
this work out of the box.) GNU screen doesn't interpret OSC 52, so cactup
wraps the sequence in screen's passthrough for it automatically.

The catch is the terminal at the far end: it has to be willing to take an
OSC 52 clipboard write (xterm needs `allowWindowOps`; some terminals refuse
it outright), and a terminal that refuses simply discards the sequence —
there is no ack, so cactup can't tell you it failed. A copy that seems to
do nothing is usually that. If it happens, your terminal's own selection —
which is what the mouse does until you press `m` — is the way around it. Very large copies are capped (100 KB); the
footer says so when a copy gets cut off.

### Searching

Press `/` to search the focused pane forward, or `?` to search backward.
The pattern is a regex (so `ERROR|WARN` lights up both), and matching is
smart-case — case-insensitive unless the pattern contains an uppercase
letter. Search is per pane: stdout and stderr each keep their own pattern,
shown in that pane's border, so switching panes doesn't lose either search.

The search is incremental: as you type, the view jumps to and highlights
the nearest match, with a `[3/17]`-style counter next to the prompt.
`Enter` commits the pattern; `Esc` abandons it and puts the view back where
it was; `Backspace` on an already-empty pattern also backs out; `Ctrl-U`
clears what's typed so far.

Once a pattern is committed, `n`/`N` jump to the next/previous match,
wrapping around the buffer with a "search hit BOTTOM, continuing at TOP"
notice (vim's phrasing) when they do. All matches stay highlighted; the one
you're on is picked out from the rest. Landing on a match centers it
vertically and pans sideways if it's off-screen, and — like any other jump
— pauses that pane's follow until you scroll back to the bottom or press
`End`.

If stdout isn't a terminal — for example, you've piped it to another
command — `--follow` falls back to the old interleaved streaming instead
of opening the TUI.

To follow just one stream, use `-o`/`--follow-out` or `-e`/`--follow-err`
instead of `--follow`. These stream plain `tail -f`-style output for just
stdout or just stderr — handy for piping into `grep` or similar, since
headers and "waiting for output" notices go to stderr, keeping stdout
clean:

```sh
cactup sim log mysim --follow-out | grep ERROR
```

`--follow`, `--follow-out`, and `--follow-err` are mutually exclusive.

## Output files

Simulation output is written to a directory per restart. What you'll find there:

- The scheduler's stdout/stderr — by default `<simulation-name>.out` / `.err`
  in the run directory (overridable per-submission with `-o` / `-e`)
- The Cactus run output — subdirectories and files named by your **parfile's**
  IO settings (Cactus decides these, not cactup)
- Checkpoint files (HDF5, etc.) written by the checkpointing thorns

The `--output-dir` flag tells you where to look:

```sh
OUTDIR=$(cactup sim show mysim --output-dir)
find $OUTDIR -name "*.h5" | head  # Find HDF5 output
```

## Monitoring builds

A build — foreground or [queue-submitted](building-configs.html) — creates a
numbered **build attempt**, and `cactup build list`/`show`/`log`/`stop`/
`prune` manage those attempts the same way the commands above manage a
simulation.

List build attempts (one row per config, its most recent attempt):

```sh
cactup build list            # summary
cactup build list --long     # extended per-attempt details
cactup build list --all      # across every installation
```

Show one config's most recent build attempt in detail:

```sh
cactup build show myconfig
cactup build show myconfig --long
```

This is the place to look for a build's outcome: whether it's still
queued or running, its job id (if submitted), and — once it's finished —
whether it actually completed, independent of what the scheduler thinks. A
batch job can exit 0 without the build having finished; `build show` reports
what cactup itself found when it checked, not the scheduler's own
accounting.

Stream a build's output the same way `sim log` streams a simulation's —
`--follow`/`-f` opens the split-pane stdout/stderr TUI, or use
`-o`/`--follow-out` and `-e`/`--follow-err` for a single stream:

```sh
cactup build log myconfig --follow
```

Stop a running or queued build:

```sh
cactup build stop myconfig
cactup build stop myconfig -f     # kill it directly instead of a graceful stop
```

Old build attempts accumulate under `.cactup-builds/` inside the config
directory; prune them, keeping only the most recent N (never automatic —
cactup doesn't delete build history on its own):

```sh
cactup build prune myconfig --keep 5
```

`<name>` defaults to the active config for all five, matching `config show`.

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

Stream test output the same way `sim log` does — `--follow` opens the
split-pane TUI, or use `--follow-out`/`--follow-err` for a single stream:

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

3. **Check the scheduler's error log** (default `<simulation-name>.err`):
   ```sh
   cat "$OUTDIR"/*.err
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
    .cactup/            # cactup's per-sim metadata dir
      simulation.toml   #   metadata (parfile, config, universe, restart state, …)
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

{{cactup:cli command="build list"}}

{{cactup:cli command="build show"}}

{{cactup:cli command="build log"}}

{{cactup:cli command="build stop"}}

{{cactup:cli command="build prune"}}

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

### Watch a queued build from submission to finish

```sh
cactup build submit myconfig
cactup build show myconfig     # job id, live status
cactup build log myconfig -f   # stream output until it finishes or you Ctrl-C
```

## Troubleshooting

**"Simulation not found"**: Use `cactup sim list` to see available simulations.

**"No build attempts found"**: `cactup build list`/`show` only ever report on attempts that exist — build the config at least once with `cactup build myconfig` first.

**"No output"**: Check the status with `cactup sim show mysim`. If still queued, wait for it to start. If running, the output directory may not exist yet.

**"Permission denied" reading output**: Output files are in your `simulation-home`, usually readable by you. Check file permissions.

**"Output directory is stale"**: Different restart directories have different numbers. Use `--restart-id` to access older restarts.

## Next steps

- [Running Simulations](running-simulations.html) — control topology, walltime, queues
- [Test Suites](test-suites.html) — validate configs before large runs
