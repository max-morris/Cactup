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

# 2. Building a configuration and running a first simulation

In this notebook you will

- build a configuration, `tutorial`, from a thornlist,
- see what cactup recorded about it,
- write a parameter file, submit a simulation to the queue, and follow it,
- find its output and plot it.

*Time: about 30 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 2
```

## Building a configuration

A configuration ("config") is one build of Cactus: a set of thorns compiled
with one set of options into one executable. The release you installed in
notebook 1 has over 300 thorns; this tutorial builds a smaller set, the
CarpetX driver with Cottonmouth's Z4c evolution and what they need, listed in
a thornlist. Each part says where its thorns come from, then lists them; here
are CarpetX's:

```{code-cell} ipython3
%show /opt/cactup-tutorial/thornlists/tutorial.th --lines 47:64
```

`cactup build NAME` builds the config `NAME` in the active installation.
`--thornlist` names the thorns to build; without it, cactup builds the
installation's own thornlist, the whole release. The compiler options come
from the machine's entry in the MDB, so there is nothing to choose here.

```{code-cell} ipython3
%%shell
cactup build tutorial --thornlist /opt/cactup-tutorial/thornlists/tutorial.th
```

The output is Cactus's own (scroll up in its box for the start): configure,
the CCL files, then every thorn's sources, the external libraries (AMReX and
NSIMD) first, and finally the link. It took under a minute because this
build was prepared ahead of time for the tutorial: cactup ran for real, but
the build itself was done earlier, and its recorded output played back while
its results were put in place. On a cluster, a first build of these thorns
takes about ten minutes. cactup ran it as a build *attempt* and kept a
record of it, and of the config itself: what it was built from, with which
flags, on which machine.

```{code-cell} ipython3
%%shell
cactup config show tutorial
```

```{code-cell} ipython3
%%shell
cactup build list
cactup build show tutorial
```

(The "job" of a build that ran here in the foreground, as this one did, is
its process id, not a SLURM job; notebook 9 comes back to that.)

The attempt's full output is in its directory (`build.out` and `build.err`),
and `cactup build log tutorial` shows it. Since the build succeeded,
`tutorial` is now the installation's active config, the one simulations use
unless told otherwise:

```{code-cell} ipython3
%%shell
cactup config list
```

## A parameter file

The run: a gravitational wave of tiny amplitude crossing a periodic box
twice, evolved with the Z4c formulation. It is the stock test of
`CottonmouthZ4c4m`, with a longer run and more output. Write it to your home
directory:

```{code-cell} ipython3
%%file ~/linear_wave.par
# A gravitational wave of tiny amplitude crossing a periodic box twice,
# evolved with Cottonmouth's Z4c on CarpetX.
$cells     = 50                  # cells along the wave's direction (x)
$dx        = 1.0 / $cells
$dt        = 0.5 * $dx           # CarpetX::dtfac below
$crossing  = ceil(1.0 / $dt)     # iterations for one crossing of the box

ActiveThorns = "
  ADMBaseX
  CarpetX
  CottonmouthLinearWaveID
  CottonmouthZ4c4m
  IOUtil
  ODESolvers
  TmunuBaseX
"

Cactus::presync_mode = "presync-only"
Cactus::terminate    = "iteration"
Cactus::cctk_itlast  = 2 * $crossing

# A thin periodic box: the wave travels along x.
CarpetX::xmin = -0.5
CarpetX::ymin = -0.5
CarpetX::zmin = -0.5
CarpetX::xmax = 0.5
CarpetX::ymax = 0.5
CarpetX::zmax = 0.5
CarpetX::ncells_x = $cells
CarpetX::ncells_y = 8
CarpetX::ncells_z = 8
CarpetX::blocking_factor_x = 1
CarpetX::blocking_factor_y = 1
CarpetX::blocking_factor_z = 1
CarpetX::ghost_size = 3
CarpetX::periodic   = yes
CarpetX::periodic_x = yes
CarpetX::periodic_y = yes
CarpetX::periodic_z = yes

# Initial data: CottonmouthLinearWaveID sets the ADM variables itself.
ADMBaseX::initial_data    = "none"
ADMBaseX::initial_lapse   = "none"
ADMBaseX::initial_shift   = "none"
ADMBaseX::initial_dtlapse = "none"
ADMBaseX::initial_dtshift = "none"
CottonmouthLinearWaveID::amplitude  = 1.0e-8
CottonmouthLinearWaveID::wavelength = 1.0

CottonmouthZ4c4m::dissipation_epsilon = 0.02
ODESolvers::method = "RK4"
CarpetX::dtfac     = 0.5

