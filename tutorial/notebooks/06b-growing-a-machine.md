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

# 6b. Growing a machine entry

In this notebook you will add to `mylab`, the machine entry from notebook
6a, the things that a real cluster's entry has:

- a queue, mapped onto one of SLURM's partitions,
- a submit script of its own for that queue,
- a check that refuses a request that the queue can't serve,
- a knob, a setting of yours that scripts can read,
- an optionlist flavor with a `[cactup]` header,
- a universe,
- a way for cactup to recognize the machine by itself.

Every step ends with cactup reading the entry, and most with a real job.

*Time: about 40 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 6b
```

The cells below change `mylab`'s files with a little Python, so the notebook
runs from top to bottom. Open the files in the editor to see each change
(*File > Open from Path…*, `.cactup/machines/mylab/...`), or make the
changes there yourself. An open tab doesn't follow the changes made by a cell:
use *File > Reload … from Disk* (the menu item names the file's type) after
the cell runs, and don't save a tab over a file that a cell has changed since.
`add` appends a piece of TOML to `meta.toml`, once:

```{code-cell} ipython3
import re
from pathlib import Path

mylab = Path("~/.cactup/machines/mylab").expanduser()
meta = mylab / "meta.toml"

def add(toml):
    """Append TOML to mylab's meta.toml, unless it is there already."""
    if toml.strip() not in meta.read_text():
        meta.write_text(meta.read_text().rstrip("\n") + "\n\n" + toml.strip() + "\n")
```

## A queue

The entry's `[queues]` are cactup's names for the scheduler's queues
(SLURM calls them *partitions*). A queue's `name` is the partition that
it submits to, when that differs. `long`: SLURM's `batch` partition, limited to
an hour per job.

A queue also needs scripts: `[submitscripts]` and `[runscripts]` say which
script serves which queues, and cactup refuses an entry with a queue that no
submit script, or no run script, lists. Add `long` to the `default` scripts'
lists, too:

```{code-cell} ipython3
add('''
[queues.long]
name         = "batch"
max-walltime = "01:00:00"
''')

QUEUES = '''default = [
    "debug",
    "short",
    "batch",
    "gpu",
]'''
WITH_LONG = QUEUES.replace('"gpu",', '"gpu",\n    "long",')
for table in ("[runscripts]", "[submitscripts]"):
    if f"{table[:-1]}.long]" in meta.read_text():
        continue  # long has a script of its own already (the next section)
    meta.write_text(meta.read_text().replace(f"{table}\n{QUEUES}", f"{table}\n{WITH_LONG}", 1))
```

```{code-cell} ipython3
%%shell
cactup machine show mylab | sed -n '/^  queues:/,/^  submitscripts:/p'
```

Submit to it (`-c 2`, two threads per task, is for a check further down).
cactup refuses it:

```{code-cell} ipython3
%%shell --expect-fail
cactup sim submit q1 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --machine mylab --ignore-machine -T 1 -c 2 -w 00:05:00 -q long --overwrite
```

The `tutorial` config was built from an optionlist whose header lists the
queues that its executables suit (`[cactup.runs-on]`, notebook 4b), and the
config recorded that list when it was built. A queue added since is not on
it. `--force-queue` overrides the check. The simulation exists already
(cactup created it before checking the queue), so this time no parameter
file:

```{code-cell} ipython3
%%shell
cactup sim submit q1 --machine mylab --ignore-machine --force-queue -T 1 -c 2 -w 00:05:00 -q long --block
grep -E '^(queue|QUEUE) ' ~/.cactup/simulations/MyCactus/tutorial/q1/output-0000/.cactup/restart.toml
```

cactup's queue is `long`; what reached SLURM (`QUEUE`, the `@QUEUE@` used by
the scripts) is `batch`.

A `-w` above an hour isn't refused on `long` either: as on `debug`
(notebook 5), cactup splits it into a chain of jobs of an hour each.

## A submit script for the queue

A job gets the submit script that lists its queue in `[submitscripts]`, and
a queue is listed by one script only. Copy the default script to `long.py`,
with one more line, a comment on the job that SLURM shows, and move `long`
from `default`'s list to its own:

```{code-cell} ipython3
scripts = mylab / "submitscripts"
long_py = scripts / "long.py"
if not long_py.exists():
    text = (scripts / "default.py").read_text()
    text = text.replace('lines.append("#SBATCH --open-mode=append")\n',
                        'lines.append("#SBATCH --open-mode=append")\n'
                        'lines.append("#SBATCH --comment=long-queue")\n')
    long_py.write_text(text)
