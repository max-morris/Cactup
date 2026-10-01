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

# 5. Shaping runs, universes, and hacking on thorns

In this notebook you will

- shape a run with the topology options, and see the batch script they make,
- build a config inside a *universe*, a wrapper around everything the
  config runs,
- edit a thorn, a thorn's parameters and the Cactus flesh, and watch what
  cactup rebuilds each time,
- put the source tree back.

*Time: about 40 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 5
```

## The shape of a run

A simulation's *topology* is how many MPI processes it runs and how many
threads each one gets, on how many nodes. `sim submit` and `sim run` take
it as options:

| Option | Meaning | Default here |
|---|---|---|
| `-n` | nodes | 1 |
| `-T` | MPI processes (*tasks*) in all | nodes × tasks per node |
| `-t` | tasks per node | as many as fit: 4 CPUs ÷ CPUs per task |
| `-c` | CPUs (OpenMP threads) per task | 1 |
| `-g`, `-G` | use GPUs, and GPUs per task | GPU queues only (notebook 4b) |
| `-w` | walltime | the queue's limit (30 minutes on `debug`) |
| `-q` | queue | `debug` |

WaveToyX, a small CarpetX example thorn, has a test whose run takes a
second, so it is good for trying things out. Submit it with the defaults.
(`--overwrite` replaces a simulation of the same name, if there is one, so
that the cells of this notebook can be run again. Replacing one whose job is
still queued or running would leave that job behind, so a cell that submits
first waits until no job of that name is in SLURM's queue: `squeue -n NAME`
lists them.)

```{code-cell} ipython3
%%shell
while squeue -h -n shape-4x1 | grep -q .; do sleep 1; done
cactup sim submit shape-4x1 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par -w 00:05:00 --overwrite
```

cactup turned the topology into the batch script it handed to `sbatch`.
Each restart keeps its own copy, beside its output:

```{code-cell} ipython3
%show ~/.cactup/simulations/ET_2026_05_v0/tutorial/shape-4x1/output-0000/.cactup/submit-script
```

With no topology options, the run fills the node: four tasks of one CPU
each. The last line runs cactup itself on the compute node, which starts
Cactus under `mpirun` in the restart's directory (`output-0000`).

Two tasks of two threads each, still filling the node's four CPUs:

```{code-cell} ipython3
%%shell
while squeue -h -n shape-2x2 | grep -q .; do sleep 1; done
cactup sim submit shape-2x2 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par -T 2 -c 2 -w 00:05:00 --overwrite
```

```{code-cell} ipython3
%show ~/.cactup/simulations/ET_2026_05_v0/tutorial/shape-2x2/output-0000/.cactup/submit-script --lines 1:9
```

The machine's entry decides what makes sense here. This one is a single
node, and its submit-script template (`default.py`, which the error names)
refuses a run on more nodes before anything reaches SLURM. (cactup creates
the simulation first, so try it with a throwaway name.)

```{code-cell} ipython3
%%shell --expect-fail
cactup sim submit shape-2n ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par -n 2 -w 00:05:00 --overwrite
```

A `-w` longer than the queue allows (`debug` allows 30 minutes) isn't an
error: cactup splits the run into a chain of jobs, each continuing from
the last one's checkpoint, if the parameter file checkpoints and recovers.
Notebook 7 does that.

Each run takes a second once SLURM starts it. The next cell waits until
neither job is in SLURM's queue any more, then picks out the lines where
Cactus says how it was started: how many MPI processes, and how many
OpenMP threads each.

```{code-cell} ipython3
%%shell
while squeue -h -n shape-4x1,shape-2x2 | grep -q .; do sleep 1; done
cactup sim list
(cd ~/.cactup/simulations/ET_2026_05_v0/tutorial &&
 grep -E 'MPI processes|OMP threads' shape-4x1/output-0000/shape-4x1.out shape-2x2/output-0000/shape-2x2.out)
