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

# 3. A second installation, on forks of the flesh and CarpetX

In this notebook you will

- make a second installation from a thornlist, to experiment on without
  touching the first,
- point two of its repositories at forks, by editing the installation's
  thornlist,
- refetch, and see how cactup guards what you have on disk,
- see how cactup decides what to rebuild once the sources have moved.

*Time: about 30 minutes.* (To run this notebook a second time from the top,
see "Starting over" at its end first: run again as it is, most cells only
report that their work is already done.)

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 3
```

## The forks

Say you want to try the mixed-precision branches of the flesh and CarpetX:
`github.com/max-morris/Cactus` and `github.com/max-morris/CarpetX`, branch
`mixed-precision`, which add single-precision (`CCTK_REAL4`) and
half-precision grid functions, with test thorns that check them. You don't
want to experiment on the installation you work with, so make a new one.

`cactup install` can install from a thornlist file instead of a release. Use
the tutorial's thornlist again: a small installation, quick to fetch. Two
flags matter here:

- `--alias et-mp` names the installation (by default it is named after the
  release, or the thornlist file: `tutorial` here, which says little);
- `--symlink-name et-mp` names the link cactup makes in your home to its
  source tree. Without it, the link would be `~/Cactus` again, and would no
  longer point at the stock installation.

```{code-cell} ipython3
%%shell
cactup install --thornlist /opt/cactup-tutorial/thornlists/tutorial.th --alias et-mp --symlink-name et-mp --silent
```

Installing doesn't switch to the new installation (unless none was active).
`cactup use` does:

```{code-cell} ipython3
%%shell
cactup list
cactup use et-mp
cactup list
```

Build the `tutorial` config in it. There is no `--thornlist` this time: an
installation made from a thornlist has that thornlist as its own, the
*live thornlist*, and the build uses it.

```{code-cell} ipython3
%%shell
cactup build tutorial
```

## Pointing repositories at forks

The live thornlist is `thornlists/installation-default.th` in the
installation. These are its entries for the flesh and for CarpetX:

```{code-cell} ipython3
%show ~/et-mp/thornlists/installation-default.th --lines 26:33
```

```{code-cell} ipython3
%show ~/et-mp/thornlists/installation-default.th --lines 47:54
```

Change the flesh's and CarpetX's `!URL` and `!REPO_BRANCH` to the forks', and
add the fork's single-precision test thorn, `CarpetX/TestReal4`, to CarpetX's
thorns. You could do this in the editor (double-click the file in
`et-mp/thornlists`); the next cell does it in Python, and shows the result
against the thornlist the installation came from:

```{code-cell} ipython3
import difflib
from pathlib import Path

live = Path.home() / "et-mp/thornlists/installation-default.th"
text = live.read_text()
edits = [
    ("!URL      = https://bitbucket.org/cactuscode/cactus.git\n!REPO_BRANCH = $ET_RELEASE",
     "!URL      = https://github.com/max-morris/Cactus.git\n!REPO_BRANCH = mixed-precision"),
    ("!URL      = https://github.com/EinsteinToolkit/CarpetX\n!REPO_BRANCH = $ET_RELEASE",
     "!URL      = https://github.com/max-morris/CarpetX.git\n!REPO_BRANCH = mixed-precision"),
    ("CarpetX/TestProlongate\n", "CarpetX/TestProlongate\nCarpetX/TestReal4\n"),
]
for old, new in edits:
    if new in text:
        continue  # already done (this cell was run before)
    if old not in text:
        print("not found, so not changed (was the file edited by hand?):\n" + old)
    text = text.replace(old, new)
live.write_text(text)

original = Path("/opt/cactup-tutorial/thornlists/tutorial.th").read_text()
print("".join(difflib.unified_diff(original.splitlines(True), text.splitlines(True),
                                   "tutorial.th", "installation-default.th", n=1)))
