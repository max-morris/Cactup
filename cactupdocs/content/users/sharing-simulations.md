+++
title = "Sharing Simulations"
description = "Export a simulation as a .sim file, reproduce one with cactup import, and work with the Cactarium registry"
+++

# Sharing Simulations

A **`.sim` file** is a simulation that you can hand to someone else: its
parfile, the thornlist that it was built from, the exact commit of every repository
behind that thornlist, and the SHA-256 of every file that it downloads. Whoever has the file can rebuild the same code and
run the same simulation with one command. **Cactarium** is a registry
where `.sim` files, thorn packages and stacks (published, versioned
thornlists) are shared.

## Exporting a simulation

```sh
cactup sim export mysim            # writes mysim.sim
cactup sim export mysim bbh.sim -y # take every default without asking
```

The pins are the commits that the simulation's executable was **built from**,
not whatever your installation has moved on to since. A file downloaded by the
thornlist (a tarball) is pinned by its SHA-256: the one given by the thornlist
in a `!CHECKSUM_SHA256` line, else the hash of the file that your installation
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
  Commit them and rebuild.
- **Commits that are on no remote branch or tag.** Push them, then run
  `cactup inst refetch`.
- **A repository that is no registered package's.** Every git repository
  must be one of a package registered with Cactarium: register it with
  `cactup package init <repo> --publish` (see
  [Publishing Packages](publishing-packages.html)) or on Cactarium's site, or
  fork a registered package.
- **A thorn that nobody else can fetch**: a `!TYPE = ignore` component, which
  exists only on this machine.
