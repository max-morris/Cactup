+++
title = "Test Suites"
description = "Run the Einstein Toolkit thorn test suites to validate your builds"
+++

# Test Suites

The Einstein Toolkit includes comprehensive test suites for each thorn (module). Use cactup to run these tests to validate your config builds before production simulations.

## Running tests interactively

Run the full test suite for the active config:

```sh
cactup test run
```

This executes interactively in the foreground.

### Selecting specific tests

Run only specific tests:

```sh
cactup test run TestArrangement   # All tests in an arrangement
cactup test run McLachlan/ML_BSSN # All tests in a thorn
cactup test run ML_BSSN           # The same thorn, without its arrangement
cactup test run some-test-name    # A specific test
cactup test run ML_BSSN/some-test # A specific test of one thorn
```

Where a name is both an arrangement and a thorn (CarpetX, for one), the bare
name selects the whole arrangement; write `CarpetX/CarpetX` for just the thorn.

Combine multiple selections:

```sh
cactup test run McLachlan/ML_BSSN TestArrangement another-test
```

If no tests are specified, all tests in the suite run.

### Test run options

Specify a config (instead of using the active one):

```sh
cactup test run --config myconfig
```

Run with different topology (nodes, CPUs, GPU):

```sh
cactup test run -n 2 --gpu -c 2     # 2 nodes, GPU, 2 CPUs per task
```

The queue decides which test scripts run, just as it does for simulations;
to get different ones, choose a different queue with `-q`.

Testsuites run only on queues the machine has a test script for. On any other
queue, `test run`/`test submit` stops with an error listing the queues your
config can run its testsuite on instead, for example
`no test run script serves queue "bigmem"; config "sim" can run its testsuite
on gpu2, gpu4; pick one of those with -q`. If there are none, the error says
the machine cannot run that config's testsuite at all.

Override the universe:

```sh
cactup test run --universe et-sif    # Run tests in a specific universe
cactup test run --no-universe        # Run tests natively
```

## Submitting tests to the queue

Submit the test suite as a batch job:

```sh
cactup test submit
```

This requires a valid job allocation:

```sh
cactup test submit -n 1 -w 1:00:00   # 1 node, 1 hour
```

Pass `--config` and `--universe` like with `test run`.

## Topology and resource control

Test runs use the same topology flags as simulations:

```sh
cactup test run -n 4        # 4 nodes
cactup test run -T 64       # 64 MPI tasks
cactup test run -t 16       # 16 tasks per node
cactup test run -c 2        # 2 CPUs per task
cactup test run --gpu       # Enable GPU
cactup test run -w 2:00:00  # Walltime limit
```

By default, tests run on a single node with 2 MPI tasks (good for quick validation).

## Listing test runs

See all test runs:

```sh
cactup test list              # Summary
cactup test list --long       # Extended details
cactup test list --all        # Across all installations
```

## Viewing test results

View a test run summary:

```sh
cactup test show mytest
```

Stream output as it runs:

```sh
cactup test log mytest --follow
```

This opens a live split-pane view of stdout/stderr — see
[Monitoring & Logs](monitoring.html) for the keybindings and the
`--follow-out`/`--follow-err` single-stream variants.

Tail the last 100 lines:

```sh
cactup test log mytest
```

## Test output location

Test output goes to a machine-level `test-home` directory (similar to `simulation-home`). View it:

```sh
cactup machine show          # Shows test-home location
```

Output structure is similar to simulations:

```
test-home/
  mytest/
    results-0000/           # Test results directory
    results-0001/           # If the test was restarted
```

## Debugging test failures

If a test fails:

1. **Check the output**:
   ```sh
   cactup test log mytest
   ```

2. **Look for specific test failures** in the results directory. Test output
   lives under the machine's `test-home` at `<test-home>/mytest/results-NNNN/`
   (run `cactup machine show` to print `test-home`):
   ```sh
   grep -r FAIL <test-home>/mytest/results-0000
   ```
   The flesh harness writes each test's pass/fail summary there.

3. **Re-run with verbose output**:
   ```sh
   cactup --trace test run
   ```

4. **Compare to a known-good build**: If tests pass on a colleague's machine but fail on yours, the machine configuration may differ. See [Cluster Authors](../authors/mdb-overview.html) for how to verify machine configuration.

## Stopping and managing test runs

Stop a test run you submitted to the queue (`test stop` asks the
scheduler to cancel its job):

```sh
cactup test stop mytest
```

