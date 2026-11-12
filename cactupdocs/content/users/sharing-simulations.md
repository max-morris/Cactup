+++
title = "Sharing Simulations"
description = "Export a simulation as a .sim file, reproduce one with cactup import, and work with the Cactarium registry"
+++

# Sharing Simulations

A **`.sim` file** is a simulation you can hand to someone else: its parfile,
the thornlist it was built from, the exact commit of every repository
behind that thornlist, and the SHA-256 of every file it downloads. Whoever has the file can rebuild the same code and
run the same simulation with one command. The **Cactarium** is a registry
where `.sim` files, thorn packages and stacks (published, versioned
thornlists) are shared.

## Exporting a simulation

```sh
cactup sim export mysim            # writes mysim.sim
cactup sim export mysim bbh.sim -y # take every default without asking
```

The pins are the commits the simulation's executable was **built from**, not
whatever your installation has moved on to since. A file the thornlist
downloads (a tarball) is pinned by its SHA-256: the one the thornlist gives
in a `!CHECKSUM_SHA256` line, else the hash of the file your installation
has, as long as it has not been downloaded again since the build. On a terminal, export asks
for a name, summary, authors and license, each prefilled: the summary and
description come from the comment block at the top of your parfile, the
authors from your git `user.name`, the license from the `default-license`
knob and the contact (where to go for help: an `https://` URL or a `mailto:`
address) from the `default-contact` knob, if you set them. Flags (`--name`,
`--summary`, `--description`, `--author`, `--license`, `--contact`) skip the
questions.

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

If the input can be downloaded, say where, and `import` will fetch it:

```sh
cactup sim export mysim --input-url kadath-initial-data=https://einsteintoolkit.org/gallery/bhns/bhns_gw230529_ID.tar.gz
```

cactup downloads the file once and records its SHA-256. For an archive, it
also works out which file inside it your knob points at (from the knob's
value here; `--input-path KNOB=FILE` names it otherwise), and compares the
archive's text files with your unpacked copy: a line you changed only to name
where you unpacked it, such as a path to an equation-of-state file, is
recorded as an edit `import` makes after unpacking, with the importer's own
folder in its place. Any other difference is reported and left out.

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
`!REPO_COMMIT` line and each pinned download by a `!CHECKSUM_SHA256` line —
and the parfile, under their original names. GetComponents and Cactus skip
both kinds of line, so the thornlist still works without cactup; it just
isn't pinned there.

## Reproducing a .sim file

```sh
cactup import bbh.sim --input my-initial-data=/data/id.h5 --submit
```

`import` installs the file's sources at their pinned commits, builds them,
creates the simulation, and with `--submit` (or `--run`) starts it. `-j N`
(or `-j max`) sets the build's parallel make jobs, as for `cactup build`.
`import` also takes every option `sim submit` and `sim run` take (`-q`, `-n`, `-T`, `-c`, `-w`,
`--universe`, `--force-queue`, `--checkpt-buffer` and the rest) and passes
them on; the file's suggested tasks, CPUs and GPUs per task and wall time fill
in whatever you don't give. One difference: `-a` is `--alias` here, so give
the account to charge as `--allocation`. The file suggests no node count: a
submitted job gets as many nodes as its tasks need on your machine, unless
you give `-n`; `--run` from a login node runs on that one machine. Without
either flag it stops after creating the simulation and prints the submit
command.

Inputs are settled **before** anything long runs: from `--input KNOB=VALUE`,
else by downloading the file the input names (into
`~/.cactup/inputs/`, checked against its recorded hash), else by asking.

An input can also be an **archive** (`.tar`, `.tar.gz`, `.tar.xz` or `.zip`)
that `import` unpacks after checking its SHA-256, pointing the knob at one
file inside it. The `.sim` can also have small text edits made to the
unpacked files, such as replacing a placeholder path with the real one. A
download over 1 GB is asked about first (`-y` says yes). Unpacked archives
are kept and reused by every simulation that needs the same one.

Downloads can be large, so on a cluster they belong somewhere other than
your home directory. The machine's `inputs-home` says where; the knob of
the same name overrides it for you:

```sh
cactup knob inputs-home /work/$USER/cactup-inputs
cactup cache inputs              # what is there, and how big
cactup cache inputs --clear      # remove it all
```
The values belong to the imported simulation only — your own knobs are never
changed — and `-K` on the command line still overrides them. If one of your
knobs has a different value, submit says which value it is using.

