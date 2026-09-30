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

# 1. Getting started: cactup, and a release to work with

In this notebook you will

- get to know the notebook's terminal cells,
- install cactup the way its documentation says to, and watch it update itself,
- see how SimFactory's commands map to cactup's,
- install the Einstein Toolkit's latest release, `ET_2026_05_v0`, and take a
  first look around.

*Time: about 30 minutes.*

Every notebook starts with a **catch-up cell**. It brings this machine to the
state the notebook expects, in case you skipped a notebook or are starting
late, and prints a line for each thing it had to do. Here, at the very start,
there is nothing to catch up on, so it prints nothing.

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 1
```

## The cells

Cells that start with `%%shell` run in a terminal: a bash shell that stays
open from one cell to the next, so a `cd` or an `export` in one cell holds in
the ones after it. Output appears as the command runs, colors and progress
bars included, and a command that fails or is stopped gets a line saying so.
Long output goes in a box of its own that shows its end; once the command has
finished, scroll up in it for the rest.

```{code-cell} ipython3
%%shell
echo "Hello from $(hostname), as $(whoami)"
cd ~
pwd
```

```{code-cell} ipython3
%%shell
pwd
ls
```

A command that runs too long can be stopped with the notebook's stop button
(■), or by pressing <kbd>I</kbd> twice outside the cell: that is Ctrl-C for
the command in the cell. This one stops itself after three seconds, the way
the stop button would:

```{code-cell} ipython3
%%shell --timeout 3 --expect-fail
echo "counting to 60..."
for i in $(seq 60); do sleep 1; echo "$i"; done
```

Two more kinds of cells appear in these notebooks: `%%file` writes a file
(with its contents highlighted as the kind of file it is), and `%show`
displays one.

```{code-cell} ipython3
%show /opt/cactup-tutorial/thornlists/tutorial.th --lines 1:14
```

The plain Python cells are ordinary Python; later notebooks use them to plot
simulation output. A **Terminal** (File > New > Terminal) is available too,
for anything you would rather type yourself.

## Installing cactup

cactup is a single program. The documentation's front page installs it with
one line:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://max-morris.github.io/Cactup/cactup-init.sh | sh
```

This machine has no route to the internet, and serves cactup from its own
update site instead, `http://127.0.0.1:8765`, just as a cluster's site might
mirror it. The next cell runs the same installer against it (the
`--proto '=https'` has to go for a plain-http address). The installer asks
before it changes anything; the cell answers `y` for you.

```{code-cell} ipython3
%%shell --stdin 'y\n'
curl -sSf http://127.0.0.1:8765/cactup-init.sh | CACTUP_UPDATE_ROOT=http://127.0.0.1:8765 sh
```

The installer put `cactup` in `~/.cactup/bin`, added that directory to your
`PATH` in `~/.profile` and `~/.bashrc`, and pointed you at the documentation.
The documentation links it printed are on this machine, not on your laptop:
read the documentation at <https://max-morris.github.io/Cactup/> instead.

These cells already have `~/.cactup/bin` on their `PATH`, so `cactup` works
right away:

```{code-cell} ipython3
%%shell
cactup --version
```

## cactup keeps itself up to date

cactup checks for a newer build of itself once a day, on the first command
you run in a terminal (`cactup --version` aside), and switches to it before
doing what you asked. The installer on this machine deliberately installed an
older build, so you can watch that happen now: the first line below is the
update.

```{code-cell} ipython3
%%shell
cactup releases
```

```{code-cell} ipython3
%%shell
cactup --version
ls -l ~/.cactup/bin
```

`cactup` is a link to the build in use. The build it replaced stays next to
it, so a bad update can be undone by hand. Whether cactup updates itself is a
*knob*, one of cactup's settings. `cactup knob NAME` shows one, and `cactup
knob` lists them all:

```{code-cell} ipython3
%%shell
cactup knob autoupdate
cactup update --check
```

A knob is set for you with `cactup knob NAME VALUE` (`cactup knob autoupdate
notify` would only tell you about a new build, `off` would not even check), or
for a single command with `-K NAME=VALUE`, which stores nothing.

## Coming from SimFactory

If you have used SimFactory (`sim`), most of what you know carries over:

| SimFactory | cactup |
|---|---|
| `GetComponents --parallel thornlist.th` | `cactup install ET_2026_05_v0` (or `--thornlist thornlist.th`) |
| `./simfactory/bin/sim build` | `cactup build` |
| `sim build tutorial --thornlist=tutorial.th` | `cactup build tutorial --thornlist tutorial.th` |
| `sim create-submit N --parfile=P --procs=4` | `cactup sim submit N P -T 4` |
| `sim list-simulations` | `cactup sim list` |
| `sim show-output N` | `cactup sim log N` |
| `sim stop N` | `cactup sim stop N` |
| `sim purge N` | `cactup sim delete N` |
| `sim whoami` | `cactup machine show` |
| `simfactory/mdb/machines/*.ini` | an MDB entry: a directory with `meta.toml`, optionlists and scripts |

Notebook 10 goes through the differences in depth.

## Installing a release

`cactup releases` above listed the Einstein Toolkit's releases, newest first.
Install the latest one. `--silent` accepts the defaults for everything
`cactup install` would otherwise ask (where to put it, what to call it). Run
again, the cell says that installation exists already.

```{code-cell} ipython3
%%shell
cactup install ET_2026_05_v0 --silent
```

That took seconds, not the several minutes some 80 repositories would take
from GitHub and Bitbucket: this machine serves the Einstein Toolkit's repositories
from local mirrors, as a site that mirrors GitHub would. cactup, the
repositories' `origin` and the thornlists all name the real repositories;
only git itself, asked directly, shows where it fetches from:

```{code-cell} ipython3
%%shell
cd ~/Cactus/repos/flesh
git config --get remote.origin.url
git remote -v
cd ~
```

## A first look around

`cactup list` lists your installations; the one marked active is the one
cactup commands work on unless told otherwise.

```{code-cell} ipython3
%%shell
cactup list
```

`cactup show` summarizes where you are: the active installation, its active
configuration (none yet: notebook 2 builds one), and the machine cactup
thinks it is running on.

```{code-cell} ipython3
%%shell
cactup show
```

cactup recognized this machine without being told: the machine database
(MDB) has an entry for it, `cactup-tutorial`, which matches its hostname and
describes its scheduler (SLURM), its queues, its hardware and how to compile
and run on it. `cactup machine show` shows that last part alone, and
`cactup machine show NAME` any other machine's. Clusters you use get entries
of their own (notebook 6).

The installation is an ordinary Cactus tree. cactup made a link to it,
`~/Cactus`, and keeps its own files under `~/.cactup`:

```{code-cell} ipython3
%%shell
ls ~/Cactus
ls ~/.cactup
```

## Where this is documented

- [Getting started](https://max-morris.github.io/Cactup/users/getting-started.html)
- [Keeping cactup up to date](https://max-morris.github.io/Cactup/users/updating.html)
- [Installing releases](https://max-morris.github.io/Cactup/users/installing-releases.html)
- [Command reference](https://max-morris.github.io/Cactup/reference/cli.html)

Next: **notebook 2**, building a configuration and running a first simulation.
