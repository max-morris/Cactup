+++
title = "Installing Releases"
description = "Install Einstein Toolkit releases with flexible paths and naming"
+++

# Installing Einstein Toolkit Releases

This guide covers everything you need to know about installing and managing Einstein Toolkit releases with cactup.

## Listing releases

View the most recent releases:

```sh
cactup releases
```

See all available versions:

```sh
cactup releases --all
```

The listing is release tags only. Newer than all of them is the manifest's
`master` branch, which `cactup releases` mentions in its closing line — see
"Installing the master tip" below.

## Installing a release

### Simple installation

Install the latest release with interactive prompts:

```sh
cactup install
```

cactup will ask for:
- **Installation name (alias)** — how you'll refer to this release (e.g., `et-2025`, `latest`)
- **Install prefix** — where to install the Cactus source and build (default: `~/.cactup/cacti`)
- **Symlink location** — optional: create a symlink to this installation

### Installing a specific release

Install a particular version without prompts:

```sh
cactup install ET_2025_05 --alias myrelease --silent
```

### Installing the master tip

Pass `master` instead of a release name to install the thornlist at the tip of
the manifest repository's `master` branch — newer than every release, and where
a component lands before it is ever part of one:

```sh
cactup install master --alias et-master
```

cactup fetches the manifest before resolving `master`, so this is always the
latest commit on that branch, never a stale local copy. The commit it resolved
to is printed with the success message.

`master` is never a default. `cactup install` with no argument still installs
the most recent release, and so does the default offered by the interactive
prompt; you only get master by naming it. Bear in mind what that means: master
moves, so two `install master` runs a week apart are two different thornlists,
and its components can point at in-progress branches that no release has
blessed.

`cactup list` and `cactup inst show` render this installation's release as
`master`. To move it to a newer master later, refetch it — see "Switching
release or thornlist" under "Updating an installation" below.

### Custom paths

Control exactly where cactup installs:

```sh
cactup install ET_2025_05 \
  --alias latest \
  --install-prefix /opt/cactus/ET_2025_05 \
  --symlink-prefix ~/bin \
  --symlink-name cactus
```

This creates:
- Installation at `/opt/cactus/ET_2025_05`
- Symlink at `~/bin/cactus` pointing to the active installation

Use `--no-symlink` to skip the symlink entirely:

```sh
cactup install ET_2025_05 --no-symlink
```

## Listing your installations

See all installed releases:

```sh
cactup list
```

Output shows the alias, release version, installation path, and active marker.

## Viewing installation details

`cactup show` (no arguments) shows cactup's overall state: the active
installation, the active config, and the machine — each best-effort, so a
missing piece doesn't hide the rest. It never takes an alias and never
prompts.

```sh
cactup show
```

For installation-specific detail, use `cactup installation show` (or the
short form `cactup inst show`). With no alias it shows the active
installation; give an alias to inspect any other one:

```sh
cactup installation show
cactup inst show myrelease
```

This displays:
- Release version
- Installation path
- List of built configs
- Active config
- Simulator and test-suite home directories

## Setting the active installation

Make an installation active (the default for `build`, `config`, `sim`, and `test` commands):

```sh
cactup use myrelease
```

You can also override the active installation for a single command:

```sh
cactup --installation myrelease config list
```

## Deleting an installation

Delete an installation:

```sh
cactup inst delete myrelease
```

cactup will ask for confirmation. Skip the prompt:

```sh
cactup inst delete myrelease --force
```

This deletes the Cactus tree, with its configs and builds, and what cactup
keeps beside it in the installation directory; the directory itself goes too
once that leaves it empty. Anything else in it is kept, so an installation
made with `--install-prefix ~/src` takes only `~/src/Cactus` and cactup's own
files with it. The symlink at the default place (`~/Cactus`) is removed while
it still points at the deleted tree; a symlink made elsewhere with
`--symlink-prefix` or `--symlink-name` is left for you to remove. Simulation
and test output under `simulation-home` and `test-home` is kept, but cactup
no longer lists those simulations and test runs.

cactup refuses while a build of one of the installation's configs is running
or queued; stop it first with `cactup build stop`, or pass `--force`. If you
interrupt the deletion, run the same command again to finish it.

## Copying an installation

Copy an installation to a new one, to try something without touching the
original:

```sh
cactup inst cp myrelease experiment
```

The copy lands in `~/.cactup/cacti/experiment` (or wherever your
`install-home` says); pass `--install-prefix` to put it somewhere else. It
gets the whole source tree, with every repo's history, local changes and
commits, the live thornlist and anything else you keep in the Cactus
directory, and the packages `cactup add` put there.

It does not get the configs and their builds: a build records where its
sources were, so a copied one would keep reading the original's files. Build
them again in the copy (`cactup -I experiment build sim`); the build cache
spares most of the compiling. The copy also starts with no simulations or
test runs, and with simulation and test homes of its own, named for its
alias.

If you interrupt the copy, nothing is registered; running the same command
again starts over.