A test run you started in the foreground has no queued job: cactup does
not stop it for you (the run's own process number is not something a
scheduler knows). Interrupt it with Ctrl-C in the terminal running it.
`test delete` refuses to remove a test run that is running; for one
you submitted whose job the scheduler lists, `--force` cancels its job
first (where the scheduler refuses, as Torque does for a completed job,
`--force` deletes the run unless its lock is held: it is running, or it
ended without letting go of the lock, which then expires within 15
minutes). A job the
scheduler lists in a state cactup does not know (suspended, exiting, or
ended) is not deleted without `--force`; `test stop` asks the scheduler
to stop it, and once it is over `test delete` goes ahead. One whose lock is held while the scheduler does not list its job
(it runs on another host, it ended without letting go of its lock, which
expires within 15 minutes, or, on a machine that cannot tell, the scheduler
could not be asked) is left as it is, `--force` too.

On a batch system, for 14 days after a test run ended, a job the scheduler
still lists is taken for what it may be: put back in the queue
(preemption, `scontrol requeue`), or just leaving it (Slurm lists an ended
job as completing for a moment, or timed out for a few minutes), which
cactup cannot tell apart. `test list` shows it `QUEUED AGAIN` or `STILL
LISTED`, `test stop` cancels it, `test delete` refuses it without `--force`,
and a new `test run` or `test submit` waits for it to leave (or for `test
stop`). Where the machine definition
says how the scheduler reports a job it does not know, a status query that
fails stops `test stop`, `test delete` and a new `test run` or `test submit`
with the scheduler's output rather than taking the job for ended, and `test
list` shows `UNKNOWN`; for a run that had already finished, the failed query
is only noted. On a machine without a batch system (the generic machine), a
submitted run's job is a background process: cactup asks about it and stops
it only on the host that started it and only while that very process runs,
so a reused process number is never stopped, and only where the process has
a process group of its own (the machine's `submit` starts it with
`setsid`). From another host, `test stop` says the run's job runs on the
host that submitted it and to stop it there, and `test delete` refuses,
while the run holds its lock. A run submitted by an older cactup is not
stopped for you while a process with its number runs (check it and stop it
yourself).

Move a test to trash:

```sh
cactup test delete mytest
```

Permanently delete:

```sh
cactup test delete mytest --force
```

Or purge from trash:

```sh
cactup test delete mytest --purge
```

## Cleaning in-tree test output

The Einstein Toolkit's test harness may leave output in the Cactus source tree (`TEST/` and `configs/<cfg>/TEST`). cactup redirects test output to `test-home`, but to clean up any old in-tree files:

```sh
cactup test clean
```

## Using tests before production

A typical workflow:

1. **Build a new config**:
   ```sh
   cactup build myconfig --optimize
   ```
   (On a machine that requires compute-node builds, this submits and waits
   for a job id rather than compiling immediately — see
   [Building Configs](building-configs.html). Check `cactup build show
   myconfig` before moving on if you're not sure it finished.)

2. **Run the test suite** to validate it:
   ```sh
   cactup test run --config myconfig
   ```

3. **If tests pass**, run your science simulation:
   ```sh
   cactup sim submit mysim mysim.par --config myconfig -n 64 -w 24:00:00
   ```

This catches compiler issues, broken thorns, and configuration problems before large runs waste queue time.

## Full command reference

{{cactup:cli command="test run"}}

{{cactup:cli command="test submit"}}

{{cactup:cli command="test clean"}}

{{cactup:cli command="test list"}}

{{cactup:cli command="test show"}}

{{cactup:cli command="test log"}}

{{cactup:cli command="test stop"}}

{{cactup:cli command="test delete"}}

## Examples

### Quick single-node test of a new build

```sh
cactup build optimized --optimize
cactup test run --config optimized
```

### Test a specific thorn

```sh
cactup test run McLachlan/ML_BSSN
```

### Test on multiple nodes with GPU (submit to queue)

```sh
cactup test submit -n 4 --gpu -w 1:00:00
```

### Test on a specific partition

```sh
cactup test submit -q workq          # the queue picks that partition's test scripts
```

## Troubleshooting

**"Config not found"**: Build the config first with `cactup build myconfig`.

**"Cannot allocate job"**: `test submit` requires running inside an allocation. Try `test run` instead, or request an interactive allocation first.

**"test selection … matches nothing"**: test/arrangement/thorn names come from the built config's thorns (its `ThornList`); double-check the exact `Arrangement`, `Arrangement/Thorn`, `Thorn`, or test name (they are case-sensitive). A thorn without tests cannot be selected either.

**"Permission denied on test-home"**: Check that you have write access to your machine's `test-home` directory (see `cactup machine show`).

## Next steps

- [Running Simulations](running-simulations.html) — run production simulations with validated configs
- [Building Configs](building-configs.html) — build new configs to test
