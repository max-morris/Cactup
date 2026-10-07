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

# 8. Parameter files: cactup's variables, Python, and a sweep

In this notebook you will

- let cactup fill in values in a parameter file: the simulation's name, the
  run's processes, a setting of yours,
- write a parameter file as a Python program,
- run the same simulation at three resolutions from one cell, and plot the
  three together.

*Time: about 25 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 8
```

## cactup's variables in a parameter file

When a restart starts, cactup copies the simulation's parameter file into
the restart's directory (`output-NNNN/`), replacing every `@NAME@` in it
with that variable's value. These are the same variables the submit and
run scripts use (notebook 6b), among them:

| Variable | Value |
|---|---|
| `@SIMULATION_NAME@`, `@RESTART_ID@` | the simulation's name; 0 for `output-0000`, and so on |
| `@RUNDIR@` | the restart's directory |
| `@SIMULATION_DIR@` | the simulation's directory, which all its restarts share (notebook 7 kept checkpoints there) |
| `@TASKS@`, `@CPUS_PER_TASK@` | the run's processes, and threads per process |
| `@HOSTNAME@` | the machine you submitted from |
| `@KNOB(name)@` | a knob's value: refused if the knob is unset |
| `@KNOB-OPTIONAL(name, default)@` | a knob's value, or the default |
| `@ENV(NAME)@` | an environment variable, as the job sees it on the compute node |

`@@` is a literal `@`. cactup checks the tokens when you submit (except
`@ENV(...)@`, which only the job can know), so a misspelled one is refused
then, not when the job starts.

A standing wave in a periodic box, evolved by WaveToyX for one period.
WaveToyX knows the exact solution, and writes out how far the numerical one
is from it. The resolution comes from a knob of your own, `cells`:

```{code-cell} ipython3
%%file ~/standing.par
# A standing wave in a periodic box, evolved by WaveToyX for one period
# (t = 1.155). WaveToyX knows the exact solution and outputs the numerical
# error; the resolution comes from the knob "cells" (-K cells=N).
ActiveThorns = "
  CarpetX
  IOUtil
  ODESolvers
  WaveToyX
"

Cactus::presync_mode = "mixed-error"
Cactus::cctk_run_title = "@SIMULATION_NAME@: @KNOB(cells)@ cells, restart @RESTART_ID@"

CarpetX::ncells_x = @KNOB(cells)@
CarpetX::ncells_y = @KNOB(cells)@
CarpetX::ncells_z = @KNOB(cells)@
CarpetX::periodic   = yes
CarpetX::periodic_x = yes
CarpetX::periodic_y = yes
CarpetX::periodic_z = yes

WaveToyX::initial_condition = "standing wave"
ODESolvers::method = "RK4"

Cactus::terminate       = "time"
Cactus::cctk_final_time = 1.155

IO::out_dir   = $parfile
CarpetX::out_norm_vars  = "WaveToyX::error"
CarpetX::out_norm_every = 2
CarpetX::out_metadata    = no
CarpetX::out_performance = no
```

Submitted without a value for `cells`, it is refused (cactup has made the
simulation by then, so `--overwrite` replaces it next time):

```{code-cell} ipython3
%%shell --expect-fail
while squeue -h -n standing-16 | grep -q .; do sleep 1; done
cactup sim submit standing-16 ~/standing.par -T 1 -c 1 -w 00:05:00 --overwrite
```

`-K cells=16` gives it one, for this command (notebook 6b): one process,
one thread, a few seconds.

```{code-cell} ipython3
%%shell
cactup sim submit standing-16 ~/standing.par -K cells=16 -T 1 -c 1 -w 00:05:00 --overwrite
while squeue -h -n standing-16 | grep -q .; do sleep 1; done
grep -E 'run_title|ncells_x' ~/.cactup/simulations/ET_2026_05_v0/tutorial/standing-16/output-0000/standing.par
```

That is the restart's copy, with the values filled in; the simulation keeps
the original in `.cactup/par/`.

## A parameter file written in Python

A parameter file can also be a Python program, a `.py` file: cactup runs
it when the restart starts, and what it prints is the parameter file. It
gets the same variables, as Python variables (strings; `typed` has the
numbers as numbers), and `knob(name, default)`. This one computes the
period from the wave number, and says who it was written for:

```{code-cell} ipython3
%%file ~/standing.py
# A computed parameter file: cactup runs it with python3 when the restart
# starts, and what it prints is the parameter file.
import math

cells = int(knob("cells", "32"))
k = 0.5                                # wave number along each axis
period = 1 / (math.sqrt(3) * k)        # u ~ cos(2 pi omega t), omega = |k|

