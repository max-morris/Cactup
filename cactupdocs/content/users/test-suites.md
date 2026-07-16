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
cactup test run some-test-name    # A specific test
```

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

Override the runscript variant:

```sh
cactup test run --variant default    # Use a specific variant
```

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

Pass `--config`, `--variant`, and `--universe` like with `test run`.

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

2. **Look for specific test failures**:
   ```sh
   TESTDIR=$(cactup test show mytest --output-dir)
   grep -r FAIL $TESTDIR
   ```

3. **Re-run with verbose output**:
   ```sh
   cactup --trace test run
   ```

4. **Compare to a known-good build**: If tests pass on a colleague's machine but fail on yours, the machine configuration may differ. See [Cluster Authors](../authors/mdb-overview.html) for how to verify machine configuration.

## Stopping and managing test runs

Stop a queued or running test:

```sh
cactup test stop mytest          # Graceful stop
cactup test stop mytest --force  # Kill immediately
```

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

### Test with a custom runscript variant

```sh
cactup test run --variant cuda --gpu
```

## Troubleshooting

**"Config not found"**: Build the config first with `cactup build myconfig`.

**"Cannot allocate job"**: `test submit` requires running inside an allocation. Try `test run` instead, or request an interactive allocation first.

**"Test not found"**: List available tests with `cactup test run --help`. Check the exact test/arrangement/thorn name.

**"Permission denied on test-home"**: Check that you have write access to your machine's `test-home` directory (see `cactup machine show`).

## Next steps

- [Running Simulations](running-simulations.html) — run production simulations with validated configs
- [Building Configs](building-configs.html) — build new configs to test