```

The forks keep the repositories' names (a repository's directory is named
after the thornlist's `!NAME`, or else after the URL's last part), so the
flesh stays in `repos/flesh` and CarpetX in `repos/CarpetX`. The thornlist
now asks for other code in the same places.

## Refetching

`cactup inst refetch` brings an installation's repositories in line with its
live thornlist. Ask it what it would do first, with `-n` (a dry run):

```{code-cell} ipython3
%%shell
cactup inst refetch -n
```

The other eight repositories would be fetched and fast-forwarded as usual.
The flesh and CarpetX would be **skipped**: their `origin` is not the URL
the thornlist names. cactup never silently replaces a repository that
differs from what it expects, since that could be your own work: a
repository you re-pointed yourself, local commits, edits. It asks you to say
so, for all the repositories it would skip (`--overwrite-modified`) or for
the ones you name (`--overwrite`):

```{code-cell} ipython3
%%shell
cactup inst refetch --overwrite flesh,CarpetX
```

Both repositories now follow the forks: their `origin` was re-pointed, and
they are on `mixed-precision`. Anything you had changed in them would have
been copied to `~/.cactup/refetch-backups/et-mp/` first. `cactup inst delta`
compares the installation with what the last fetch put there, repository by
repository and thorn by thorn. Right after a refetch everything matches: that
is the baseline any later edit, checkout or deleted thorn shows against.

```{code-cell} ipython3
%%shell
cactup inst delta
git -C ~/et-mp/repos/flesh log --oneline -1
git -C ~/et-mp/repos/CarpetX log --oneline -1
```

## Rebuilding

The `tutorial` config was built from the stock flesh and CarpetX. `cactup
config delta` shows how its sources have moved since:

```{code-cell} ipython3
%%shell
cactup config delta tutorial
```

A thorn's repository on another commit means a rebuild of what it affects.
The flesh on another commit means more: the flesh is Cactus's make system
and generates `cctk_Config.h`, which every object depends on, so cactup
rebuilds the config from scratch. `cactup build` says why before it starts.
That line is at the top of its output: drag the box's scrollbar to the top
once the build is done. (The fork's CarpetX compiles each precision's
templates, with many more warnings; the middle of so long an output is left
out.)

```{code-cell} ipython3
%%shell
cactup build tutorial
```

## Trying the fork

`TestReal4` checks that single precision works: it fills a `CCTK_REAL8` and
a `CCTK_REAL4` grid function with the same function, syncs them, and checks
every point of both, and does the same for grid scalars and arrays and for
functions called with either precision. Its parameter files are in the
thorn's `par` directory; run the first, as one process with four threads:

```{code-cell} ipython3
%%shell
cactup sim submit real4 ~/et-mp/arrangements/CarpetX/TestReal4/par/testreal4.par -T 1 -c 4 -w 00:05:00 --overwrite
while [ -n "$(squeue -h -u "$USER")" ]; do sleep 2; done
cactup sim list
```

It checks after every iteration, three here, and the grid-function checks
report once for each of the grid's two boxes. Count the verdicts:

```{code-cell} ipython3
%%shell
grep -oE 'TestReal4(\[[^]]*\]|-aliasfn| \(GF3D5 accessor\)): (PASS|FAIL)' \
    "$(cactup sim show real4 --output-dir)/real4.out" | sort | uniq -c
```

The stock installation is just as you left it: `cactup -I NAME` runs one
command on another installation, without switching to it.

```{code-cell} ipython3
%%shell
cactup -I ET_2026_05_v0 config list
git -C ~/Cactus/repos/flesh log --oneline -1
```

## Starting over

To run this notebook again from the top, remove the installation, its link
and its simulations (`cactup uninstall` keeps an installation's simulations
on disk, under `~/.cactup/simulations/et-mp`; here they would only get in
the way): take the `#` off the three commands below, run the cell, and put
the `#` back.

```{code-cell} ipython3
%%shell
# cactup uninstall et-mp --force
# rm ~/et-mp
# rm -rf ~/.cactup/simulations/et-mp
cactup list
```

## Where this is documented

- [Installing releases](https://max-morris.github.io/Cactup/users/installing-releases.html)
  (thornlists, `--alias`, `inst refetch`, `inst delta`)
- [Building configurations](https://max-morris.github.io/Cactup/users/building-configs.html)
  (what triggers a rebuild, `config delta`)

Next: **notebook 4a**, living with several installations.
