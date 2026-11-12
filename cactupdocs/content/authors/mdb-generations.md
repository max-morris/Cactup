+++
title = "MDB Generations"
description = "How the published machine database stays readable by every deployed cactup, and when to bump its generation"
+++

# MDB Generations

The machine database (MDB) is published separately from the cactup binary,
and users update the two at different times. A **generation** is the number
that keeps them compatible. This page is for anyone who changes files under
`mdb/` in the cactup repository, and for users whose own overlay machines
need to catch up after a generation change.

## Why generations exist

cactup reads `meta.toml` with a **closed schema**: a key it does not know is
an error, not something to skip. That strictness is what catches typos, but
it also means an MDB that starts using a new key, even an optional one,
cannot be read by a cactup built before that key existed. Other changes
break silently rather than loudly: a template variable that an older cactup
does not define, or a default whose meaning changed.

So every cactup binary is pinned to one MDB generation, and the published
MDB only changes in ways that every binary of that generation understands.
A change that would break those binaries starts a new generation instead.

## Where the generation lives

- **`mdb/GENERATION`** holds the current generation, a single integer.
  cactup reads it at build time, and `cactup --version` shows it
  (`… mdb generation 3`).
- **`mdb/GENERATIONS.md`** is the changelog: one `## Generation N` entry per
  generation, saying what changed and how to bring an overlay machine
  forward. It is published with the MDB, so every user has a copy at
  `~/.cactup/mdb/gen-<N>/GENERATIONS.md`.
- **The `mdb` branch.** On every push to `master` that passes CI, the
  contents of `mdb/` are published as the repository's `mdb` branch. The
  branch holds every generation's history, one after another.
