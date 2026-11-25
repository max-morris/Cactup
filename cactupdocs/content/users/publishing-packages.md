+++
title = "Publishing Packages"
description = "Write a cactarium.toml for a repository with cactup package init, register it with Cactarium, and write a stack for an installation"
+++

# Publishing Packages

A **package** is a repository of thorns registered with Cactarium. What it
is and what it needs is written in a `cactarium.toml` at the root of the
repository, read by Cactarium at every release tag. Others add it with
`cactup add`, which brings in what it needs (see
[Sharing Simulations](sharing-simulations.html#dependencies)). A shared `.sim` can
pin only repositories of registered packages. cactup writes a first version
of the file from your installation, for you to review:

```sh
cactup package init z4c          # writes repos/z4c/cactarium.toml
cactup package init              # the repository holding the current directory
```

`z4c` is the repository's name under the installation's `repos/`. cactup
reads the repository's thorns and the active config's build, so build the
config first (`cactup build`). A config built with `--virtual-executable`
has no `ThornList`, so `package init` needs a real build of it.
`package init` maps to registered packages only the repositories whose URLs
others can fetch, so a repository cloned from a local path is never matched
to a registered package.

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
- **`owners`** is your Cactarium user, as named by the token stored by
  `cactup login` (`--owner` sets another).
- **`root`** and **`arrangement`** are written when the thorns are not
  where Cactarium looks for them by default: deeper in the repository than
  `Arrangement/Thorn/`, or in folders that do not name the arrangement
  used by the build.
- **A dependency** is written for each requirement of the thorns' CCL files
  (an implementation they inherit from, a capability, a function, a thorn,
  a header they cannot compile without) that no thorn of the repository
  meets and exactly one installed thorn does: the one in the config's
  build, when the build has it. Its package is the dependency's target,
  its thorn is in `needs`, and the repository's thorns that require it are
  in `needed-by`, always written out. A package whose thorns serve
  different thorns of yours gets one entry per group. A package never
  depends on itself: a requirement met only by another clone of the same
  package is a TODO.

## Ranges

Each range starts at the version used by the config's **build**: `~1.0.0`
accepts 1.0.0 and later patch releases (`mixed-precision:~1.2.0` on a
track). `--exact` writes `=1.0.0` instead. When the build recorded no
sources (built by an older cactup), the version is the one at the head of
the repository's clone.

## TODO lines

Whatever cactup cannot decide is a comment starting with `# TODO:`, never a
dependency or a claim. Resolve each before you commit the file; cactup says
how many there are.

- **Several providers.** Carpet and PUGH both implement `driver`. The TODO
  lists them as a commented `any-of`; keep the one needed by your thorns,
  or all of them.
- **A provider that is no released package**: in a repository nobody
  registered, or at a commit no version froze, or at a version cactup
  cannot read.
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
- **A shallow clone**: cactup clones at depth 1, so the history cannot show
  whether the repository forks a package. cactup prints the commands:
  `git -C <clone> fetch --unshallow`, then `cactup package init <repo> -f`.
  It never changes the clone itself.

The installation's claim overrides (`cactup claims`, see
[Sharing Simulations](sharing-simulations.html#when-a-claim-is-wrong)) are
copied in as comments: they are about other packages' claims, not this one's.

## Forks

When the repository's history contains the frozen commit of a version of an
installed package that shares a thorn name with it (or of a package
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

Cactarium reads a package at its **release tags**. Commit the file, tag a
release, push both, then:

```sh
git -C repos/z4c add cactarium.toml
git -C repos/z4c commit -m "Add cactarium.toml"
git -C repos/z4c tag v1.0.0
git -C repos/z4c push
git -C repos/z4c push origin v1.0.0
cactup package init z4c --publish
```

`--publish` writes nothing. It looks for the newest release tag (named as
`tag-pattern` says, else `vX.Y.Z`) that the remote has, and refuses when
there is none, when that tag holds no `cactarium.toml`, or when the one on
disk differs from it, printing the git commands that fix it. A newer tag
not pushed yet is said, and the pushed one is used. The repository's URL
must be one anybody can fetch: no credentials in it, no local path, and it
must answer anonymously (an ssh remote is registered as its https form).
Then Cactarium registers the package under your own name, or under one of
your organizations with `--owner`, after a preview that guards the
registration. A package registered already is checked for new tags
instead, as `cactup refresh` does. New versions may wait for an admin's
review.

## A stack for the installation

```sh
cactup stack init                 # writes ./cactarium-stack.toml
cactup stack init stacks/mine     # in another directory
```

The installation must be on a stack release (`cactup install
owner/stack@version`, or `cactup inst refetch --release`); a branch tip
(`@head`) or a bare commit has no release to extend. Like `sim export`, it
refuses repositories with uncommitted changes or commits on no remote, and
thorns in a `!TYPE = ignore` section, which nobody else can fetch. A release
with such thorns can't be extended; Cactarium refuses the stack.
`--name` sets the stack's name (default: the installation's alias, lowercased)
and `--owner` its owner (default: your Cactarium user). The stack:

- **extends** the stack release in use by the installation;
- **pins** each base repository moved since that release;
- **overrides** a base repository with a package added with
  `cactup add --replace` when the package takes the place of every
  checkout of it, at its exact version. A package that replaces only some
  checkouts (or several repositories) is a section of
  `cactarium-stack.th` instead, which takes the place of those checkouts
  alone, as the installation does;
- adds every other added package as a pinned section of
  `cactarium-stack.th`, written beside it. A package added at a branch
  tip, at a commit no version froze, or with commits on top of its version
  is pinned at the commit installed, never at a version;
- lists, as `[[dependency]]` entries, the dependencies declared by the
  installed packages, each with a comment naming the package declaring
  it. These are information: they say what could be swapped in with a
  good chance of the stack still working. Installing a stack never
  resolves them.

Publish it as a release with

```sh
cactup stack init --publish --version 1.0.0
```

which writes nothing and uploads the `cactarium-stack.toml` found there
(and its thornlist) as `owner/name@1.0.0`, the owner being the first in its
`owners` unless `--owner` names another.
