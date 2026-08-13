+++
title = "Building Configs"
description = "Build Cactus configurations with variants, universes, and customized compiler settings"
+++

# Building Cactus Configurations

A Cactus **configuration** is a compiled Cactus executable built from your installation with specific thorns, compiler settings, and optionlist variant. This guide covers everything you need to know about building configs.

## Basic build

Build a config with default settings:

```sh
cactup build myconfig
```

This creates a config named `myconfig` using:
- The active Einstein Toolkit installation
- The machine's default optionlist variant
- Default compiler flags: **optimized** (optimization is the one build flag that is on by default), no debugging, no profiling

## Build options

### Optimization and debugging

These flags set Cactus's own build switches in the rendered optionlist
(`OPTIMISE`, `DEBUG`, `PROFILE`, `UNSAFE`); the concrete compiler flags each one
implies come from the machine's optionlist:

```sh
cactup build myconfig --optimize   # OPTIMISE=yes (on by default already)
cactup build myconfig --debug      # DEBUG=yes — debug symbols and checks
cactup build myconfig --profile    # PROFILE=yes — profiling support
cactup build myconfig --unsafe     # UNSAFE=yes — unsafe optimizations; use with care
```

### Parallel builds

Control how many parallel `make` jobs to use:

```sh
cactup build myconfig -j 8         # 8 parallel jobs
cactup build myconfig -j max       # All available threads
cactup build myconfig              # Machine default (usually 1)
```

The `-j max` option is useful in batch environments: it uses all threads available in the current compute context (e.g., an `srun` allocation), not just the login node.

### Reconfiguration and cleaning

Force a full rebuild:

```sh
cactup build myconfig --reconfig --clean --force
```

- `--reconfig` — Re-run `configure` before building
- `--clean` — Run `make clean` before building
- `--force` — Rebuild even if the config is already built

### Thornlist

Use a custom thornlist:

```sh
cactup build myconfig --thornlist /path/to/custom.th
```

For a brand-new config, cactup defaults to the installation's built-in thornlist
(`<Cactus root>/thornlists/einsteintoolkit.th`). After that, the config remembers
the thornlist it was built from, so you don't have to repeat `--thornlist` on
every rebuild — pass it again only to switch to a different file.

#### Editing a thornlist and rebuilding

Edit the **source** thornlist — the file you passed to `--thornlist`, or the
installation's `thornlists/einsteintoolkit.th`. Then rebuild:

```sh
cactup build myconfig
```

cactup notices the change and reconfigures. Adding or removing a thorn does
*not* force a from-scratch rebuild, so this is usually quick; use `--force` if
you want everything recompiled anyway.

Do **not** edit the two thornlists inside `configs/<name>/`. Both are derived
copies, rewritten on every build, so your edits there would be overwritten:

- `cactup-thornlist.th` — cactup's processed copy, with the machine's
  `disabled-thorns`/`enabled-thorns` toggles applied. This is what cactup hands
  to Cactus.
- `ThornList` — Cactus's own copy of that file, made by its build system. This
  is the one Cactus compiles from.
- `<installation root>/einsteintoolkit.th` — the pristine as-fetched copy of
  the thornlist, written at install time and updated by `cactup installation
  refetch`. It's the baseline cactup diffs the live thornlist against to
  detect hand edits; don't edit it directly.

A third file, `cactup-thornlist.src.th`, is a verbatim snapshot of the source
thornlist. It exists so the config stays rebuildable if the original file is
later moved or deleted: cactup falls back to the snapshot (with a warning) rather
than silently building the stock thornlist instead. It is a safety net, not an
edit target — edits to it are picked up only while the original is missing.

Creating a simulation copies both thornlists into the simulation's
`.cactup/cfg/`, so you can always tell which thorns a given run's executable was
built with — even after the config has been rebuilt or deleted.

#### Refetching and rebuilding

A config is built from two things, and cactup tracks changes to both:

- **Which thorns get built** comes from the thornlist. A config with no
  `--thornlist` override builds from the installation's live
  `Cactus/thornlists/einsteintoolkit.th`; a config built with an explicit
  `--thornlist` keeps building from that same file, wherever it lives.
- **What those thorns are built from** comes from the source repos under
  `Cactus/repos/`. At every build, cactup records the commit each of that
  config's repos was on and whether its worktree had local modifications, then
  compares that against the tree at the next build.

The second half is what makes ordinary source work visible. All of these are
picked up by a plain `cactup build <name>`, with no `-f`:

- **You edited a thorn in place.** Same commit, changed files — the everyday
  workflow of hacking on a thorn and rebuilding.
- **You checked out a different branch or commit inside a repo.**
- **A refetch moved the repos** (see
  [Installing Releases](installing-releases.html)), even though it usually
  leaves the thornlist byte-for-byte identical.

What each costs:

| What changed | What `cactup build <name>` does |
|---|---|
| Nothing under this config | short-circuits: "up to date" |
| A thorn edited in place | reconfigure + rebuild what the edit affects |
| A thorn repo on a different commit | reconfigure + rebuild what that affects |
| The **Cactus flesh** on a different commit | a from-scratch rebuild — the make system and everything `config-data` is generated from have changed |