If one of your installations already has exactly the file's code — every
thorn it builds, from the same repositories, each pinned repository at its
commit with no uncommitted changes — `import` uses it instead of installing a
second copy, and goes straight to the build. Extra thorns in the
installation do not matter. Name the installation with `-I` to insist on it
(the import then stops if it differs, saying how), or pass
`--new-installation` to install a fresh copy anyway.

You can also just install the sources:

```sh
cactup install --simfile bbh.sim
```

Every repository is checked out at its pinned commit, and `cactup inst
refetch` will never move it. Every pinned download is checked against its
hash: a file that has changed on the server since is refused, and the old
copy, if any, stays.

## The Cactarium

cactup talks to `https://cactarium.cct.lsu.edu` unless the `cactarium-url`
knob or the `CACTARIUM_URL` environment variable names another
(`cactup knob cactarium-url https://registry.example.org`).

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
nothing dirty. Versions are semantic versions: `--version 1.2.0` chooses one,
and without it the next major version is used (`me/bbh@1.0.0`, `@2.0.0`, …).
A new version may wait for an admin's review before anyone can import it;
cactup says so when it does.

### Importing from the Cactarium

```sh
cactup import someone/bbh          # the newest version
cactup import someone/bbh@1        # the newest 1.x.x
cactup import someone/bbh@=1.0.1   # exactly version 1.0.1
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
cactup inst refetch --release max/carpetx-mp     # the newest on the track it is on
```

Without a version, the installation stays on the track its release came from
(a `mixed-precision:1.2.0` installation moves to the newest
`mixed-precision` release, never to the default track).

`@head` (the stack's branches as they are now) and `@master` (every
repository's default branch) install unpinned, for development; cactup says
so, because such an installation cannot be reproduced.

Nobody reviews those branch tips. For a stack the Cactarium does not trust,
the Cactarium may refuse to serve them at all; if it does serve one, cactup
warns that it may fetch and build arbitrary code and asks you to type `yes`.
`--risky-allow-untrusted-artifacts` skips the question (the warning still
prints), and is the only way to take such a tip in a script.

### Adding a package

```sh
cactup add max/carpetx@mixed-precision:^1                # a whole package, from a track
cactup add max/carpetx/TestReal4                         # one thorn of it
cactup add max/carpetx/TestReal4 max/carpetx/Loop        # several thorns
cactup add max/carpetx/Loop max/spacetimex               # several packages at once
cactup add max/carpetx#3f2c9e1                          # at one commit
```

The package is fetched at the commit the Cactarium recorded for that version
and added to the installation's live thornlist in a block marked as managed
by cactup; only its repository is fetched. If your thornlist already has that
repository (or one of its thorns), pass `--replace` to use the package in its
place. Packages you add survive moving the installation to a newer stack
release. Rebuild a config to compile them.

`#` and a commit id (at least its first 7 hex digits) adds the package as it
was at that commit; if the prefix matches more than one commit, the
Cactarium lists them and you give more digits. If a published version was
cut there, that version is what you get. Otherwise nobody has reviewed that commit, so
the same rule as for a branch tip applies to a package the Cactarium does not
trust: it may refuse, or cactup asks you to type `yes`. `cactup inst show`
lists such a package as `owner/name at commit 3f2c9e1a0011`. The package's
repository must let a commit be fetched by its id, as GitHub, GitLab and other
current git hosts do.

Some packages build against a **library**: a package with no thorns, such as
FUKA, which KadathThorn builds from source. The package says where the
library goes in its own repository, and `cactup add` brings it along at the
version the package asks for: `cactup add et/kadaththorn` puts FUKA at
`repos/KadathThorn/src/fuka`, where KadathThorn's build looks for it. The
library is cloned next to the other repositories (`repos/fuka`) and linked in
there; cactup never replaces anything already at that path, and says so
instead. `cactup inst show` lists each library under its package. A library
is not added on its own; add the package that uses it.

`cactup remove max/carpetx` takes a package out again, putting back any
repository it replaced at the commit the base names.

If a version you install, add or import has been **yanked** on the
Cactarium (still available, but no longer chosen by default), cactup says so
with the reason and goes on; `cactup inst show` also says when the stack or a
package an installation uses was yanked later.

`cactup inst show` lists the packages you added, each with where to report
problems with it (the package's contact, else its repository), and for an
installation of a stack, the same for the stack. If a build
fails in a thorn you added, cactup says which package it came from and the
same address.

### Getting a new release noticed

The Cactarium checks each package's repository for new release tags every few
hours. Right after tagging one, have it look now:

```sh
cactup refresh max/carpetx
```

It needs `cactup login`, as an owner of the package. It also says how many
versions are waiting for review, which nobody can install until an admin
approves them.
