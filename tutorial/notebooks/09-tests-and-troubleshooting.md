---
jupytext:
  text_representation:
    extension: .md
    format_name: myst
kernelspec:
  display_name: Python 3
  language: python
  name: python3
---

# 9. Test suites, and when things fail

In this notebook you will

- run thorns' test suites, and find out why some tests fail,
- break a thorn on purpose, and find the error as you would in real life:
  the build's output, its log, and what cactup ran,
- fix it, and put the thorn back,
- read what a run with a broken parameter file left behind.

*Time: about 30 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 9
```

## Test suites

Most thorns come with tests: parameter files in their `test/` directory,
each with the output that it should produce. Cactus's test harness runs each one
and compares. `cactup test` runs the harness for a config, on a compute
node like any job. This machine won't run it in the foreground, where you
are (the tests start processes with `mpirun`, which belongs inside a job):

```{code-cell} ipython3
%%shell --expect-fail
cactup test run WaveToyX -T 1
```

`cactup test submit` sends it to SLURM. Name the tests to run by thorn
(`WaveToyX`, or `CarpetX/WaveToyX`), by thorn and test
(`WaveToyX/radiative`), or by arrangement (`CarpetX`, every thorn in it
that has tests); a name that selects nothing is refused before anything is
queued. Three thorns, on one process. (`test submit` has no `--block`, so
the cell waits until SLURM's queue has no job of yours left.)

```{code-cell} ipython3
%%shell
cactup test submit WaveToyX TestNorms TestOutput -T 1 -w 00:10:00
while squeue -h --me | grep -q .; do sleep 2; done
cactup test show tutorial
```

A test run is named after its config (`tutorial`); each submit adds a
*result set* (`results-NNNN`) to it. Three tests failed. `cactup test log
tutorial` shows the end of the harness's own output, whose summary names
them; the harness also keeps each test's log in the result set. TestOutput's
three are `checkpoint-openpmd`, `output-openpmd` and `recover-openpmd`, and
their logs say why:

```{code-cell} ipython3
%%shell
results=$(ls -d ~/.cactup/tests/MyCactus/tutorial/tutorial/results-[0-9][0-9][0-9][0-9] | tail -n 1)
ls $results/tutorial/TestOutput
grep -a -h -o -E '[A-Za-z:_]+ is set to "openpmd", but openPMD_api is not enabled|openPMD is not enabled' $results/tutorial/TestOutput/*.log | sort -u
```

openPMD isn't in this config's thornlist (notebook 7's `tutorial-ckpt` added
Silo, not openPMD): not a bug in CarpetX, but a test that this config can't
run.

## A broken build

Next, break a thorn: TestNorms, one small source file. The next cell takes
the semicolon off the end of a line. (Like notebook 5's edits, this file is
put back by the end of this notebook, or by the first cell of any later
notebook, which saves your version in `~/tutorial-saved/`.)

```{code-cell} ipython3
from pathlib import Path

test_cc = Path("~/Cactus/arrangements/CarpetX/TestNorms/src/test.cc").expanduser()
text = test_cc.read_text()
test_cc.write_text(text.replace("  const int order = 3; //", "  const int order = 3 //", 1));
```

```{code-cell} ipython3
%%shell --expect-fail
cactup build tutorial
```

The compiler's error is near the end of the box: `test.cc:52:3: error:
expected ‘,’ or ‘;’ before ‘Loop’`. Line 52, not 48, where the semicolon
is missing: a compiler notices a missing semicolon where the next statement
begins. The second error, at line 54, follows from the first. The last lines
name the build's two log files.

cactup keeps every build *attempt*, with its output and the script that it ran:

```{code-cell} ipython3
%%shell
cactup build show tutorial
```

`FAILED` is this attempt; the list is every attempt that this config has
had, the earlier ones from earlier notebooks, or from catch-up cells. (For a
build in the foreground, as here, the `job-id` is cactup's process id.)

`cactup build log tutorial` shows the end of the latest attempt's output;
`-e` follows its error stream, which is where the compiler's errors are
(`--timeout` stops the cell):

```{code-cell} ipython3
%%shell --timeout 8
cactup build log tutorial -e
```

To see what cactup itself runs, `--trace` prints each command before it
runs it (`-v` adds a little more about what cactup decides). The build
fails again (the cell shows only the traced line that runs the build
script), and the build script is the whole recipe: configure, then `make`:

```{code-cell} ipython3
%%shell
cactup --trace build tutorial 2>&1 | grep -a 'build-script'
cat "$(ls -d ~/Cactus/configs/tutorial/.cactup-builds/[0-9][0-9][0-9][0-9] | tail -n 1)/build-script"
```

### The fix

Put the semicolon back. Here the fix is written a little differently from
the original (`constexpr`, which a constant like this can be), so that it
is an edit of its own, which `git diff` shows and the build names. The
original text would be exactly that of the last build that worked: cactup
would still rebuild, because the file was written after that build, but it
would only say `thorn sources were edited`, and `git diff` would show
nothing.

```{code-cell} ipython3
text = test_cc.read_text()
test_cc.write_text(text.replace("  const int order = 3 //", "  constexpr int order = 3; //", 1));
```

```{code-cell} ipython3
%%shell
git -C ~/Cactus/repos/CarpetX diff
```

The diff is the one line, 48, now with `constexpr` and its semicolon.

```{code-cell} ipython3
%%shell
cactup build tutorial
```

One file compiles (`COMPILING CarpetX/TestNorms/src/test.cc`, near the end
of the box). Its test passes again:

```{code-cell} ipython3
%%shell
cactup test submit TestNorms -T 1 -w 00:10:00
while squeue -h --me | grep -q .; do sleep 2; done
cactup test show tutorial
```

Put the file back as it was fetched, and build once more, so that later
notebooks find the thorn as it was:

```{code-cell} ipython3
%%shell
git -C ~/Cactus/repos/CarpetX checkout -- TestNorms/src/test.cc
cactup build tutorial
cactup config delta
```

## A broken parameter file

A misspelled parameter name, in a copy of WaveToyX's test:

```{code-cell} ipython3
%%shell --expect-fail
sed 's/^WaveToyX::initial_condition/WaveToyX::initial_conditon/' ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par > ~/typo.par
cactup sim submit typo ~/typo.par -T 1 -w 00:05:00 --overwrite --block
```

`--block` read how the run ended from what the run recorded, and fails the
command, as a script that runs after the simulation would want. `sim show`
is another matter:

```{code-cell} ipython3
%%shell
cactup sim show typo
```

`FINISHED` means the job is over, not that Cactus succeeded: `sim show`
shows the job's state. What happened is in the run's output, which `cactup
sim log` shows. Cactus checks every parameter before it starts, and stops if
any is wrong:

```{code-cell} ipython3
%%shell
cactup sim log typo | grep -a -E 'not found|major error'
grep failed ~/.cactup/simulations/MyCactus/tutorial/typo/log.txt
```

Cactus prints its warnings to both its output and its error stream, and
`sim log` shows both, so each appears twice. `log.txt` does record the
failure, with Cactus's exit status; `sim show` reports only the job's state.
The line number is the restart's copy's, `output-0000/typo.par`. A value out
of range gives `Range error setting parameter` instead.

## Cleaning up

```{code-cell} ipython3
%%shell
cactup sim delete typo
rm -f ~/typo.par
```

## Where this is documented

- [Test suites](https://max-morris.github.io/Cactup/users/test-suites.html)
- [Building configurations](https://max-morris.github.io/Cactup/users/building-configs.html)
  (build attempts, `build log`, troubleshooting)
- [Monitoring](https://max-morris.github.io/Cactup/users/monitoring.html)
  (`sim log`)

Next: **notebook 10**, from SimFactory to cactup.