print(f"""\
# Generated for {SIMULATION_NAME}, restart {RESTART_ID}, on {typed['TASKS']} process(es).
ActiveThorns = "CarpetX IOUtil ODESolvers WaveToyX"

Cactus::presync_mode = "mixed-error"
Cactus::terminate       = "time"
Cactus::cctk_final_time = {period:.6f}     # one period

CarpetX::ncells_x = {cells}
CarpetX::ncells_y = {cells}
CarpetX::ncells_z = {cells}
CarpetX::periodic   = yes
CarpetX::periodic_x = yes
CarpetX::periodic_y = yes
CarpetX::periodic_z = yes

WaveToyX::initial_condition = "standing wave"
WaveToyX::standing_wave_kx = {k}
WaveToyX::standing_wave_ky = {k}
WaveToyX::standing_wave_kz = {k}
ODESolvers::method = "RK4"

IO::out_dir = $parfile
CarpetX::out_norm_vars   = "WaveToyX::error"
CarpetX::out_norm_every  = 2
CarpetX::out_metadata    = no
CarpetX::out_performance = no
""")
```

```{code-cell} ipython3
%%shell
while squeue -h -n computed | grep -q .; do sleep 1; done
cactup sim submit computed ~/standing.py -K cells=24 -T 1 -c 1 -w 00:05:00 --overwrite
while squeue -h -n computed | grep -q .; do sleep 1; done
grep -E '^# Generated|cctk_final_time|ncells_x' ~/.cactup/simulations/ET_2026_05_v0/tutorial/computed/output-0000/standing.par
```

Unlike a `.par`, a `.py` isn't checked when you submit: cactup runs it only
when the restart starts, on the compute node. A mistake in it shows in the
run's error stream (`cactup sim log NAME`), with the program's traceback.

## A sweep

The same simulation at 16³, 32³ and 64³ cells: one cell submits all three,
with one process and one thread each, so that the node's four CPUs run them
side by side.

```{code-cell} ipython3
%%shell
for n in 16 32 64; do
  while squeue -h -n standing-$n | grep -q .; do sleep 1; done
  cactup sim submit standing-$n ~/standing.par -K cells=$n -T 1 -c 1 -w 00:10:00 --overwrite
done
squeue
while squeue -h -n standing-16,standing-32,standing-64 | grep -q .; do sleep 2; done
cactup sim list
```

Right after submitting, `squeue` still shows them pending; they start within
seconds. `sim list` lists every simulation in this installation, so
notebook 2's `lw` too, if you ran it.

Each simulation's error, over the one period. `read_tsv` is notebook 2's,
and `output_dir` notebook 7's without a restart number: `cactup sim show
NAME --output-dir` says where a simulation's output is (its current
restart's `output-NNNN`).

```{code-cell} ipython3
import subprocess
from pathlib import Path

import matplotlib.pyplot as plt
import pandas as pd


def read_tsv(path):
    """A CarpetX TSV file, its columns named from its header line."""
    with open(path) as f:
        header = f.readline().lstrip("# ").split("\t")
    names = [column.split(":", 1)[1].strip() for column in header]
    return pd.read_csv(path, sep="\t", comment="#", names=names)


def output_dir(sim):
    """A simulation's output directory, as cactup reports it."""
    cmd = ["cactup", "sim", "show", sim, "--output-dir"]
    return Path(subprocess.run(cmd, capture_output=True, text=True, check=True).stdout.strip())


fig, ax = plt.subplots(figsize=(8, 4))
errors = {}
for n in (16, 32, 64):
    err = read_tsv(output_dir(f"standing-{n}") / "standing" / "norms" / "wavetoyx-error.tsv")
    err = err[err["time"] > 0]  # the error starts at 0, which a log axis can't show
    ax.semilogy(err["time"], err["wavetoyx::u_err.L2norm"], label=f"{n}³ cells")
    at = err.iloc[(err["time"] - 1.0).abs().argmin()]
    errors[n] = at["wavetoyx::u_err.L2norm"]
    print(f"{n:3d}³ cells: L2 error {errors[n]:.3e} at t = {at['time']:.3f}")
print(f"ratios: {errors[16] / errors[32]:.2f}, {errors[32] / errors[64]:.2f}")
ax.set_xlabel("t")
ax.set_ylabel("L2 norm of the error in u")
ax.set_title("A standing wave at three resolutions")
ax.legend()
plt.show()
```

Each doubling of the resolution divides the error by about four: WaveToyX's
derivatives are second-order accurate.

## Cleaning up

```{code-cell} ipython3
%%shell
for sim in standing-16 standing-32 standing-64 computed; do cactup sim delete $sim; done
rm -f ~/standing.par ~/standing.py
```

## Where this is documented

- [Running simulations](https://max-morris.github.io/Cactup/users/running-simulations.html)
  (parameter file substitution, knobs and `-K`)
- [Submit and run scripts](https://max-morris.github.io/Cactup/authors/scripts-and-variables.html)
  (every variable, and the Python form)

Next: **notebook 9**, test suites, and what to do when a build or a run
fails.