- **A download that Cactarium has not approved.** A download (a tarball
  fetched by the thornlist, or an input's file) passes once Cactarium has reviewed
  that URL with that SHA-256. Submit one for review with
  `cactup download submit <url>` (see below). Until an admin approves it,
  export says whether Cactarium is still verifying it or it awaits review;
  when Cactarium could not verify it (the file's hash differs, or its server
  failed), export says why, and submitting it again retries the check.
  A download on a loopback, private or link-local address
  (`http://127.0.0.1/…`, `http://10.0.0.5/…`) is refused outright: nobody
  outside your network could fetch it. A host that your machine cannot look up
  is left to Cactarium to check.
- **A download with no hash**, which nothing could review: the build
  recorded none (built before cactup recorded download hashes; rebuild the
  config), or, for `sim pack`, the thornlist gives none.
- **A repository that nothing pins**: a git repository with no commit (the
  build recorded none: rebuild the config; or, for `sim pack -I`, the
  installation never fetched it: run `cactup inst refetch`, or, when the
  installation's own thornlist lacks it, `cactup inst refetch <the list>`,
  which makes the installation follow that list), an svn, cvs,
  hg or darcs component (no commit pins it), or one section downloading
  several files (no single hash pins them). Move such a component to git,
  or to downloads each pinned by a `!CHECKSUM_SHA256` line. For `sim pack`
  without an installation, a `!REPO_COMMIT` that Cactarium says the
  package's repository lacks is refused too.
- **Repository URLs that nobody else can fetch, or that carry credentials.** A
  local path or `file://` URL, a URL with a user name, password or token in
  it (`https://TOKEN@host/…`), a query string (`?private_token=…`), or a
  `git://` URL (it can't be checked, and has no encryption) is refused, as
  Cactarium refuses it. Remove the credentials, or push the repository
  somewhere that others can reach and use its https URL.
- **Every git remote is checked.** Export asks each one over the network,
  as a stranger would, with no credentials of any kind: one small request
  per repository, all at once, within 30 seconds. A remote that asks for
  credentials, isn't there, or does not answer is refused, and so is one
  that says it is busy or failing (try again later); one that redirects
  to another path has moved, and export names where to. A remote
  whose host resolves to a private address (a machine on your own network)
  is refused too: nobody outside could reach it. (A host that your machine
  cannot look up itself, as on a login node that goes out through a proxy,
  is still asked.)
- **ssh URLs** (`git@github.com:me/thorns.git`) need an account on the
  host, so export asks the matching https URL
  (`https://github.com/me/thorns.git`) instead: if it answers, the `.sim`
  names the https URL, in the thornlist too, and export says so.

A URL written in quotes is refused too (cactup would take the quotes as
part of it). Everything refused, of every kind above, is listed in one
error, a line each, with what to do about it.

Export asks Cactarium once, at the same time as it asks the git remotes,
which package each pinned repository belongs to and whether each download
is approved; a URL already refused is never sent. The `.sim` then labels
each repository of a registered package with it: `owner/pkg@1.2.0` when
that version froze the commit, a bare `owner/pkg` for any other commit. An
input's file is asked about once it is downloaded and hashed, after the
questions. When Cactarium cannot be asked (it is down, or failing), that is
one more line of the error, beside everything else refused. A repository
or download refused before then (for its URL, or a private address) is
never sent to Cactarium, and is named once, for that. The claim overrides of the installation (`cactup claims`) are
recorded in the `.sim`'s provenance, for information only: `import` never
resolves dependencies.

A user name, password or token in a line that nothing fetches (a comment, an
unused `!DEFINE`, an overridden `!URL`) is not refused but hidden: the
`.sim` gets `***` in its place, and export prints each line that it changed.
This covers URLs, `user:password@host:path` remotes and cvs roots with a
password. A download's URL, and an input's
(`--input-url`), may carry a query (`?download=1` is how many download links
work), but not one that looks like a token or a signature, nor a user name
or password; an ssh or local one is refused as well.

A `.sim` meant only for this machine can skip all of that:

```sh
cactup sim export mysim --local   # nothing refused; no remote or Cactarium asked
```

Such a file keeps every URL as it is, marks sources with uncommitted
changes as dirty, and labels no repository. Export lists, a line each, what
keeps it from being published. `cactup sim publish` refuses such a file.

A parfile value that is an absolute path (`/scratch/me/initial-data`) gets a
warning: whoever imports the simulation will not have that path. Use a knob
instead — `@KNOB(my-initial-data)@` — and export records it as an **input**
that the importer must supply.

If the input can be downloaded, say where, and `import` will fetch it:

```sh
cactup sim export mysim --input-url kadath-initial-data=https://einsteintoolkit.org/gallery/bhns/bhns_gw230529_ID.tar.gz
```

cactup downloads the file once and records its SHA-256. For an archive, it
also works out which file inside it your knob points at (from the knob's
value here; `--input-path KNOB=FILE` names it otherwise), and compares the
archive's text files with your unpacked copy: a line that you changed only to
name where you unpacked it, such as a path to an equation-of-state file, is
recorded as an edit made by `import` after unpacking, with the importer's own
folder in its place. Any other difference is reported and left out.

### Only the thorns needed by the parfile

```sh
cactup sim export mysim --trim
```

`--trim` cuts the thornlist down to the thorns named by your parfile's
`ActiveThorns`, plus everything that their `.ccl` files say they need and every
thorn whose headers they include. The rest are not
deleted but turned into `#DISABLED Arrangement/Thorn` lines, so they are
neither fetched nor built, yet one edit brings any of them back.

Trimmed or not, `export` (and `pack -I`) refuses a thornlist that builds a
thorn without what it needs, the same check that `cactup build` makes before it
builds: a `.sim` like that would fail every import's build.

## Packing loose files

If you have a thornlist and a parfile but no cactup simulation:

```sh
cactup sim pack bhns.th bhns.par                 # pinned to each branch's tip today
cactup -I myinstall sim pack bhns.th bhns.par    # pinned to what an installation has
```

`pack` checks the URLs, the repositories and the downloads exactly as
`export` does, and takes `--local` too. Without an installation, an unpinned
repository's branch tip is asked only after its URL passes; a URL refused
then fails on its own, before the rest is checked.

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
`import` also takes every option that `sim submit` and `sim run` take (`-q`, `-n`, `-T`, `-c`, `-w`,
`--universe`, `--force-queue`, `--checkpt-buffer` and the rest) and passes
them on; the file's suggested tasks, CPUs and GPUs per task and wall time fill
in whatever you don't give. One difference: `-a` is `--alias` here, so give
the account to charge as `--allocation`. The file suggests no node count: a
submitted job gets as many nodes as its tasks need on your machine, unless
you give `-n`; `--run` from a login node runs on that one machine. Without
either flag it stops after creating the simulation and prints the submit
command.

On a machine that builds on the queue, `import` submits the build and follows
it: it names the job, shows its state and how long it has waited, and streams
the build's output once it starts. Ctrl-C stops the import but not the build;
run the same command again once the build is over to finish the import.

Inputs are settled **before** anything long runs: from `--input KNOB=VALUE`,
else by downloading the file named by the input (into
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
thorn that it builds, from the same repositories, each pinned repository at its
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

## Cactarium

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

To publish a package or a stack instead of a sim, see
[Publishing Packages](publishing-packages.html).

```sh
cactup sim publish bbh.sim me/bbh
```

Only a **fully specified** file can be published: every repository pinned,
nothing dirty. Before anything is sent, publish checks the file as export
checks what it writes: URLs that nobody else can fetch or that carry
credentials, `ignore` components, repositories that nothing pins, downloads on a
private address. Then it checks that you have a token, then asks Cactarium
about every pin: a repository that is no registered package's, or a
download (an input's file included) that Cactarium has not approved, is
refused. Every problem is a line of one error, and nothing is uploaded. Versions are semantic versions: `--version 1.2.0` chooses one,
and without it the next major version is used (`me/bbh@1.0.0`, `@2.0.0`, …).
A new version may wait for an admin's review before anyone can import it;
cactup says so when it does.

### Submitting a download for review

A `.sim` for others may pin a download (a tarball fetched by its thornlist,
an input's file) only once Cactarium has approved it:

```sh
cactup download submit https://zenodo.org/records/1/files/id.tar.gz --note "initial data of the paper"
```

cactup downloads the file once, computes its SHA-256 (nothing is kept),
and submits the URL and the hash with your token (`cactup login`).
Cactarium then fetches the file itself to verify the hash, and an admin
reviews it. cactup then says where it stands: being verified, verified and
awaiting review, approved, rejected, or not verified and why. A download
submitted before is not an error: cactup says the same of it, and
submitting one that could not be verified makes Cactarium check it again
(cactup says why the last check failed).
A URL with credentials, or on a private address, is refused before
anything is downloaded.

### Importing from Cactarium

```sh
cactup import someone/bbh          # the newest version
cactup import someone/bbh@1        # the newest 1.x.x
cactup import someone/bbh@=1.0.1   # exactly version 1.0.1
```

Some entries hold **several sims**, released together from one repository
(for example a code's example simulations). Each of those is always named,
as `owner/name/sim`:

```sh
cactup import et/cottonmouth-sims/qc0-z4c              # the newest version that has qc0-z4c
cactup import et/cottonmouth-sims/qc0-z4c@~2026.5      # the newest 2026.5.x that has it
cactup import et/cottonmouth-sims/mag-tov@=2026.5.1    # exactly version 2026.5.1
```

Naming such an entry without a sim (`cactup import et/cottonmouth-sims`) is
refused, and the message lists its sims. A version range picks the newest
version in it that still has the sim that you named; if a newer version in the
range dropped that sim, cactup says so and imports the newest one that has
it. An entry that is one sim is never named with a third part.

A downloaded sim is kept in `~/.cactup/simfiles/`, so importing the same
exact version again still works when Cactarium cannot be reached. When
it can, the sim is downloaded again, and if Cactarium's copy has
changed since (a version re-read after its tag was moved), cactup says so
and imports the new one. Naming one sim of several needs a server that knows
such entries; an older one is refused with a message saying so.

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

Without a version, the installation stays on the track that its release came from
(a `mixed-precision:1.2.0` installation moves to the newest
`mixed-precision` release, never to the default track).

`@head` (the stack's branches as they are now) and `@master` (every
repository's default branch) install unpinned, for development; cactup says
so, because such an installation cannot be reproduced.

Nobody reviews those branch tips. For a stack not trusted by Cactarium,
Cactarium may refuse to serve them at all; if it does serve one, cactup
warns that it may fetch and build arbitrary code and asks you to type `yes`.
`--risky-allow-untrusted-artifacts` skips the question (the warning still
prints), and is the only way to take such a tip in a script.

The same flag covers versions that nobody has approved yet. Without it,
`add`, `remove`, `claims`, `import`, `install` and `inst refetch` ask
Cactarium anonymously: no token, and approved versions only. With it, each
request of those commands carries `x-cactarium-include-unreviewed: true` and
your token (`CACTARIUM_TOKEN`, else the one stored by `cactup login`), if
there is one, to the Cactarium server named by the `cactarium-url` knob,
whatever server that is. The other commands never send the header, so a
shared `.sim` rests on approved versions only. Those that need your token
(`sim publish`, `download submit`, `package init`, `stack init`, `refresh`,
`login`) send it to authenticate. Cactarium then
serves versions awaiting review as its settings allow: to an admin's token
always, to anyone only when the site serves unreviewed code. An answer that
includes one is warned about, and `cactup add` lists each such release in red
(`et/foo@1.2.0 awaits review`). Ranges prefer approved versions: a version
awaiting review is taken only when named exactly (`@=1.2.0`), or when no
approved version fits. An installed package is never moved to an older
version for being awaiting review; a move to an older version is shown as a
`downgrade`. A package added from a release awaiting review is marked so in
`installation.toml`. Without the flag, Cactarium hides that release, so later
commands can't read the package. They say why, and `cactup inst show` lists it
as "awaited review when added".

A sim awaiting review downloads to
`~/.cactup/simfiles/<owner>/<name>/[<sim>/]<version>.unreviewed.sim`
(`<sim>/` only for one sim of a multi-sim entry).
When Cactarium cannot be asked, `cactup import` uses the cached copy of an
approved version and never one of these. An unreviewed copy is kept while an
installation made from it exists; once none does, a later sim download deletes
it (after a week, so an import in progress keeps its copy).

A stack release is installed as it was frozen: nothing is resolved. After
`install` and `inst refetch --release` to a stack, once their own output is
done, cactup runs `cactup claims check` over the result and prints what it
finds as warnings, each naming a way past (`cactup add --upgrade <pkg>`, or
a `cactup claims override …`). They never fail the command. The check is
quick by design: it gives up after about 20 seconds, saying Cactarium did
not answer in time, and does not read base packages at commits not frozen
by any version (it says how many it left unchecked). Ctrl-C ends the check alone:
the install or refetch that it followed stays a success. Run
`cactup claims check` for the full check.

### Adding a package

```sh
cactup add max/carpetx@mixed-precision:^1                # a whole package, from a track
cactup add max/carpetx/TestReal4                         # one thorn of it
cactup add max/carpetx/TestReal4 max/carpetx/Loop        # several thorns
cactup add max/carpetx/Loop max/spacetimex               # several packages at once
cactup add max/carpetx#3f2c9e1                          # at one commit
```

The package is fetched at the commit recorded by Cactarium for that version
and added to the installation's live thornlist in a block marked as managed
by cactup; only its repository is fetched. If your thornlist already has that
repository (or one of its thorns), pass `--replace` to use the package in its
place. Packages that you add survive moving the installation to a newer stack
release. Rebuild a config to compile them.

`#` and a commit id (at least its first 7 hex digits) adds the package as it
was at that commit; if the prefix matches more than one commit,
Cactarium lists them and you give more digits. If a published version was
cut there, that version is what you get. Otherwise nobody has reviewed that commit, so
the same rule as for a branch tip applies to a package not trusted by
Cactarium: it may refuse, or cactup asks you to type `yes`. `cactup inst show`
lists such a package as `owner/name at commit 3f2c9e1a0011`. The package's
repository must let a commit be fetched by its id, as GitHub, GitLab and other
current git hosts do.

Some packages build against a **library**: a package with no thorns, such as
FUKA, which KadathThorn builds from source. The package says where the
library goes in its own repository, and `cactup add` brings it along at the
version that the package asks for: `cactup add et/kadaththorn` puts FUKA at
`repos/KadathThorn/src/fuka`, where KadathThorn's build looks for it. The
library is cloned next to the other repositories (`repos/fuka`) and linked in
there; cactup never replaces anything already at that path, and says so
instead. `cactup inst show` lists each library under its package. A library
is not added on its own; add the package that uses it.

`cactup remove max/carpetx` takes a package out again (see
[Removing a package](#removing-a-package)).

If a version that you install, add or import has been **yanked** on
Cactarium (still available, but no longer chosen by default), cactup says so
with the reason and goes on; `cactup inst show` also says when the stack or a
package used by an installation was yanked later.

`cactup inst show` lists the packages that you added, each with where to report
problems with it (the package's contact, else its repository), and for an
installation of a stack, the same for the stack. If a build
fails in a thorn that you added, cactup says which package it came from and the
same address.

### Dependencies

A package declares what it needs from other packages: a **dependency** names
the package, the versions that it accepts and the thorns of it that are needed.
`cactup add` reads these claims and brings in whatever is missing, so one
command gives a working set:

```sh
cactup add max/z4c
```

```
Plan for cactup add max/z4c:
  add  max/z4c@1.2.0        Z4c, Z4cTest   named
  add  et/carpetx@2026.5.1  CarpetX, Loop  needed by max/z4c@1.2.0 (Z4c), ^2026.5
Proceed? [Y/n]
```

Each step is one of `add`, `re-add`, `upgrade`, `downgrade`, `replace` or
`keep`, with the reason at the end of its line. A package brought in for a
dependency gets only the thorns needed by that dependency, and only when the
thorns of the dependent that need it are enabled. Each is an ordinary added
package afterward: `inst refetch` keeps it, and `remove` takes it out.

cactup asks only when the plan goes beyond the packages named. Without a
terminal it refuses, naming `--yes` and `--dry-run`. `--dry-run` shows the plan
and changes nothing; `--yes` skips the question.

What the plan decides:

- **Installed packages stay put.** A dependency that an installed package
  does not meet is refused, naming the way past. `--upgrade` moves the
  installed packages that the plan touches to the versions that it needs;
  `--upgrade-all` moves every installed package to its newest version that
  fits. An added package that you name again is moved to its newest version
  that fits. A named base package stays put unless `--upgrade`,
  `--upgrade-all` or `--replace`.
- **The base counts as packages.** The repositories of the installation's
  base thornlist are matched to registered packages by URL and commit, in
  one request to Cactarium. A repository matched to none is treated as
  `local:<name>`: its thorns still count in clashes.
- **Any-of.** A dependency may list alternatives (Carpet or PUGH). cactup
  takes one already installed, else one already in the plan, else the first
  listed that conflicts with nothing. The plan names the rule
  (`max/z4c@1.2.0: any of et/carpet, et/cactuspugh (the first listed)`), and
  `cactup claims list` shows the choice later.
- **Clashes are refused.** Two enabled thorns of one name from different
  packages, a package that says it is incompatible with another, and a fork
  beside its upstream are refused, each with its ways past: `--replace`,
  `--ignore-claim`, or a `cactup claims override` (see below). A **fork**
  (a package that declares `fork-of`) takes the place of its upstream for
  every dependency on it that the declared range covers.
- **Requirements that nobody declared are refused.** After the plan, every CCL
  requirement of a thorn that it enables must be met by the final set. cactup
  never adds a package that nobody declared as a dependency; it lists the
  registered providers as `cactup add` lines, or says that none provides it.
- **Libraries** are placed as above, also under `--no-deps`.
- **Awaiting review.** A version that nobody has approved is taken only when named
  exactly or when no approved version fits (see the flag above). The plan
  lists each in red.

`--no-deps` adds only the packages named, with no dependencies (libraries are
still placed). The dependency claims of installed packages stop binding, and
the plan notes each one left unmet. Thorn clashes, incompatibilities, a fork
beside its upstream and the CCL check still refuse. `--ignore-claim
OWNER/PKG[:KIND:TARGET]` ignores a package's claims for one command; KIND is
`dependency`, `incompatible-with` or `fork-of`.

A **flesh package** is a package that replaces the base's Cactus flesh (the
`Makefile`, `lib` and `src` at the root of the tree). It takes the place of
the base's flesh, or of another added flesh package, and `remove` puts the
base's flesh back. When the plan is shown it says `The flesh changes; every
config needs a rebuild`, and the command ends with `Rebuild every config for
the new flesh, e.g. cactup build <config>`. Each config then compiles from
scratch.

A base package at a commit that Cactarium has not read (unpushed work in a
base clone, say) has dependencies that cactup cannot know. It says so in a note and
goes on. An added package that Cactarium cannot give for its installed version
hides what it depends on: `remove` refuses beside one, and `add` refuses to
move packages beside one. `--no-deps` gets past both.

A server from before dependencies cannot answer `add`'s requests, and the error
names `--no-deps` as the way past.

### Removing a package

```sh
cactup remove max/z4c
```

```
Plan for cactup remove max/z4c:
  remove  max/z4c@1.2.0          named
  remove  et/carpetx@2026.5.1    brought in by max/z4c@1.2.0, no longer needed
```

`remove` takes out the packages named, and the packages that only they
needed: those brought in as dependencies, never named by you, that no
remaining package depends on, down the chain. A package that you named at any
time is never removed this way, nor is one added by a cactup that did not
record why. Any base repository that a removed package replaced is restored at
the commit that the base names, and what a returning base package needs counts
too. When Cactarium cannot read a returning base repo, `remove` prints
`Not checked (unreadable): what these returning base repos need:` and a list,
and each package that it would sweep says `unless a returning base repo needs
it` in the plan, so the confirmation covers that risk.

`remove` is refused while another package, added or in the base, depends on one
named, and says which. `--no-deps` removes exactly the packages named, with no
dependents check and no sweep, and needs no Cactarium; without it, `remove`
asks Cactarium for each package's claims and refuses when it cannot be reached.
Like `add`, it asks before removing more than was named, refuses without a
terminal unless `--yes` is given, and shows the plan with `--dry-run`.

### When a claim is wrong

Packages declare **claims** about each other: a dependency, an
incompatibility, a fork. When a claim is wrong for your installation, `add`
refuses and names the way past. `--ignore-claim OWNER/PKG[:KIND:TARGET]`
ignores claims for one command. `cactup claims override` keeps the change in
the installation, for every later `add` and `remove`:

```sh
cactup claims override drop max/foo:incompatible-with:et/bar      # forget a claim
cactup claims override needs max/foo:dependency:et/bar Loop       # more thorns it needs
cactup claims override add max/foo --dependency et/bar --needs Loop   # a dependency it lacks
cactup claims override satisfies max/carpet-mp et/carpet          # one package stands in for another
cactup claims override satisfies local:carpetx et/carpetx         # so does a repo of the base list
cactup claims override --remove 'drop max/foo:incompatible-with:et/bar'
```

`drop` and `needs` name a claim that Cactarium serves for the package in some
version. `satisfies X Y` lets X take every need for Y without a version check.
Name a repo of the base list that no registered package stands for as
`local:NAME`.

`cactup claims list` shows the overrides, and the any-of choices made when
packages were added; it fails, after printing, when an override does not
read. The overrides go into a `.sim` exported from the
installation, as information only. `cactup inst show` lists the overrides too.

`cactup claims check` looks at the installed packages as they are: forks
installed beside their upstream, incompatible packages, a thorn name enabled
twice, CCL requirements that no installed thorn meets, dependencies that
nothing installed meets (a release outside the range, thorns not enabled),
and `satisfies` overrides naming a repo that is not installed. It prints one
line for each problem, with the way past, and changes nothing. What it could
not check goes on "Not checked" lines: base packages at commits that no
version froze, added packages that Cactarium cannot give, and a base with no
fetch record. With no problem it says "No problems.", or "No problems in what
was checked." when something was left unchecked. It exits with an error ("2 problems.") when it finds
some.

An installation that has had an override, or has had a flesh package, is refused
by a cactup that predates them: `cactup update` fixes that.

### Getting a new release noticed

Cactarium checks each package's repository for new release tags every few
hours. Right after tagging one, have it look now:

```sh
cactup refresh max/carpetx
```

It needs `cactup login`, as an owner of the package. It also says how many
versions are waiting for review, which nobody can install until an admin
approves them.
