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
  (`… mdb generation 1`).
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
| Variants | The shape of a `[variants.*]` entry; the script-kind directories (`submitscripts/`, `runscripts/`, `buildsubmitscripts/`); how `.sh` and `.py` variants are found |
| Templates | A new, renamed, removed or re-meant `@VAR@`; the `@ENV(…)@` and `@KNOB(…)@` forms; `@@`; how comments pass through; where the environment setup is placed |
| `.py` scripts | The preamble and the calling protocol |
| Re-invocations | The flags generated scripts pass back to cactup: `sim run --sim-dir/--restart-id`, `test run --test-dir/--results-id`, `build run --config-dir/--attempt-id` |
| Discovery | What `hostname.regexp` and `discover.py` mean |
| Optionlists | Keys in the `[cactup]` header (an older binary silently **ignores** an unknown one, so this needs judgment); the render and `VERSION` rules |
| Universes | `wrapper-argv`, `wrapper` and `@COMMAND@` |

**Not breaking:** adding a machine, or editing values in an existing one,
using only constructs the generation already has; comments; and cactup
gaining support for something the MDB does not use yet.

## Bumping the generation

Do all of this in **one commit**:

1. Increment the number in `mdb/GENERATION`.
2. Append a `## Generation N+1` entry to `mdb/GENERATIONS.md`: what changed,
   and the recipe for bringing an overlay machine forward (which keys to
   rename, add or remove, which variables changed). Users will follow it
   step by step.
3. Migrate every machine under `mdb/` in the repository to the new
   generation.
4. Make the cactup source understand it: a binary built from this commit
   embeds the new number.

Two checks back this up. A unit test fails the build when
`mdb/GENERATIONS.md` has no entry for the number in `mdb/GENERATION`. The
`mdb-compat` CI job loads every machine in both directions of the rule (the
MDB at your commit with the generation's first binary, and the generation's
first MDB with your binary) and tells you to bump if either fails. It checks
loading only: a changed template variable, a changed `.py` protocol, or a
new optionlist header key that older binaries silently ignore still needs
your judgment.

After a bump, users on older builds see the "moved to generation" warning
until they update; an older build keeps working with the last MDB of its
generation.

## Overlay machines

A machine in a user's overlay (`~/.cactup/machines/<name>/`) records the
generation it was written for in its `meta.toml`:

```toml
[cactup]
mdb-generation = 1
```

`cactup machine create` writes the current generation for you. A machine
without the key loads with a warning that cactup is assuming its own
generation; add the key to silence it. A machine written for an older
generation is refused until it is brought forward; one written for a newer
generation asks for `cactup update`.

### Updating an overlay machine

If your overlay machine says `mdb-generation = N` and cactup is at a newer
generation:

1. Open `GENERATIONS.md` (the error message says where it is; it is also in
   `~/.cactup/mdb/gen-<M>/GENERATIONS.md`).
2. Apply the recipe of every entry after yours, in order: `## Generation N+1`,
   then `N+2`, up to the current one.
3. Set `mdb-generation` to the current generation.
4. Check that it loads: `cactup machine show <name>`.

If your machine was copied from a shipped one with `--from-existing` and you
changed little, it can be simpler to delete it and copy it again from the
updated MDB.