Editing the flesh *in place* is deliberately **not** escalated to a
from-scratch rebuild: `make` recompiles what the edit affects, and a realclean
would be a brutal price for iterating on flesh code. Pass `-f` when you want
one anyway.

Untracked files are ignored throughout — the test harness leaves output inside
the source tree, and that must never read as a source change.

#### Seeing what diverged

```sh
cactup config delta            # the active config
cactup config delta mp         # a named one
```

`cactup config delta` reports how the source trees differ from what that config
was last built with — repos now on another commit, repos edited in place, the
files involved (`--verbose` for all of them) — and what a rebuild would do
about it. It only reads; it never builds.

Its sibling `cactup installation delta` uses a different baseline: the last
**fetch** rather than the last build. Use that one to answer "what have I
changed since cactup put these sources here", including untracked files, which
builds ignore but `refetch --prune` does not.

> [!NOTE]
> A config built by a cactup older than source tracking has no recorded
> baseline. The first `cactup build` establishes one — including when it
> short-circuits as up to date — after which every later change is detected.
> `cactup config delta` says so explicitly when a config is in that state.

An explicit-source refetch (`--release` or a thornlist path) also refuses to
overwrite a hand-edited live thornlist unless you pass `--replace-thornlist`
(or `-f`) — see [Installing Releases](installing-releases.html) for the full
refetch contract.

### Virtual executables

For testing or special cases, copy a prebuilt `cactus_<config>` into place instead of building:

```sh
cactup build myconfig --virtual-executable /path/to/cactus_myconfig
```

This skips `configure` and `make` entirely, useful when the executable was built elsewhere.

## Optionlist variants

Your machine may provide multiple **optionlist variants** — different compiler environments or configurations. For example:

- `default` — CPU build
- `cuda` — GPU/CUDA build
- `intel` — Intel compiler variant

List available variants:

```sh
cactup machine show --variants
```

Build with a specific variant:

```sh
cactup build myconfig --variant cuda
```

If your machine has multiple variants, you must choose one explicitly (unless one is marked default). Each variant can have different compiler flags, GPU support, and compatible queues for job submission.

## Universes

A **universe** is an optional build environment that wraps the build process. Common examples:

- **Singularity container** — executes `make` inside a container image
- **Module-loaded shell** — sources module files before building
- **Native host** — no wrapping (default)

If your machine defines universe options, use:

```sh
cactup build myconfig --universe et-sif
```

Or force the native host context:

```sh
cactup build myconfig --no-universe
```

The universe is **recorded** with your config and affects how simulations are submitted and run. See [Running Simulations](running-simulations.html) for how universe affects job submission.

## Viewing build status

See all configs in the active installation:

```sh
cactup config list
```

View details of a specific config:

```sh
cactup config show myconfig
```

Set the active config (the default for `sim` and `test` commands):

```sh
cactup config use myconfig
```

## Deleting configs

Remove a config and its build artifacts:

```sh
cactup config delete myconfig
```

By default, cactup prevents deletion if the config is attached to any simulations. Force deletion:

```sh
cactup config delete myconfig --force
```

This removes the config but does not touch existing simulations (though they may become unable to run).

## Understanding build variants and universes

**Variants** and **universes** allow machines to support diverse build environments without fragmenting the MDB (machine database). Here's how they work:

### Optionlist variants

An **optionlist** is a TOML file specifying compiler flags, optimization levels, and enabled thorns. A machine can have multiple variants:

- Each variant has a name (`default`, `cuda`, etc.) and a description
- You choose which variant when building with `--variant`
- The variant determines the compiler environment and enabled features
- Variants are configured per-machine in the machine definition

### Universes

A **universe** is an optional build wrapper (Singularity, modules, etc.):

- The optionlist can specify a default universe, or you can override with `--universe`
- `--no-universe` forces native (host) compilation
- The recorded universe affects how simulations run — they run in the same universe they were built in
- Useful for machines that support both container-based and native builds with different optimizations

### Common patterns

**Single build environment**: Most machines have one optionlist variant and no universe wrappers. Just run:

```sh
cactup build myconfig
```

**GPU vs CPU**: Machines with both CPU and GPU queues often have two optionlist variants:

```sh
cactup build cpu-config --variant default
cactup build gpu-config --variant cuda
```

**Container + native**: Some machines support both Singularity and native builds:

```sh
cactup build container-build --universe et-sif
cactup build native-build --no-universe
```

## Full build command reference

{{cactup:cli command="build"}}

{{cactup:cli command="config build"}}

{{cactup:cli command="config list"}}

{{cactup:cli command="config show"}}

{{cactup:cli command="config use"}}

{{cactup:cli command="config delete"}}

{{cactup:cli command="config delta"}}

## Troubleshooting

**"Must specify --variant"**: Your machine has multiple optionlist variants. Use `cactup machine show --variants` to list them, then `--variant <name>`.

**"Variant not found"**: Check the spelling with `cactup machine show --variants`.

**Build fails**: Check the build log in the config directory. Use `cactup show` to find the installation path, then look for `configs/myconfig/` inside it.

**"Universe not found"**: If the machine defines optional universes, use `cactup machine show` to see available ones.

## Next steps

- [Running Simulations](running-simulations.html) — submit or run a simulation with your built config
- [Test Suites](test-suites.html) — validate your config against the test suite