```

`sim list` shows both runs FINISHED, and `shape-2n` INACTIVE: cactup
created it but never submitted it. (Simulations from earlier notebooks,
such as notebook 2's `lw`, are listed too.)

Delete the three simulations:

```{code-cell} ipython3
%%shell
cactup sim delete shape-4x1
cactup sim delete shape-2x2
cactup sim delete shape-2n
```

## Universes

A *universe* wraps the commands cactup runs for a config: its build and,
unless the optionlist says otherwise, its simulations. Clusters use them to
build and run inside a container image (Singularity or Apptainer) or a
shell with modules loaded. This machine has one universe, `pinned`, which
runs everything under `taskset` (which keeps a process on a given set of
CPUs), restricted to this container's CPUs: a simple stand-in for a
container image, to see what wrapping does.

```{code-cell} ipython3
%%shell
cactup machine show | grep universes
```

A config's universe is chosen when it is built. Build one in `pinned`:
`tutorial-pinned`, from the same thornlist as `tutorial`. The first command,
`cactup-tutorial-sources-clean`, is the tutorial's, not cactup's: it checks
that the stock installation's sources are as fetched (the fetched commits,
no edits, no extra files in its thorns), because this build was prepared
ahead of time from them (it takes about a minute). Notebook 4b covered the
other way to vary a build, optionlist variants.

```{code-cell} ipython3
%%shell
cactup-tutorial-sources-clean && cactup build tutorial-pinned --universe pinned --thornlist /opt/cactup-tutorial/thornlists/tutorial.th
```

```{code-cell} ipython3
%%shell
cactup config show tutorial-pinned
```

`universe: pinned` is recorded with the config, and its simulations run in
it (`coerce-run-universe: true`), unless a submit says `--universe` or
`--no-universe`. Each restart records the universe it runs in. (`-T 1`:
one process is plenty for a one-second test.)

```{code-cell} ipython3
%%shell
while squeue -h -n wave-pinned | grep -q .; do sleep 1; done
cactup sim submit wave-pinned ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --config tutorial-pinned -T 1 -w 00:05:00 --overwrite
sed -n '/^\[universe\]/,/^\]/p' ~/.cactup/simulations/ET_2026_05_v0/tutorial-pinned/wave-pinned/output-0000/.cactup/restart.toml
```

`@ENV(NAME)@` is filled in from the environment when the job starts; here
`CACTUP_TUTORIAL_CPUS` holds this container's CPUs, so the run is pinned to
them.

One thing to know: unlike the thornlist, the variant and the build flags, a
config's universe is not reused by later builds. Every build works out its
universe again, from `--universe`, or else the optionlist or the machine's
default, and a universe
other than the one the config was built in means a rebuild from scratch.
So build `tutorial-pinned` with `--universe pinned` every time.

## Hacking on a thorn

Now the everyday loop: edit a thorn, rebuild, run. cactup tracks the
source tree each config was built from, so a plain `cactup build` sees
your edits and rebuilds only what they affect.

You will edit three files in the stock installation. The cells below make
each edit, so the notebook runs from top to bottom. Open the files in the
editor (the file browser on the left, under `Cactus/`) to see them, or make
an edit yourself instead of running its cell; type it exactly as the cell
would, since the later cells look for that text. Before you leave this
notebook, its last section puts the files back. If you don't get there, the
first cell of any later notebook (and of this one, if you run it again)
does it for you, and saves your versions in `~/tutorial-saved/`.
Close the files' editor tabs when you finish: an open tab would offer to
save its old text over the restored file.

Editors can leave files of their own next to the ones you open: stock
JupyterLab keeps checkpoints in `.ipynb_checkpoints/` (this one keeps them
elsewhere), and vim keeps swap files. They aren't part of a thorn, and
the global ignore file tells cactup to skip them (a repository can have a
`.cactupignore` of its own, too):

```{code-cell} ipython3
%show ~/.cactup/cactupignore
```

### A thorn's code

The edit: WaveToyX says hello when it sets up its initial data.

```{code-cell} ipython3
# Insert the hello line after the declarations; does nothing if it's already there.
from pathlib import Path

wavetoyx = Path("~/Cactus/arrangements/CarpetX/WaveToyX/src/wavetoyx.cxx").expanduser()
text = wavetoyx.read_text()
after = "  DECLARE_CCTK_ARGUMENTSX_WaveToyX_Initial;\n  DECLARE_CCTK_PARAMETERS;\n"
hello = '  CCTK_VINFO("Hello from notebook 5: initial condition %s", initial_condition);\n'
if hello not in text:
    wavetoyx.write_text(text.replace(after, after + hello, 1))
```

Each repository is a git clone under `~/Cactus/repos/` (the entries under
`arrangements/` link into them, as with GetComponents), so `git diff` shows
the edit, and `cactup config delta` shows how the source tree differs from
what a config was last built from, and what a build would do about it:

```{code-cell} ipython3
%%shell
git -C ~/Cactus/repos/CarpetX diff
cactup config delta
```

`config delta` only reads; `build` acts on it. Near the end of the box,
`COMPILING CarpetX/WaveToyX/src/wavetoyx.cxx` is the edited file; then the
executable is linked again:

```{code-cell} ipython3
%%shell
cactup build tutorial
```

Run the test with the new executable, wait for it as before, and pick the
hello line out of its log:

```{code-cell} ipython3
%%shell
while squeue -h -n hello | grep -q .; do sleep 1; done
cactup sim submit hello ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par -T 1 -w 00:05:00 --overwrite
while squeue -h -n hello | grep -q .; do sleep 1; done
grep Hello ~/.cactup/simulations/ET_2026_05_v0/tutorial/hello/output-0000/hello.out
```

### A thorn's parameters

Next, make the greeting a parameter. Parameters are declared in the thorn's
`param.ccl`, one of the `.ccl` files that define a thorn's interface to
Cactus. Cactus generates code from them, so an edit to one changes the
thorn's *shape* (another sense of the word than a run's shape: the set of
files cactup fingerprints, which the build reports as "thorn contents
changed"), as a file added to or removed from its `src/` does. cactup then
rebuilds that thorn from scratch rather than trusting `make` to notice
everything that changed; WaveToyX has one source file, so that is still
quick.

The cell appends a string parameter, `greeting` (any value, default "Hello
from notebook 5"), to `param.ccl`, and changes the hello line to print it.

```{code-cell} ipython3
# Add the parameter and use it; does nothing if that's already done.
thorn = Path("~/Cactus/arrangements/CarpetX/WaveToyX").expanduser()
param = thorn / "param.ccl"
greeting = '''
CCTK_STRING greeting "What WaveToyX says when it sets up initial data"
{
  ".*" :: "anything"
} "Hello from notebook 5"
'''
if "greeting" not in param.read_text():
    param.write_text(param.read_text() + greeting)