### Symbolic copies

A symbolic copy takes seconds and almost no space, whatever the size of the
original:

```sh
cactup inst cp -s myrelease experiment
```

Each file in it is a link to the same file in `myrelease`, until something
changes it. A refetch or `cactup add` in `experiment`, or a `git checkout` you
run there, gives each file it changes a copy of its own and leaves the rest
as links, so `myrelease` is never touched. `cactup inst list` and
`cactup inst show` say which installations are symbolic copies, and of what.

Two things to know:

- **Edit by hand only what is its own.** An editor may write through a link
  into the original's file. Make the files you are about to edit its own
  first, by path (relative to the Cactus directory, or to where you are):

  ```sh
  cactup inst reify experiment repos/carpetx
  ```

- **It follows the original.** A file `experiment` has not made its own
  changes when `myrelease`'s does, for example when you refetch
  `myrelease`. cactup says so when you do. To keep the copy as it is, make it
  wholly its own first:

  ```sh
  cactup inst reify experiment
  ```

  After that it no longer depends on `myrelease`. Until then cactup refuses
  to delete `myrelease`.

Inside a symbolic copy, `git status` shows each linked file as a type change
(`T`); cactup's own checks (`inst delta`, a refetch's check for local changes)
know a link stands for its file.

## Updating an installation (refetch)

`cactup installation refetch` (or `cactup inst refetch`) re-runs the
component fetch natively — no `GetComponents`, no perl. Use it to pick up
newly added thorns, switch to a different Einstein Toolkit release, or adopt
a different thornlist file entirely, without reinstalling from scratch.

### Picking up thornlist edits

Edit the installation's live thornlist
(`Cactus/thornlists/installation-default.th`), then refetch with no arguments:

```sh
cactup inst refetch
```

This re-reads the live thornlist, updates every repo that's clean, and
clones anything newly referenced that's still missing.

### Switching release or thornlist

Refetch straight to another Einstein Toolkit release — this switches every
repo's branch:

```sh
cactup inst refetch --release ET_2026_11
```

`--release master` does the same for the tip of the manifest's `master` branch,
which is also how you move a master installation forward onto a newer master:

```sh
cactup inst refetch --release master
```

Or adopt a thornlist file from somewhere else:

```sh
cactup inst refetch path/to/custom.th
```

### The skip contract

A repo with local modifications, local commits, a switched branch, a
detached HEAD, an in-progress rebase/merge, or a changed remote URL is left
alone and reported by name (naming the affected thorns) rather than
silently overwritten:

```sh
cactup inst refetch
# 2 repo(s) skipped (local state preserved).
#   McLachlan — local modifications (2 files)
#     thorns: McLachlan/ML_BSSN, McLachlan/ML_ADMConstraints
```

- `--overwrite-modified` (or `-f`) fetches over skipped repos anyway, after
  backing their modified files up to
  `~/.cactup/refetch-backups/<alias>/<timestamp>/`. `--overwrite <NAMES>`
  does the same for just the named repos — a repo directory under
  `Cactus/repos/`, a full thorn checkout it provides, or a bare thorn name
  (case-insensitive; space- or comma-separated, repeatable) — leaving every
  other skipped repo's local state untouched.
- A repo skipped for a changed remote URL is compared canonically, so
  spelling alone (`git@github.com:user/repo.git` vs.
  `https://github.com/user/repo`) never trips it — only a genuinely
  different upstream does. Forcing that repo (`-f`/`--overwrite-modified`,
  or naming it in `--overwrite`) rewrites its `origin` to the thornlist's
  URL before fetching, which is how you adopt a fork: point the thornlist
  at your own fork of a component, then `refetch --overwrite <that repo>`.
- `-s`/`--silent` hides the detailed skip list; a one-line count still
  prints.
- `-f` is the "bypass all nagging" umbrella: it implies
  `--overwrite-modified` and `--replace-thornlist`, and skips the
  `--prune` confirmation prompt. It does **not** imply `--prune` — pruning
  is always opt-in.

### Replacing a hand-edited live thornlist

When you refetch to an explicit source (`--release` or a thornlist path),
cactup refuses to overwrite a hand-edited live
`Cactus/thornlists/installation-default.th` — detected by diffing it against
the pristine as-fetched copy at `<installation root>/installation-source.th`.
Pass `--replace-thornlist` (or `-f`) to proceed anyway; the old live file is
snapshotted first, to `<installation root>/.cactup/thornlists/<timestamp>.th`.

(An installation created before these two files took their current names —
they used to share one name, `einsteintoolkit.th` — is upgraded to the new
names automatically, the first time any `cactup` command touches it; nothing
to do by hand.)

### Pruning removed thorns

`--prune` removes repos and arrangement symlinks the (new) thornlist no
longer mentions — but only repos cactup itself fetched, as recorded in
`<installation root>/.cactup/fetch-state.toml`. Anything hand-placed is
reported and never touched. cactup asks for confirmation once (`-f` skips
it); dirty or untracked orphans additionally require `-f` and are backed up
first.

