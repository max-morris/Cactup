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

# 6a. Your own machine entry

In this notebook you will

- see where cactup's knowledge of machines comes from: the machine database,
  and your own additions to it,
- make a machine entry of your own, `mylab`, from the tutorial machine's,
- fix what the copy gets wrong, and see how strictly cactup reads an entry,
- run a simulation on it.

Notebook 6b goes on with `mylab`, and grows it into a bigger entry.

*Time: about 30 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 6a
```

## The machine database

Everything cactup knows about a machine (its queues, its compilers, how to
submit a job and how to start Cactus in one) comes from the machine's entry
in the *machine database*, the MDB. There are two layers:

- the *system MDB*, which cactup downloads and keeps up to date, with an
  entry for each cluster supported by the Cactup project;
- your *user MDB*, in `~/.cactup/machines/`: entries of your own, for a
  machine that the system MDB doesn't know, or to change one that it does.

`cactup machine list` shows both: entries of yours are marked `(user MDB)`
(you have none yet), and the one that this machine was recognized as is marked
`(detected)`:

```{code-cell} ipython3
%%shell
cactup machine list
```

An entry is a directory. The system MDB keeps its copy under
`~/.cactup/mdb/`. (`gen-5` is the database generation read by this cactup, a
link to a directory named after the MDB's commit, which is the path `machine
show` prints; more on generations below.)

```{code-cell} ipython3
%%shell
(cd ~/.cactup/mdb/gen-5/cactup-tutorial && find . -type f | sort)
```

- `meta.toml` describes the machine: its hardware, its scheduler and queues,
  and which scripts go with which queues.
- `optionlists/` are the build settings, one file for each flavor (notebook
  4b built with `gpu`).
- `submitscripts/` make the batch script for a job (notebook 5 showed one),
  and `runscripts/` the script that starts Cactus inside it.
- `hostname.regexp` is how cactup recognizes the machine: a pattern for its
  hostname.

## A machine of your own

`cactup machine create` makes a new entry in your user MDB.
`--from-existing` starts it as a copy of another entry, which is the quickest
way to describe a machine much like one known to cactup. Two more options matter
here:

- `--no-discover`: by default, the new entry claims the host that it was made on
  (it gets a `hostname.regexp` for it) and becomes the detected machine. This
  container must stay `cactup-tutorial` for the other notebooks, so `mylab`
  claims nothing, and commands use it when you say so with `--machine mylab`.
- `--silent`: don't ask where installations and simulations go, or for an
  email address and an allocation; take the defaults.

```{code-cell} ipython3
%%shell
cactup machine create mylab --from-existing cactup-tutorial --no-discover --silent
(cd ~/.cactup/machines/mylab && find . -type f | sort)
```

The scripts and optionlists are copied as they are, but not
`hostname.regexp` (nor a `discover.py`): a copy never claims the original's
hosts. `meta.toml` is written anew: without the original's comments, and
with some values found out afresh. Its first lines:

```{code-cell} ipython3
%show ~/.cactup/machines/mylab/meta.toml --lines 1:13
```

`[hardware]` is the original's: it states this machine's 4 CPUs and its
memory, and a copy keeps what the original states. (For a machine whose
entry leaves them out, `machine create` detects them on the host that it
runs on, which inside a container would be the computer that the container
runs on, not the 4 CPUs and the memory given to this container.) The
original's comments say why it states them:

```{code-cell} ipython3
%show ~/.cactup/mdb/gen-5/cactup-tutorial/meta.toml --lines 25:36
```

One value is wrong for `mylab`: `nickname`, a short label for people (cactup
only displays it, in `machine show`), was set to this host's name,
`cactup-tutorial`, the same as the original's. (`hostname`, like the rest of
`[machine]`, only describes the machine; claiming a host is
`hostname.regexp`'s job, which notebook 6b comes to.)

### Exercise: fix `nickname`

Set `nickname` to `"mylab"`. Open `~/.cactup/machines/mylab/meta.toml` in
the editor (*File > Open from Path…*, then `.cactup/machines/mylab/meta.toml`)
and edit it there, or run the next cell, which makes the same edit. If you
edit in the editor, save, and close the tab before going on: the next cells
change the file again, and an open tab doesn't follow changes made on disk.

```{code-cell} ipython3
import re
from pathlib import Path