meta.write_text(meta.read_text().replace(f"[submitscripts]\n{WITH_LONG}", f"[submitscripts]\n{QUEUES}", 1))
add('''
[submitscripts.long]
queues = ["long"]
''')
```

```{code-cell} ipython3
%%shell
cactup machine show mylab | grep submitscripts
cactup sim submit q2 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --machine mylab --ignore-machine --force-queue -T 1 -c 2 -w 00:05:00 -q long --overwrite --block
grep comment ~/.cactup/simulations/MyCactus/tutorial/q2/output-0000/.cactup/submit-script
```

A script marked `test = true` is one for `cactup test` runs (notebook 9),
never for simulations, and its queues are counted separately: `mylab`'s
`test` scripts list the queues that the tests may run on, which needn't be
all of them (`long` isn't one).


## A script that checks the request

A `.py` submit script gets every template variable as a Python variable
(`typed` has the numbers as numbers), and what it prints is the job script.
It can also refuse a request, with `CactupError`, before anything reaches
SLURM; `default.py` does that for `-n` above 1 (notebook 5). Say `long` is
for multithreaded runs: refuse fewer than two threads per task.

```{code-cell} ipython3
check = '''
if typed['CPUS_PER_TASK'] < 2:
    raise CactupError(
        "the long queue is for multithreaded runs, but this one asks for {0} "
        "thread per task.\\nPass -c 2 or more.".format(CPUS_PER_TASK)
    )

'''
text = long_py.read_text()
if "multithreaded" not in text:
    long_py.write_text(text.replace('lines = ["#!/bin/bash"]\n', check + 'lines = ["#!/bin/bash"]\n', 1))
```

```{code-cell} ipython3
%%shell --expect-fail
cactup sim submit q3 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --machine mylab --ignore-machine --force-queue -T 1 -w 00:05:00 -q long --overwrite
```

The message is the script's, followed by the script's path. `q1` and `q2`
passed `-c 2`, so they would have passed the check.

## A knob

A *knob* is a setting of yours that cactup keeps for every command:
`cactup knob` lists them (`allocation`, `mail`, `queue` and so on). Scripts
read them: `@KNOB(name)@` in a `.sh` script (refused if unset),
`@KNOB-OPTIONAL(name, default)@` (with a fallback), and `knob(name,
default)` in a `.py` one. Have `mylab`'s run script print a note from a knob
of your own, `lab-note`:

```{code-cell} ipython3
run = mylab / "runscripts" / "default.sh"
text = run.read_text()
note = 'echo "Lab note: @KNOB-OPTIONAL(lab-note, "nothing to note")@"\n'
if note not in text:
    run.write_text(text.replace("#!/bin/bash\n", "#!/bin/bash\n" + note, 1))
```

`-K NAME=VALUE` sets a knob for one command:

```{code-cell} ipython3
%%shell
cactup sim submit k1 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --machine mylab --ignore-machine -T 1 -w 00:05:00 -K lab-note="set for one command" --overwrite --block
grep 'Lab note' ~/.cactup/simulations/MyCactus/tutorial/k1/output-0000/k1.out
```

`cactup knob NAME VALUE` keeps one. A knob that isn't one of cactup's own
needs `-c` (`--custom`) the first time, so a typo doesn't quietly make a new
one:

```{code-cell} ipython3
%%shell
cactup knob -c lab-note "kept by cactup"
cactup knob
cactup sim submit k2 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --machine mylab --ignore-machine -T 1 -w 00:05:00 --overwrite --block
grep 'Lab note' ~/.cactup/simulations/MyCactus/tutorial/k2/output-0000/k2.out
cactup knob delete lab-note
```

A restart records the knobs that it was submitted with (in its
`restart.toml`), so the job sees the values that it was submitted with, even
if you change a knob while it waits in the queue; a later submit reads the
knobs anew.

## An optionlist flavor

Notebook 4b built with this machine's `gpu` flavor. A flavor is an
optionlist file in the entry's `optionlists/` directory, named after the
flavor, plus its `[cactup]` header, which is for cactup only. Among its keys:

- `description`, shown by `machine show --flavors`;
- `[cactup.runs-on]`: `gpu`, whether its executables need a GPU; `queues`,
  which queues they may run on; and `default-queue`, the one that they go
  to when a submit names none;
- `disabled-thorns`: thorns that the flavor leaves out of a build;
  `enabled-thorns`: thorns commented out in the thornlist as `#DISABLED`
  that the flavor puts back;
- `default`: whether it's the flavor used by a build without `--flavor` (one
  flavor only), and `universe` and `coerce-run-universe` (notebook 5).

