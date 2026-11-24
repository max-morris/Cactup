+++
title = "Publishing Packages"
description = "Write a cactarium.toml for a repository with cactup package init, register it with Cactarium, and write a stack for an installation"
+++

# Publishing Packages

A **package** is a repository of thorns registered with Cactarium. What it
is and what it needs is written in a `cactarium.toml` at the root of the
repository, read by Cactarium at every release tag. cactup writes a first
version of it from your installation, for you to review:

```sh
cactup package init z4c          # writes repos/z4c/cactarium.toml
cactup package init              # the repository holding the current directory
```

`z4c` is the repository's name under the installation's `repos/`. cactup
reads the repository's thorns and the active config's build, so build the
config first (`cactup build`).

## What it writes

```toml
format = 1

[package]
name = "z4c"
owners = ["max"]

[[dependency]]
target = "et/carpetx@~1.0.0"
needs = ["Loop"]
needed-by = ["Z4c"]
```

- **`name`** is the repository's name, lowercased (`--name` sets another).
- **`owners`** is your Cactarium user, as the token stored by
  `cactup login` says (`--owner` sets another).
- **`root`** and **`arrangement`** are written when the thorns are not where
  Cactarium looks for them by default: deeper in the repository than
  `Arrangement/Thorn/`, or in folders that do not name the arrangement they
  are built in.
- **A dependency** is written for each requirement of the thorns' CCL files
  (an implementation they inherit from, a capability, a function, a thorn,
  a header they cannot compile without) that no thorn of the repository
  meets and exactly one installed thorn does: the one the config built,
  when the build has it. Its package is the dependency's target, its thorn
  is in `needs`, and the repository's thorns that require it are in
  `needed-by`, always written out. A package whose thorns serve different
  thorns of yours gets one entry per group.

## Ranges

Each range starts at the version the config was **built with**:
`~1.0.0` accepts 1.0.0 and later patch releases (`mixed-precision:~1.2.0`
on a track). `--exact` writes `=1.0.0` instead. When the config's build did
not record its sources (built by an older cactup), the version is the one
the repository's clone is at now.

## TODO lines

Whatever cactup cannot decide is a comment starting with `# TODO:`, never a
dependency or a claim. Resolve each before you commit the file; cactup says
how many there are.

- **Several providers.** Carpet and PUGH both implement `driver`. The TODO
  lists them as a commented `any-of`; keep the one your thorns need, or all
  of them.
- **A provider that is no released package**: in a repository nobody
  registered, or at a commit no version froze.
- **Nothing provides it** in the installation.
- **Thorns the config does not build**, with a commented `exclude-thorns`:
  Cactarium publishes only what builds.
- **An `#include` into another thorn's directory**
  (`#include "../../Loop/src/loop.hxx"`): code the CCL does not declare.
- **A thorn that inherits the generic `driver`** but includes a header only
  one driver provides (`carpet.hh`): it may need that driver.
- **A flesh fork in use**: these thorns may need it, which no CCL can say.
  The dependency on it is written out, commented.
- **A fork** that lacks some of the upstream version's thorns (see below).
- **A shallow clone**: cactup clones at depth 1, so it cannot read whether
  the repository forks a package. Fetch its history
  (`git fetch --unshallow` in the clone), then run `cactup package init -f`
  again.

The installation's claim overrides (`cactup claims`) are copied in as
comments: they are about other packages' claims, not this one's.

## Forks

When the repository's history contains the frozen commit of a version of an
installed package that shares a thorn name with it (or of the package it
replaced with `cactup add --replace`), and the repository has every thorn
of that version, cactup writes

```toml
[[fork-of]]
target = "et/carpet@=1.0.0"
```

with the newest such version, exactly: a fork has none of the patches
released after it was forked.

## Overwriting

An existing `cactarium.toml` is never overwritten unless you pass `-f`.

## Registering

Commit and push the file, then:

```sh
cactup package init z4c --publish
```

`--publish` writes nothing. It refuses a `cactarium.toml` that is not
committed, has uncommitted changes, or is on no remote branch, and prints
the git commands that fix it. The repository's URL must be one anybody can
fetch: no credentials in it, no local path, and it must answer anonymously
(an ssh remote is registered as its https form). Then Cactarium registers
the package under your own name, or under one of your organizations with
`--owner`, starting from the repository's release tags. A package
registered already is checked for new tags instead, as `cactup refresh`
does. New versions may wait for an admin's review.

## A stack for the installation

```sh
cactup stack init                 # writes ./cactarium-stack.toml
cactup stack init stacks/mine     # in another directory
```

The installation must be on a stack release (`cactup install
owner/stack@version`, or `cactup inst refetch --release`); a branch tip
(`@head`) has no release to extend. Like `sim export`, it refuses
repositories with uncommitted changes or commits on no remote. The stack:

- **extends** the release the installation is on;
- **pins** each base repository moved since that release;
- **overrides** a base repository with each package added with
  `cactup add --replace`, at its exact version;
- adds every other added package as a pinned section of
  `cactarium-stack.th`, written beside it. A package added at a branch tip
  or at a commit no version froze is pinned at the commit installed, never
  at a version;
- lists, as `[[dependency]]` entries, what the installed packages declare
  they depend on, each with a comment naming the package that declares it.
  These are information: they say what could be swapped in with a good
  chance of the stack still working. Installing a stack never resolves
  them.

Publish it as a release with

```sh
cactup stack init --publish --version 1.0.0
```

which writes nothing and uploads the `cactarium-stack.toml` found there
(and its thornlist) as `owner/name@1.0.0`, the owner being the first in its
`owners` unless `--owner` names another.