text = wavetoyx.read_text()
old = '"Hello from notebook 5: initial condition %s", initial_condition'
if old in text:
    wavetoyx.write_text(text.replace(old, '"%s: initial condition %s", greeting, initial_condition'))
```

```{code-cell} ipython3
%%shell
git -C ~/Cactus/repos/CarpetX diff --stat
```

```{code-cell} ipython3
%%shell
cactup build tutorial
```

Scroll to the top of the box: cactup's first lines name the reason,
`thorn contents changed`, and the thorn, WaveToyX, whose own build files it
removes so that all of it recompiles, with the bindings Cactus generates
from its `.ccl` files.

Set the new parameter in a copy of the test's parameter file, and run
again. A simulation keeps a copy of the executable it was created with, so
it takes a new `hello`, made with the new build, to run the new code;
`--overwrite` replaces the old one:

```{code-cell} ipython3
%%shell
cp ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par ~/hello.par
echo 'WaveToyX::greeting = "Hello from a parameter file"' >> ~/hello.par
while squeue -h -n hello | grep -q .; do sleep 1; done
cactup sim submit hello ~/hello.par -T 1 -w 00:05:00 --overwrite
while squeue -h -n hello | grep -q .; do sleep 1; done
grep Hello ~/.cactup/simulations/ET_2026_05_v0/tutorial/hello/output-0000/hello.out
```

### The flesh

The *flesh* is Cactus's core: the code every config is built around. Here,
it prints the banner every run starts with. Add a line to it:

```{code-cell} ipython3
# Add a line to the banner; does nothing if it's already there.
banner = Path("~/Cactus/src/main/Banner.c").expanduser()
text = banner.read_text()
after = '  printf ("Parameter file:    %s\\n", buffer);\n'
line = '  printf ("Edited in:         notebook 5\\n");\n'
if line not in text:
    banner.write_text(text.replace(after, after + line, 1))
```

```{code-cell} ipython3
%%shell
cactup config delta
```

```{code-cell} ipython3
%%shell
cactup build tutorial
```

(Scroll up a little in the box: `COMPILING Cactus/main/Banner.c` is the
edited file.) The flesh is a repository of its own (`~/Cactus/src` is a link
into `~/Cactus/repos/flesh`). An edit in place like this one is rebuilt like
any other. A flesh on a different *commit* is another matter: cactup
rebuilds everything then, since the build system itself may have changed, as
notebook 3 showed.

```{code-cell} ipython3
%%shell
while squeue -h -n hello | grep -q .; do sleep 1; done
cactup sim submit hello ~/hello.par -T 1 -w 00:05:00 --overwrite
while squeue -h -n hello | grep -q .; do sleep 1; done
grep -B 2 'Edited in' ~/.cactup/simulations/ET_2026_05_v0/tutorial/hello/output-0000/hello.out
grep Hello ~/.cactup/simulations/ET_2026_05_v0/tutorial/hello/output-0000/hello.out
```

## Putting it back

`git checkout` puts the edited files back to their committed state, and
`config delta` sees that too:

```{code-cell} ipython3
%%shell
git -C ~/Cactus/repos/CarpetX checkout -- WaveToyX
git -C ~/Cactus/repos/flesh checkout -- src/main/Banner.c
cactup config delta
```

The build that follows recompiles the reverted files, and the config is
back to what it was built from:

```{code-cell} ipython3
%%shell
cactup build tutorial
```

```{code-cell} ipython3
%%shell
cactup config delta
```

Close the editor tabs of the files you opened, and delete the simulations
of this notebook:

```{code-cell} ipython3
%%shell
cactup sim delete hello
cactup sim delete wave-pinned
rm -f ~/hello.par
```

## Where this is documented

- [Running simulations](https://max-morris.github.io/Cactup/users/running-simulations.html)
  (topology, queues, universes on submit)
- [Building configurations](https://max-morris.github.io/Cactup/users/building-configs.html)
  (what a rebuild does after an edit, `config delta`, `.cactupignore`,
  universes)

Next: **notebook 6a**, your own machine entry.