meta = Path("~/.cactup/machines/mylab/meta.toml").expanduser()
text = meta.read_text()
meta.write_text(re.sub(r"^nickname = .*$", 'nickname = "mylab"', text, count=1, flags=re.M));
```

`cactup machine show NAME` reads an entry as every command does:

```{code-cell} ipython3
%%shell
cactup machine show mylab
```

(`?` means `[hardware]` leaves it unset; the `gpu` queue below sets its own
`max-gpus-per-node`.)

## How strictly cactup reads an entry

cactup checks a `meta.toml` whenever it reads one, and refuses one that
it doesn't understand rather than guess. A misspelled key, for instance
(`max-cpu-per-node`):

```{code-cell} ipython3
meta.write_text(re.sub(r"^max-cpus-per-node\s*=", "max-cpu-per-node =", meta.read_text(), flags=re.M));
```

```{code-cell} ipython3
%%shell --expect-fail
cactup machine show mylab
```

The error names the line and the keys that would be right. Put it back:

```{code-cell} ipython3
meta.write_text(re.sub(r"^max-cpu-per-node\s*=", "max-cpus-per-node =", meta.read_text(), flags=re.M));
```

The same goes for a value of the wrong type (`memory = "7372 MB"`), a queue
name that a script lists but `[queues]` doesn't have, or a script that the
entry names but its directory lacks.

### Generations

The end of the file is cactup's own:

```{code-cell} ipython3
%%shell
sed -n '/^\[cactup\]/,$p' ~/.cactup/machines/mylab/meta.toml
```

- `mdb-generation` is the version of the entry format that this file was
  written in. When a cactup release changes the format, the system MDB moves
  to a new generation along with it. cactup brings an older user entry
  forward itself, when it first reads it, and keeps a copy of the old files
  in `~/.cactup/machine-backups/`; where it can't be sure the entry would
  still mean the same, it stops and says what to change. An entry written
  for a newer cactup is refused rather than misread.
- `[cactup.origin]` records which entry this one was copied from, and a hash
  of that entry. If the original changes later (a fix in the system MDB),
  cactup warns that `mylab` is a copy of an older version: a copy doesn't
  follow its original.

Say `mylab` were written for a newer cactup:

```{code-cell} ipython3
meta.write_text(re.sub(r"^mdb-generation\s*=.*$", "mdb-generation = 6", meta.read_text(), flags=re.M));
```

```{code-cell} ipython3
%%shell --expect-fail
cactup machine show mylab
```

The error tells you to `cactup update`, which is what you'd do for real.
Here the number was changed by hand, so put it back:

```{code-cell} ipython3
meta.write_text(re.sub(r"^mdb-generation\s*=.*$", "mdb-generation = 5", meta.read_text(), flags=re.M));
```

### Replacing a system entry

An entry in your user MDB with the same name as a system one replaces it,
for every command: that is how you change a system entry for yourself
(`cactup machine create NAME --from-existing NAME`, then edit). Not here,
though: the tutorial needs its own entry as it is, and the first cell of
each notebook moves such a replacement out of the way.

## Running on mylab

Commands use the detected machine unless told otherwise; `--machine mylab`
says otherwise. Submit WaveToyX's test on `mylab`, with the `tutorial`
config (the active one). cactup refuses:

```{code-cell} ipython3
%%shell --expect-fail
cactup sim submit lab1 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --machine mylab -T 1 -w 00:05:00 --overwrite
```

A config remembers the machine that it was built for, and cactup won't run it on
another: an executable built with one machine's compilers and libraries
seldom runs on another. Here `mylab` is a copy of `cactup-tutorial`, so it
will; `--ignore-machine` says so. (As in notebook 5, `--overwrite` replaces
an earlier `lab1`, and `--block` waits for the job.)

```{code-cell} ipython3
%%shell
cactup sim submit lab1 ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par --machine mylab --ignore-machine -T 1 -w 00:05:00 --overwrite --block
cactup sim show lab1
```

(The `note:` line about the config's machine appears twice: once when the
simulation is created, and once when it is submitted.) The simulation
records `mylab` as its machine, and the job ran through `mylab`'s scripts:
the last line of its batch script starts cactup with `--machine=mylab`.

```{code-cell} ipython3
%%shell
tail -n 1 ~/.cactup/simulations/MyCactus/tutorial/lab1/output-0000/.cactup/submit-script
cactup sim delete lab1
```

## Where this is documented

- [MDB overview](https://max-morris.github.io/Cactup/authors/mdb-overview.html)
  (the two layers, an entry's files)
- [Porting a cluster](https://max-morris.github.io/Cactup/authors/porting-a-cluster.html)
  (`machine create`, step by step)
- [meta.toml reference](https://max-morris.github.io/Cactup/authors/meta-toml.html)
- [MDB generations](https://max-morris.github.io/Cactup/authors/mdb-generations.html)

Next: **notebook 6b**, growing `mylab`: a queue, its scripts, a script
that checks a request, knobs, an optionlist flavor, a universe, and how
cactup recognizes a machine.
