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

# 4b. Several configurations of one installation

In this notebook you will

- build a GPU configuration from the machine's GPU optionlist,
- see how cactup keeps GPU executables off CPU queues,
- build a debug configuration, and see the flags a config records,
- list, switch and delete configurations, and see their build attempts.

*Time: about 20 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 4b
```

## Optionlist variants

How Cactus is compiled (compilers, flags, where the libraries are) is the
*optionlist*. A machine's entry in the MDB can offer several, as *variants*.
This machine has two:

```{code-cell} ipython3
%%shell
cactup machine show --variants
```

`gpu` is a CUDA build: `nvcc` compiles CarpetX's and AMReX's kernels for
NVIDIA GPUs. Its `[cactup]` header (`config show` prints what came of it,
below) says what it is for: `gpu = true`, and the queues its executables
may run on. This machine has no GPU and no CUDA toolkit of
its own, much like many clusters' login nodes; the GPU build below was prepared ahead of time on a machine with the toolkit the
optionlist describes, and this machine replays it.

One more thing a GPU build needs here: the release's CarpetX doesn't compile
with CUDA 13's `nvcc` (three assertions trip over CUDA's own `all()` and
`any()`). A branch of a CarpetX fork carries the release plus that one fix,
and a thornlist for GPU builds points CarpetX at it:

```{code-cell} ipython3
%show /opt/cactup-tutorial/thornlists/tutorial-gpu.th --lines 1:12
```

So GPU builds get an installation of their own, from that thornlist (as
notebook 3 made `et-mp`), and the stock installation stays the release:

```{code-cell} ipython3
%%shell
cactup install --thornlist /opt/cactup-tutorial/thornlists/tutorial-gpu.th --alias et-gpu --symlink-name et-gpu --silent
```

A config is built with one variant; `--variant gpu` picks this one. `-I
et-gpu` builds it in the new installation without switching to it:

```{code-cell} ipython3
%%shell
cactup -I et-gpu build tutorial-gpu --variant gpu
```

```{code-cell} ipython3
%%shell
cactup -I et-gpu config show tutorial-gpu
```

## GPU executables on GPU queues

`gpu: true` and `compatible-queues: gpu` came from the variant's header, and
cactup holds simulations to them. Submit to the default queue, a CPU
partition, and cactup refuses. (It creates the simulation before checking
the queue, so try it with a throwaway name.)

```{code-cell} ipython3
%%shell --expect-fail
cactup -I et-gpu sim submit gpu-try ~/et-gpu/arrangements/Cottonmouth/CottonmouthZ4c4m/test/linear_wave_z4c.par --config tutorial-gpu
```

`--force-queue` would override the check: for an executable that doesn't
need a GPU after all, or a queue the machine's entry gets wrong. Here it
would only start a GPU program on a node without one. Delete the throwaway
simulation:

```{code-cell} ipython3
%%shell
cactup -I et-gpu sim delete gpu-try
```

The right way is a GPU queue, `-q gpu`. This machine has a `gpu` partition,
but it is *inactive*, since there is no GPU behind it:

```{code-cell} ipython3
%%shell
sinfo
```

So this time SLURM refuses the job, not cactup. (This submit leaves out
`--config`: `tutorial-gpu` is the only config `et-gpu` has, so it is that
installation's *active* config, the one `sim` commands use by default. More
on that below.)

```{code-cell} ipython3
%%shell --expect-fail
cactup -I et-gpu sim submit gpu-try ~/et-gpu/arrangements/Cottonmouth/CottonmouthZ4c4m/test/linear_wave_z4c.par -q gpu
```

cactup created the simulation and its first restart before handing the
job to SLURM, so they exist without a job: `sim list` shows the simulation
ACTIVE, with nothing queued or running behind it. (`sim delete`
moves a simulation to a `TRASH` directory beside the others, where you can
still look at it.)

```{code-cell} ipython3
%%shell
cactup -I et-gpu sim list
cactup -I et-gpu sim delete gpu-try
```

## A debug build

Back in the stock installation: `--debug` builds with Cactus's debug
settings: full debugging information (`-g3`) and the extra checks thorns
compile in when debugging (Cactus defines `CCTK_DEBUG`, and CarpetX's
array and loop code checks its indices when it is set), for a debugger or a
bug the checks can catch. Optimization is a flag of its own (`optimize`):
it is always on, debug builds included (`-O2` with `-g3` here), and cactup
has no switch to turn it off. These are *build flags*: a config records the ones it was
built with (`config show` lists them), and later builds of it reuse them, so
a plain `cactup build tutorial-debug` stays a debug build. Build a debug
config of its own, so the plain one stays as it is:

```{code-cell} ipython3
%%shell
cactup build tutorial-debug --debug --thornlist /opt/cactup-tutorial/thornlists/tutorial.th
cactup config show tutorial-debug
```

A flag on the command line can only turn a flag on, never off. And turning
`--debug` on for an existing optimized config is no shortcut: a build that
is up to date stops before it looks at new flags. Give the debug build its
own name, as here.

## Managing configurations

An installation's *active* config is the one `sim` commands use without
`--config`. A build makes its config the active one only when none is
active yet: `tutorial-gpu` became `et-gpu`'s, while the stock installation
kept `tutorial`. `cactup config use` switches:

```{code-cell} ipython3
%%shell
cactup config list
cactup config use tutorial-debug
cactup config use tutorial
```

Every build is an *attempt*, kept with its output under the config. `cactup
build list` shows the latest attempt of each config; `cactup build prune
NAME` removes old attempts once they pile up, keeping the most recent ten
(or `--keep N`):

```{code-cell} ipython3
%%shell
cactup build list
```

A config you no longer need goes with `cactup config delete`. It refuses
while simulations or tests still use the config (`--force` deletes it
anyway); nothing uses `tutorial-debug`:

```{code-cell} ipython3
%%shell
cactup config delete tutorial-debug
cactup config list
```

## Where this is documented

- [Building configurations](https://max-morris.github.io/Cactup/users/building-configs.html)
  (variants, flags, `config`)
- [Monitoring](https://max-morris.github.io/Cactup/users/monitoring.html)
  (`build list`, `build prune`)
- [Optionlists](https://max-morris.github.io/Cactup/authors/optionlists.html)
  (variants and their `[cactup]` header)
- [Running simulations](https://max-morris.github.io/Cactup/users/running-simulations.html)
  (queues, `--force-queue`)

Next: **notebook 5**, topology, the delta checker while hacking on thorns,
and universes.
