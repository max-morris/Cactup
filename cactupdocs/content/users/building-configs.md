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

On most machines that's the whole story: `make` runs right there in your
terminal. Some clusters forbid compiling on the login node, though, and on
those `cactup build` does something different — see the next section.

## Foreground vs. queued builds

Some clusters require building on a compute node, the same way they require
*running* on one. `cactup build` handles both cases with the same command:

```sh
cactup build myconfig
```

- On a machine with no queued-build support, this compiles right here,
  in the foreground, exactly like the basic build above.
- On a machine that requires (or defaults to) building on the queue, this
  **submits** the build as a batch job instead, prints the job id, and
  returns immediately — your terminal is free while `make` runs on a
  compute node.

Which one happens is decided by the machine's definition, not by anything
you typed — see [meta.toml Reference](meta-toml.html#mdb-build) for exactly
how a machine opts into queued builds. If you want to force one or the other
regardless of what the machine prefers:

```sh
cactup build run myconfig       # always foreground, even if the machine can submit
cactup build submit myconfig    # always submit — errors if the machine can't
```

`build submit` accepts `--follow` to stream the build's output and block
until it finishes, instead of returning as soon as the job is queued:

```sh
cactup build submit myconfig --follow
```

It also accepts `--block`, which waits for the queued build to finish
before returning but — unlike `--follow` — prints no build output along the
way; just the job id up front and the final verdict. Use `--block` in a
script that must not continue until the build is done, and `--follow` when
you want to watch it happen. The two are mutually exclusive. Either way,
Ctrl-C only detaches your terminal — the queued build keeps running and you
get its job id back:

```sh
cactup build submit myconfig --block
```

Every other build flag — `--variant`, `--universe`, `--optimize`, `-j`, and
so on — works identically whether the build runs in the foreground or on the
queue; the queue is a detail of *where* `make` runs, not of what gets built.
`build submit` also accepts the same topology flags as `sim submit`/`sim
run` (`-q`/`--queue`, `-w`/`--wall-time`, `-a`/`--allocation`, `-n`/`--nodes`,
and so on — see [Running Simulations](running-simulations.html)) for
overriding the machine's default build-job shape on a one-off basis. Note
that `-j` still means `--make-jobs` here, not job name — use `-J`/`--job-name`
if you need to set that.

A queue-submitted build behaves like any other queued job: it can sit
waiting for resources, and if you close your terminal or lose your
connection, the build keeps running. Nothing about the *result* is
scheduler-dependent, though — see the next section.

### Where build output goes

Every build — foreground or queued — creates a numbered **build attempt**
under the config's own directory:

```
<Cactus root>/configs/myconfig/.cactup-builds/
  0001/
    build.toml    # what was built, with what flags, and the outcome
    build-script  # the frozen build steps this attempt ran
    build.out     # make's stdout
    build.err     # make's stderr
  0002/
    ...
```

There's no separate build log file to go hunting for — `build.out`/
`build.err` are always at a predictable path, and `cactup build log`
(covered in [Monitoring & Logs](monitoring.html)) reads them for you without
you needing to know the attempt number. The highest-numbered attempt is
always the one that matters: the in-progress one if a build is currently
running or queued, the most recent result otherwise.

The build attempt's own record — not the scheduler's job exit status — is
what determines success or failure. A batch job can "succeed" (exit 0)
without the build actually finishing, so cactup checks completeness itself
and writes that verdict into `build.toml`; `cactup build show` reports what
cactup found, not what the scheduler thinks happened.

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

For a brand-new config, cactup defaults to the installation's built-in
thornlist (`<Cactus root>/thornlists/installation-default.th`). After that, the
config remembers the thornlist it was built from, so you don't have to repeat
`--thornlist` on every rebuild — pass it again only to switch to a different
file.

That built-in file is named `installation-default.th` whatever it holds: on a
custom installation it holds your custom list's thorns, and the filename alone
won't tell you which. So `cactup config show` annotates the `thornlist:` line
with its provenance:
whether the path is the installation's live list (and what that list actually
is — a release, or a custom file), was recorded from an explicit
`--thornlist`, or points at a file that has since disappeared (in which case
rebuilds fall back to the config's snapshot).

#### Editing a thornlist and rebuilding

Edit the **source** thornlist — the file you passed to `--thornlist`, or the
installation's `thornlists/installation-default.th`. Then rebuild:

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
- `<installation root>/installation-source.th` — the pristine as-fetched copy
  of the thornlist, written at install time and updated by `cactup
  installation refetch`. It's the baseline cactup diffs the live thornlist
  against to detect hand edits; don't edit it directly.

(Both names are recent: the pristine and the live copy used to share one name,
`einsteintoolkit.th`, which is why it was never obvious which was which. An
installation created before the rename is upgraded to the new names
automatically, the first time any `cactup` command touches it, so there's
nothing to do by hand.)

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
  `Cactus/thornlists/installation-default.th`; a config built with an explicit
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
| A thorn's body code edited in place (a `.cc`, `.F90`, header, …) | reconfigure + rebuild what the edit affects; that thorn's `build/<Thorn>/` is left alone |
| A thorn's *shape* changed — a file added or removed, or a `.ccl` / `make.code.defn` / `make.configuration.defn` / `make.code.deps` edited | that thorn's `build/<Thorn>/` and `libthorn_<Thorn>.a` are deleted, then rebuilt from scratch |
| A thorn repo on a different commit | reconfigure + rebuild what that affects |
| The **Cactus flesh** on a different commit | a from-scratch rebuild — the make system and everything `config-data` is generated from have changed |

Editing the flesh *in place* is deliberately **not** escalated to a
from-scratch rebuild: `make` recompiles what the edit affects, and a realclean
would be a brutal price for iterating on flesh code. Pass `-f` when you want
one anyway.

The same trust-but-verify split applies one level down, per thorn. Ordinary
body-code edits are left to `make`'s own `.d` dependency tracking, which gets
them right — there's no reason to punish the edit-and-rebuild loop by
discarding a thorn's object files every time a line changes. But `make` can't
be trusted with a change to a thorn's *shape*: if a `.ccl` drops a
`REQUIRES`, Cactus deletes the now-unneeded bindings header, yet the stale
`build/<Thorn>/*.d` still lists it as a prerequisite, and make dies with "No
rule to make target" instead of rebuilding. And if a source file is removed
from a thorn, its leftover `.o` isn't recompiled away — Cactus updates
`libthorn_<Thorn>.a` with `ar`, which only adds and replaces members, so the
orphan object keeps linking in silently. Both failures require deleting that
thorn's build state outright, which is why a shape change gets a harder reset
than a body edit. As with source tracking, a config built before this landed
has no recorded shape baseline; its first build afterward records one and
invalidates nothing, and every build after that is detected.

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

This skips `configure` and `make` entirely, useful when the executable was built elsewhere. It's a plain file copy, not a build, so it's rejected together with `build submit` — and, for the same reason, with `--block`, which has nothing queued to wait for — combine it with `build run` (or the plain `cactup build` foreground form) instead.

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

> [!NOTE]
> A **universe** and a **queued build** (above) answer different questions,
> and a machine can use either, both, or neither. A universe says *where*
> `make` runs — inside a container or module-loaded shell, versus bare on the
> host. Queued builds say *how the build reaches a machine at all* — through
> the batch scheduler, versus running immediately in your terminal. A machine
> that requires compute-node builds isn't declaring a universe; it's declaring
> that `make` has to go through the same queue a simulation would.

## Viewing build status

See all configs in the active installation, with each one's build status:

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

`config show`/`config list` describe the **config** — what it's built from,
its variant and flags, whether it's complete. For the **build attempt**
itself — whether one is currently queued or running, its job id, its output —
use `cactup build show`/`cactup build list`/`cactup build log`, covered in
[Monitoring & Logs](monitoring.html).

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

{{cactup:cli command="build run"}}

{{cactup:cli command="build submit"}}

{{cactup:cli command="config list"}}

{{cactup:cli command="config show"}}

{{cactup:cli command="config use"}}

{{cactup:cli command="config delete"}}

{{cactup:cli command="config delta"}}

## Troubleshooting

**"Must specify --variant"**: Your machine has multiple optionlist variants. Use `cactup machine show --variants` to list them, then `--variant <name>`.

**"Variant not found"**: Check the spelling with `cactup machine show --variants`.

**Build fails**: `cactup build show myconfig` reports the most recent attempt's outcome, and `cactup build log myconfig` shows its output. Both find the right attempt automatically — there's no build log path to remember (see "Where build output goes" above).

**"Universe not found"**: If the machine defines optional universes, use `cactup machine show` to see available ones.

**"cactup build submit is not possible on this machine"**: the machine needs both a `buildsubmitscript` variant and a scheduler `submit` command declared before it can queue a build — see [meta.toml Reference](meta-toml.html) or ask whoever ported the machine. `cactup build run` always works regardless.

**"--block waits for a queued build to finish, but this build runs in the foreground"**: `--block` only makes sense for a build that's going to the queue, and a bare `cactup build` resolved to the foreground instead — because you passed `--virtual-executable`, because the machine sets `[build].default-action = "run"`, or because it can't submit builds at all. The rest of the message names which one applies. Drop `--block`, or use `cactup build submit --block` on a machine that can queue the build. (`cactup build run --block` gets its own, blunter version of the same complaint: that command never queues anything.)

## Next steps

- [Running Simulations](running-simulations.html) — submit or run a simulation with your built config
- [Test Suites](test-suites.html) — validate your config against the test suite
- [Monitoring & Logs](monitoring.html) — track a queued build, tail its output, stop or prune old attempts
