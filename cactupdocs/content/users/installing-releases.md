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

## Uninstalling

Remove an installation:

```sh
cactup uninstall myrelease
```

cactup will ask for confirmation. Skip the prompt:

```sh
cactup uninstall myrelease --force
```

This removes the installation directory and any symlinks, but preserves simulation data under `simulation-home`.

## Updating an installation (refetch)

`cactup installation refetch` (or `cactup inst refetch`) re-runs the
component fetch natively — no `GetComponents`, no perl. Use it to pick up
newly added thorns, switch to a different Einstein Toolkit release, or adopt
a different thornlist file entirely, without reinstalling from scratch.

### Picking up thornlist edits

Edit the installation's live thornlist
(`Cactus/thornlists/einsteintoolkit.th`), then refetch with no arguments:

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

Or adopt a thornlist file from somewhere else:

```sh
cactup inst refetch path/to/custom.th
```

### The skip contract

A repo with local modifications, local commits, a switched branch, a
detached HEAD, or an in-progress rebase/merge is left alone and reported by
name (naming the affected thorns) rather than silently overwritten:

```sh
cactup inst refetch
# 2 repo(s) skipped (local state preserved).
#   McLachlan — local modifications (2 files)
#     thorns: McLachlan/ML_BSSN, McLachlan/ML_ADMConstraints
```

- `--overwrite-modified` (or `-f`) fetches over skipped repos anyway, after
  backing their modified files up to
  `~/.cactup/refetch-backups/<alias>/<timestamp>/`.
- `-s`/`--silent` hides the detailed skip list; a one-line count still
  prints.
- `-f` is the "bypass all nagging" umbrella: it implies
  `--overwrite-modified` and `--replace-thornlist`, and skips the
  `--prune` confirmation prompt. It does **not** imply `--prune` — pruning
  is always opt-in.

### Replacing a hand-edited live thornlist

When you refetch to an explicit source (`--release` or a thornlist path),
cactup refuses to overwrite a hand-edited live
`Cactus/thornlists/einsteintoolkit.th` — detected by diffing it against the
pristine as-fetched copy at `<installation root>/einsteintoolkit.th`. Pass
`--replace-thornlist` (or `-f`) to proceed anyway; the old live file is
snapshotted first, to `<installation root>/.cactup/thornlists/<timestamp>.th`.

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

After `refetch --release TAG` or `refetch FILE`, `cactup list` and
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
# conformance:  INCOMPLETE — 1 repo(s) FAILED to fetch and 1 repo(s) were skipped; 4 thorn(s) on disk do not match the thornlist above.
#                 Failed:
#                   carpetx — connection reset by peer
#                     CarpetX/Algo, CarpetX/BoxUtils
#                 Skipped:
#                   mclachlan — local commits
#                     McLachlan/ML_BSSN, McLachlan/ML_ADMConstraints
#                 Retry the failed repo(s) with `cactup inst refetch`.
#                 Fetch over the skipped ones with `cactup inst refetch -f` (modified files are backed up first); `cactup inst delta` shows what differs.
```

When nothing failed the header softens to `PARTIAL — N repo(s) were
skipped, …` and only the yellow skipped group appears. At most six thorns
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

## Global flags for installation commands

{{cactup:cli command="install"}}

{{cactup:cli command="list"}}

{{cactup:cli command="installation show"}}

{{cactup:cli command="use"}}

{{cactup:cli command="uninstall"}}

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