### Previewing changes

`-n`/`--dry-run` prints the full classification of what a refetch would do
— fetched, skipped, pruned — and touches nothing.

After `refetch --release RELEASE` or `refetch FILE`, `cactup list` and
`cactup inst show` render "now on `<TAG>`" next to the install-time release,
so you can tell an installation has moved on.

### Partial adoption

If the skip contract left some repos untouched — or a fetch simply failed —
the new thornlist is still adopted: cactup now tracks the installation as
being on `<TAG>` as far as that refetch could actually manage, rather than
silently keeping the old provenance. But it also remembers exactly which
repos didn't make it over — and *why*, because the two causes mean different
things. A **skipped** repo is a supported workflow: you have local work
there, and refetch preserved it on purpose. A **failed** repo is an error —
you asked for it and didn't get it — so it is reported first, in red, with
its own remedy. `show` calls all of this out instead of reporting clean
conformance:

```sh
cactup inst show myrelease
# release:      ET_2026_05
# now on:       ET_2026_11 (refetched)
# conformance:  INCOMPLETE: 1 repo(s) FAILED to fetch, 1 skipped; 4 thorn(s) differ from the thornlist.
#                 Failed:
#                   carpetx — connection reset by peer
#                     CarpetX/Algo, CarpetX/BoxUtils
#                 Skipped:
#                   mclachlan — local commits
#                     McLachlan/ML_BSSN, McLachlan/ML_ADMConstraints
#                 Retry the failed repo(s) with `cactup inst refetch`.
#                 Fetch over the skipped ones with `cactup inst refetch -f` (backs up modified files).
#                 See what differs with `cactup inst delta`.
```

When nothing failed the header softens to `PARTIAL: N repo(s)
skipped; …` and only the yellow skipped group appears. At most six thorns
are named per repo and eight repos in total, each capped with a `+N more`
tail; `cactup inst delta` has the unabridged picture.

`cactup list` appends the shorter `, partial (2 repo(s) not fetched)` — or,
when anything failed, `, partial (1 repo(s) FAILED, 1 skipped)` in red — and
`cactup config show`'s live-thornlist provenance line gets the matching
`(partial: …)` suffix. Refetch itself doesn't adopt a partial thornlist
quietly either — it prints a "Partial adoption" block naming the same repos
at the moment it happens, with the same FAILED/skipped split.

The marker clears itself the next time a refetch manages to fetch every repo
the thornlist names — `cactup inst refetch -f` backs up the dirty repos'
modified files first and then fetches over them, or you can clean the repos
by hand and refetch normally.

> [!NOTE]
> Refetching does not rebuild anything by itself, but it does not have to be
> followed by `-f` either. Every build records the commit each of that config's
> repos was on, so a plain `cactup build <name>` afterwards notices that the
> sources moved and recompiles what that affects. Moving the Cactus flesh —
> which a release change always does — is classified as a from-scratch rebuild
> automatically. A config built with a custom `--thornlist` still builds the
> thorn *set* from its own list, but its sources are refetched and recompiled
> like any other. See
> [Refetching and rebuilding](building-configs.html) for the full table.

### What has changed since the last fetch

```sh
cactup inst delta              # the active installation
cactup inst delta myrelease    # a named one
```

`cactup installation delta` compares the source trees against what the last
fetch left behind: repos now on a different branch or commit, files you have
modified, and untracked files (which builds ignore but `--prune` does not).
It only reads — nothing is fetched, locked, or written — so it is the quickest
way to see what a refetch would have to contend with before running one.

For the other baseline — what has changed since a *config was built*, and what
rebuilding would cost — use `cactup config delta`.

{{cactup:cli command="installation refetch"}}

{{cactup:cli command="installation delta"}}

{{cactup:cli command="installation delete"}}

{{cactup:cli command="installation cp"}}

{{cactup:cli command="installation reify"}}

## Global flags for installation commands

{{cactup:cli command="install"}}

{{cactup:cli command="list"}}

{{cactup:cli command="installation show"}}

{{cactup:cli command="use"}}

## Installation locations

By default, cactup uses:

- **Install prefix**: `~/.cactup/cacti/` (can be overridden with `--install-prefix`)
- **Symlink**: `~/Cactus` — i.e. prefix `~` (home) and name `Cactus` by default (customize with `--symlink-prefix` and `--symlink-name`, or skip with `--no-symlink`)
- **Simulation home**: `~/.cactup/simulations/` (can be configured in your machine definition)
- **Test-suite home**: `~/.cactup/tests/` (can be configured in your machine definition)

See your machine's configuration:

```sh
cactup machine show
```

## Next steps

Once you've installed a release, proceed to:
- [Building Configs](building-configs.html) — compile Cactus for your machine
- [Running Simulations](running-simulations.html) — create and run a simulation
- [Test Suites](test-suites.html) — validate your build