`small`: the default build without the Cottonmouth thorns, for `short` and
`long` only. Copy `optionlists/default.toml` to `small.toml`, give it the
header below (no `default`: that stays the default flavor's), and give it a
`VERSION` of its own: a config is rebuilt from scratch whenever its
optionlist's `VERSION` changes, so a flavor's should say which it is. The
new file is all that it takes: every file in `optionlists/` is a flavor.

```{code-cell} ipython3
optionlists = mylab / "optionlists"
small = optionlists / "small.toml"
if not small.exists():
    text = (optionlists / "default.toml").read_text()
    # The header and the comment above it, which describes the default flavor.
    header = text[text.index("# The [cactup] header"):text.index("\n[options]\n") + 1]
    small.write_text(text.replace(header, '''# The small flavor: the default build without Cottonmouth, for the short
# and long queues only.
[cactup]
description     = "The CPU build without Cottonmouth, for the short and long queues"
disabled-thorns = ["Cottonmouth/CottonmouthZ4c4m", "Cottonmouth/CottonmouthLinearWaveID"]

[cactup.runs-on]
queues = ["short", "long"]
gpu    = false

''').replace('VERSION = "cactup-tutorial-2026-09-29"', 'VERSION = "mylab-small-1"')
      .replace('(flavor "default")', '(flavor "small")', 1))
```

```{code-cell} ipython3
%%shell
cactup machine show mylab --flavors
```

`cactup build tutorial-small --machine mylab --flavor small --thornlist
/opt/cactup-tutorial/thornlists/tutorial.th` would build it: a config of its
own, whose processed thornlist has the two thorns `#DISABLED`, and whose
simulations need `-q short` or `-q long`. It isn't built
here: no build in this notebook was prepared ahead of time, so it would
compile everything (about ten minutes). The header is read as strictly as
`meta.toml`: a misspelled key in it is an error, which names the file.

## A universe

Notebook 5's `taskset` universe wrapped runs in `taskset`. A universe can also
set up the environment that its commands run in: `run-env-setup` is shell that
runs at the top of the run script. `nice` runs Cactus at a lower priority,
through `env` (which sets a variable for what it runs) and `nice`:

```{code-cell} ipython3
add('''
[universes.nice]
wrapper-argv  = ["env", "LAB_WRAPPED=yes", "nice", "-n", "5"]
run-env-setup = """
export LAB_UNIVERSE=nice
"""
''')
```

`--universe nice` picks it for one simulation (the `tutorial` config has no
universe of its own):

```{code-cell} ipython3
%%shell
cactup machine show mylab | grep universes
cactup sim submit u1 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --machine mylab --ignore-machine -T 1 -w 00:05:00 --universe nice --overwrite --block
grep -E '^LAB_' ~/.cactup/simulations/MyCactus/tutorial/u1/output-0000/.cactup/ENVIRONMENT
```

`.cactup/ENVIRONMENT` is the environment that the run started Cactus with:
`LAB_UNIVERSE` from the setup, `LAB_WRAPPED` from the wrapper. The wrapper
wraps the run on the compute node, not the `sbatch` that submits it.

## Recognizing the machine

Without `--machine`, cactup works out which machine it is on, once, and
remembers it (`cactup machine forget` makes it look again). Two ways for an
entry to claim a host:

- `hostname.regexp`: a regular expression for the host's name;
- `discover.py`: a Python function, `is_machine(hostname)`, for when the
  name isn't enough (say, a login node that shares its name pattern with
  another cluster's, but has a file that only this one has).

If two entries claim the same host, cactup asks which one is meant, so
`mylab` must not claim this one. Give it a name that it does claim:

```{code-cell} ipython3
(mylab / "hostname.regexp").write_text(r"^mylab(\.example\.org)?$" + "\n");
```

`--hostname` tells cactup to recognize a machine as if it ran on that host
(a `~/.hostname` file does the same for every command):

```{code-cell} ipython3
%%shell
cactup --hostname mylab.example.org machine show | sed -n '1,3p'
cactup machine show | sed -n '1,3p'
```

The first command found `mylab`, and the second, on this host's own name,
`cactup-tutorial` again: each change of name makes cactup look again.

The same with a `discover.py` instead:

```{code-cell} ipython3
(mylab / "hostname.regexp").unlink(missing_ok=True)
(mylab / "discover.py").write_text('''
def is_machine(hostname):
    # A real one might also check, say, Path("/etc/mylab-release").exists().
    return hostname.endswith(".example.org")
''');
```

```{code-cell} ipython3
%%shell
cactup --hostname node7.example.org machine show | sed -n '1,3p'
cactup machine show | sed -n '1,3p'
```

A `discover.py` that raises an error counts as "not this machine" (`-v`
shows the error).

## Cleaning up

Delete the simulations of this notebook and `mylab` itself, and make cactup
look for its machine afresh. Simulations outlive their machine entry, so
they go first.

```{code-cell} ipython3
%%shell
for sim in q1 q2 q3 k1 k2 u1; do cactup sim delete $sim; done
cactup machine delete mylab
cactup machine forget
cactup machine show | sed -n '1,2p'
```

## Where this is documented

- [meta.toml reference](https://max-morris.github.io/Cactup/authors/meta-toml.html)
  (queues, scripts, universes, knobs)
- [Submit and run scripts](https://max-morris.github.io/Cactup/authors/scripts-and-variables.html)
  (`.py` scripts, `CactupError`, the template variables)
- [Optionlists](https://max-morris.github.io/Cactup/authors/optionlists.html)
  (the `[cactup]` header)
- [Machine discovery](https://max-morris.github.io/Cactup/authors/machine-discovery.html)
- [Running simulations](https://max-morris.github.io/Cactup/users/running-simulations.html)
  (knobs, `-K`)

Next: **notebook 7**, long runs: chained jobs, following a run, checkpoints
and restarts.