# Output: the conformal metric along x ten times per crossing, and the
# constraints' norms every iteration. (out_tsv is for 3D output; the lines
# along the axes come from out_tsv_vars alone.)
IO::out_dir                             = $parfile
IO::out_every                           = $crossing / 10
CarpetX::out_tsv                        = no
CarpetX::out_tsv_output_boundary_points = no
CarpetX::out_tsv_vars                   = "CottonmouthZ4c4m::gt"
CarpetX::out_norm_vars                  = "CottonmouthZ4c4m::HamCons CottonmouthZ4c4m::MomCons"
CarpetX::out_norm_every                 = 1
CarpetX::out_metadata                   = no
CarpetX::out_performance                = no
```

## Submitting a simulation

`cactup sim submit NAME PARFILE` creates a simulation from the parameter file
and the active config, and submits it to the queue. The flags say how to
run it:

- `-T 2`: two MPI processes (tasks),
- `-c 2`: two threads (CPUs) each, so the run uses all four of the machine's
  CPUs,
- `-w 00:10:00`: at most ten minutes (the run takes a minute or two).

No queue is named, so it goes to the machine's default queue, `debug`.
(`--overwrite` replaces the simulation if you run this cell again. Let the
earlier run finish first: replacing a simulation doesn't stop its job.)

```{code-cell} ipython3
%%shell
cactup sim submit lw ~/linear_wave.par -T 2 -c 2 -w 00:10:00 --overwrite
```

cactup wrote a submit script from the machine's template and handed it to
SLURM. `cactup sim list` shows your simulations and their state; `squeue`,
SLURM's own view, shows the job:

```{code-cell} ipython3
%%shell
cactup sim list
squeue
```

The next cell waits until the job has left the queue, then shows the
simulation. (On a real cluster you would rather come back later.)

```{code-cell} ipython3
%%shell
while [ -n "$(squeue -h -u "$USER")" ]; do sleep 5; done
cactup sim show lw
```

`cactup sim log` shows the end of the run's standard output, then of its
standard error. CarpetX reports its performance after every iteration, so
that is most of the output; near its end the run script says the run is done
("Done."), and the standard error is the run script's trace of its commands:

```{code-cell} ipython3
%%shell
cactup sim log lw
```

## The output

Each run of a simulation is a *restart*, `output-0000` for the first, with
the run's output in a directory named after the parameter file. cactup tells
you where:

```{code-cell} ipython3
%%shell
cactup sim show lw --output-dir
ls $(cactup sim show lw --output-dir)/linear_wave | sed -n 1,5p
ls $(cactup sim show lw --output-dir)/linear_wave/norms
```

CarpetX writes plain tab-separated files, one per group of variables,
iteration and direction for the 1D output, and one per group for the norms. The first
line names the columns. Read them with pandas:

```{code-cell} ipython3
import subprocess
from pathlib import Path

import matplotlib.pyplot as plt
import pandas as pd

out = Path(subprocess.run(["cactup", "sim", "show", "lw", "--output-dir"],
                          capture_output=True, text=True, check=True).stdout.strip()) / "linear_wave"


def read_tsv(path):
    """A CarpetX TSV file, its columns named from its header line."""
    with open(path) as f:
        header = f.readline().lstrip("# ").split("\t")
    names = [column.split(":", 1)[1].strip() for column in header]
    return pd.read_csv(path, sep="\t", comment="#", names=names)


gt = {int(p.name.split(".it")[1][:6]): read_tsv(p)
      for p in sorted(out.glob("cottonmouthz4c4m-gt.it*.x.tsv"))}
print(f"{len(gt)} snapshots of gt along x, iterations {min(gt)} to {max(gt)}")
gt[0].head()
```

The wave is a small perturbation of flat space in the transverse components
of the metric: `gtDD11` (the yy component) is 1 plus a sine of amplitude
1e-8 traveling along x at the speed of light, 1 in these units. The box is 1
long, so after each crossing (t = 1, 2) the wave should be back where it
started (dashed; the t = 0 curve is right under them):

```{code-cell} ipython3
fig, ax = plt.subplots(figsize=(8, 4))
for it in (0, 20, 40, 100, 200):
    ax.plot(gt[it]["x"], gt[it]["gtDD11"] - 1, "--" if it >= 100 else "-",
            label=f"t = {gt[it]['time'].iloc[0]:.1f}")
ax.set_xlabel("x")
ax.set_ylabel(r"$\tilde\gamma_{yy} - 1$")
ax.legend()
ax.set_title("A linear wave crossing a periodic box")
plt.show()
```

The Hamiltonian constraint vanishes for an exact solution of Einstein's
equations, so its norm shows how well the run keeps to one. At this
amplitude it stays tiny:

```{code-cell} ipython3
ham = read_tsv(out / "norms" / "cottonmouthz4c4m-hamcons.tsv")
fig, ax = plt.subplots(figsize=(8, 4))
ax.semilogy(ham["time"], ham["cottonmouthz4c4m::hamcons.L2norm"])
ax.set_xlabel("t")
ax.set_ylabel("L2 norm of the Hamiltonian constraint")
plt.show()
```

## Where this is documented

- [Building configurations](https://max-morris.github.io/Cactup/users/building-configs.html)
- [Running simulations](https://max-morris.github.io/Cactup/users/running-simulations.html)
- [Monitoring](https://max-morris.github.io/Cactup/users/monitoring.html)

Next: **notebook 3**, a second installation, pointed at forks of the flesh
and CarpetX.
