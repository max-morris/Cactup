+++
title = "Sharing Simulations"
description = "Export a simulation as a .sim file, reproduce one with cactup import, and work with the Cactarium registry"
+++

# Sharing Simulations

A **`.sim` file** is a simulation you can hand to someone else: its parfile,
the thornlist it was built from, and the exact commit of every repository
behind that thornlist. Whoever has the file can rebuild the same code and
run the same simulation with one command. The **Cactarium** is a registry
where `.sim` files, thorn packages and stacks (published, versioned
thornlists) are shared.

## Exporting a simulation

```sh
cactup sim export mysim            # writes mysim.sim
cactup sim export mysim bbh.sim -y # take every default without asking
```

The pins are the commits the simulation's executable was **built from**, not
whatever your installation has moved on to since. On a terminal, export asks
for a name, summary, authors and license, each prefilled: the summary and
description come from the comment block at the top of your parfile, the
authors from your git `user.name`, and the license from the `default-license`
knob if you set one. Flags (`--name`, `--summary`, `--description`,
`--author`, `--license`) skip the questions.

Export refuses what nobody else could reproduce:

- **Uncommitted changes** in a repository when the executable was built.
  Commit them and rebuild, or pass `--allow-dirty` to export them marked
  dirty.
- **Commits that are on no remote branch or tag.** Push them and refetch, or
  pass `--allow-unpushed`.

A parfile value that is an absolute path (`/scratch/me/initial-data`) gets a
warning: whoever imports the simulation will not have that path. Use a knob
instead — `@KNOB(my-initial-data)@` — and export records it as an **input**
the importer must supply.

### Only the thorns the parfile needs

```sh
cactup sim export mysim --trim
```

`--trim` cuts the thornlist down to the thorns your parfile's `ActiveThorns`
name, plus everything their `.ccl` files say they need. The rest are not
deleted but turned into `#DISABLED Arrangement/Thorn` lines, so they are
neither fetched nor built, yet one edit brings any of them back.

## Packing loose files

If you have a thornlist and a parfile but no cactup simulation:

```sh
cactup sim pack bhns.th bhns.par                 # pinned to each branch's tip today
cactup -I myinstall sim pack bhns.th bhns.par    # pinned to what an installation has
```

## Unpacking a .sim file

```sh
cactup sim unpack bbh.sim --dir work/
```

writes the thornlist — with each pinned repository pinned by a
`!REPO_COMMIT` line — and the parfile, under their original names.

## Reproducing a .sim file

```sh
cactup import bbh.sim --input my-initial-data=/data/id.h5 --submit
```

`import` installs the file's sources at their pinned commits, builds them,
creates the simulation, and with `--submit` (or `--run`) starts it, using the
file's suggested tasks, CPUs per task and wall time unless you give `-T`,
`-c`, `-w` or `-q`. Without either flag it stops after creating the
simulation and prints the submit command.

Inputs are settled **before** anything long runs: from `--input KNOB=VALUE`,
else by downloading the file the input names (into
`~/.cactup/inputs/`, checked against its recorded hash), else by asking.
The values belong to the imported simulation only — your own knobs are never
changed — and `-K` on the command line still overrides them. If one of your
knobs has a different value, submit says which value it is using.

You can also just install the sources:

```sh
cactup install --simfile bbh.sim
```

Every repository is checked out at its pinned commit, and `cactup inst
refetch` will never move it.

## The Cactarium

cactup talks to `https://cactarium.cct.lsu.edu` unless the `cactarium-url`
knob or the `CACTARIUM_URL` environment variable names another.

### Signing in

Create a token on the site's settings page, then:

```sh
cactup login
```

The token is stored in `~/.cactup/credentials`, readable only by you.

### Publishing

```sh
cactup sim publish bbh.sim me/bbh
```

Only a **fully specified** file can be published: every repository pinned,
nothing dirty. The Cactarium numbers the versions (`me/bbh@1`, `@2`, …).

### Importing from the Cactarium

```sh
cactup import someone/bbh        # the newest version
cactup import someone/bbh@3      # exactly version 3
```

### Installing a stack

A stack is a published, versioned thornlist. Install one like a release:

```sh
cactup install et/et@2026.5.0
cactup install max/carpetx-mp@^1     # the newest 1.x release
```

Move an installation to a newer release with

```sh
cactup inst refetch --release max/carpetx-mp@^1
```

`@head` (the stack's branches as they are now) and `@master` (every
repository's default branch) install unpinned, for development; cactup says
so, because such an installation cannot be reproduced.

### Adding a package

```sh
cactup add max/carpetx@mixed-precision:^1      # a whole package, from a track
cactup add max/carpetx/TestReal4               # one thorn of it
```

The package is fetched at the commit the Cactarium recorded for that version
and added to the installation's live thornlist in a block marked as managed
by cactup; only its repository is fetched. If your thornlist already has that
repository (or one of its thorns), pass `--replace` to use the package in its
place. Packages you add survive moving the installation to a newer stack
release. Rebuild a config to compile them.

`cactup remove max/carpetx` takes a package out again, putting back any
repository it replaced at the commit the base names.