- **cactup picks the newest commit of its generation.** It fetches the `mdb`
  branch and walks back from the tip to the newest commit whose `GENERATION`
  matches its own. When the tip is newer than that, it keeps using the
  last matching commit and warns that the machine database has moved to a
  newer generation (see [Updating cactup](../users/updating.html#machine-database-generations)).

CI publishes the MDB before it publishes the binaries, so a freshly released
cactup of a new generation always finds a commit of its generation.

## The bump rule

Bump the generation when **either** of these is true:

1. **Old binaries, new MDB.** `mdb/` as of your commit would fail to load,
   or would behave differently, in the oldest cactup of the current
   generation (the one built from the commit that last changed
   `mdb/GENERATION`).
2. **New binary, old overlays.** A user's overlay machine written for the
   current generation would fail to load, or would behave differently, in
   the cactup built from your commit.

The first direction guards the published MDB against binaries already
deployed. The second guards users' own machines against a new binary. A
change can break one direction and not the other; either is enough.

### What counts as breaking

| Area | Breaking changes |
|---|---|
| `meta.toml` schema | Any key or table the MDB starts using that the generation's first binary lacks, **even an optional one**; renaming, removing or retyping a key; a new required key; a changed default or meaning; stricter validation |
| Scripts and flavors | The shape of a route in `[submitscripts]`, `[runscripts]` or `[buildsubmitscripts]`; the script-kind directories (`submitscripts/`, `runscripts/`, `buildsubmitscripts/`); how `.sh` and `.py` scripts are found; how flavors are discovered in `optionlists/` |
| Templates | A new, renamed, removed or re-meant `@VAR@`; the `@ENV(…)@` and `@KNOB(…)@` forms (`-OPTIONAL`, `-OR-DROP-LINE`, and any new one); `@@`; how comments pass through; where the environment setup is placed |
| `.py` scripts | The preamble and the calling protocol |
| Re-invocations | The flags generated scripts pass back to cactup: `sim run --sim-dir/--restart-id`, `test run --test-dir/--results-id`, `build run --config-dir/--attempt-id` |
| Discovery | What `hostname.regexp` and `discover.py` mean |
| Optionlists | Keys in the `[cactup]` header or its `[cactup.runs-on]` table (both are closed, like `meta.toml`); the render and `VERSION` rules |
| Universes | `wrapper-argv`, `wrapper` and `@COMMAND@` |

**Not breaking:** adding a machine, or editing values in an existing one,
using only constructs the generation already has; comments; and cactup
gaining support for something the MDB does not use yet.

## Bumping the generation

Do all of this in **one commit**:

0. Before touching anything, on the tree as it is (the last one at the old
   generation), snapshot that generation's overlays:
   `ci/overlay-fixtures.sh N OUTDIR` builds the tree, has it create an overlay
   from each of a fixed list of shipped machines, scrubs what depends on your
   host, and copies them. Commit the result as
   `fixtures/mdb-overlays/gen-N/`. These are real output of the generation's
   own binary, which is what users have on disk; never edit a committed
   snapshot.
1. Increment the number in `mdb/GENERATION`.
2. Append a `## Generation N+1` entry to `mdb/GENERATIONS.md`: what changed;
   an "Automatically:" paragraph saying what the overlay migration does for
   this step and when it stops; and the recipe for bringing an overlay
   machine forward by hand (which keys to rename, add or remove, which
   variables changed), including what the migration refuses and what is
   optional. Users follow it step by step.
3. Add the overlay migration step for N to N+1 to `STEPS` in
   `src/mdb/migrate.rs`. A step that only raises the number is still a step
   (`raise`). A step preserves behavior or refuses: it does the mechanical
   edits, notes a change that can only show up later as a loud error, and
   stops where the machine could quietly do something different. It never
   applies an optional change, never edits a script, and never writes a key
   the new generation's first binary lacks.
4. Migrate every machine under `mdb/` in the repository to the new
   generation.
5. Make the cactup source understand it: a binary built from this commit
   embeds the new number.

Checks back this up. Unit tests fail the build when `mdb/GENERATIONS.md` has
no entry for every generation up to the number in `mdb/GENERATION`, when
`STEPS` lacks a step for any older generation, when `fixtures/mdb-overlays/`
lacks a snapshot for one, and when any snapshot (or hand-written edge case
beside them) fails to migrate to the current generation, load, and leave
itself untouched on the second load. The `mdb-compat` CI job loads every
machine in both directions of the rule (the MDB at your commit with the
generation's first binary, and the generation's first MDB with your binary)
and tells you to bump if either fails. It checks loading only: a changed
template variable or a changed `.py` protocol still needs your judgment.

After a bump, users on older builds see the "moved to generation" warning
until they update; an older build keeps working with the last MDB of its
generation.

## Overlay machines

A machine in a user's overlay (`~/.cactup/machines/<name>/`) records the
generation it was written for in its `meta.toml`:

```toml
[cactup]
mdb-generation = 5
```

`cactup machine create` writes the current generation for you. A machine
without the key loads with a warning that cactup is assuming its own
generation; add the key to silence it. A machine written for an older
generation is brought forward by a newer cactup, as described next. One
written for a newer generation asks for `cactup update`.

### Updating an overlay machine

cactup updates an overlay machine itself the first time it loads it after an
upgrade, and prints one line saying so and where the old files went: a full
copy in `~/.cactup/machine-backups/<name>/generation-<N>/`, kept for good,
with a `NOTES.txt` of what was done. The edits are mechanical and keep your
comments; scripts are never touched, and the optional changes in a
generation's entry are never made for you. Where the update could change what
the machine does without saying so, it stops instead and keeps whatever
earlier steps it finished. The message names the machine and file, what
stopped it, and the next step on a line of its own: the entry of
`GENERATIONS.md` to follow by hand. Make that change, set `mdb-generation` to
the generation the message names, and run any cactup command again; cactup
carries on from there. Notes the update prints (such as a setup that may now
belong in a provisioning key) are for you to read; nothing breaks if you do
not.

An overlay written for generation 1 is not updated automatically: cactup
refuses it untouched, and the recipe under Generation 2 is followed by hand.
A machine whose `meta.toml` has no `mdb-generation` is assumed current and
never rewritten (the warning says so, and names the generation its keys look
like when they give it away).

If the message offers to start over, it gives a command that moves the
machine's directory aside to a hidden `.<name>.old` beside it, which cactup
never lists or loads, and creates the machine again from the same base; your
own edits are then in the moved directory. Another cactup sharing the same
`~/.cactup` that is still on the older generation will ask you to run
`cactup update`. To go back after an update, copy the backup over the
machine directory.

