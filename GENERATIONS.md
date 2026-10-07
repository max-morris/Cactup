# MDB generations

This file travels with the machine database. It is published on the `mdb`
branch of the Cactup repository alongside `GENERATION`, which holds the
number of the generation the MDB is written in. Every cactup binary is built
for exactly one generation and always reads the newest published MDB commit
of that generation; a binary of an older generation keeps working from the
last revision of its own generation and warns that a newer one exists.

A generation changes whenever an MDB change would break a deployed reader:
either a `meta.toml` key or table that a binary of the current generation
would reject, or a change that makes overlays written for the current
generation invalid. The full rules and the maintainer procedure are on the
documentation site:
<https://max-morris.github.io/Cactup/authors/mdb-generations.html>.

User-MDB overlays (`~/.cactup/machines/<name>/meta.toml`) record the
generation they were written for:

```toml
[cactup]
mdb-generation = 1
```

`cactup machine create` writes that line. An overlay without it is assumed
to match the running cactup's generation, with a warning. An overlay from an
older generation is refused when it is loaded; the entry for each newer
generation below says what to change to bring it forward.

## Generation 1

The first published generation. Overlays add `mdb-generation = 1` under
`[cactup]`.

## Generation 2

What changed: `[paths]` has a new key, `build-cache-home`: where cactup's
build cache keeps its objects when the user's `build-cache-dir` knob does
not say. Several machines set it, to a directory beside their
`simulation-home` on scratch or work storage (a cache written from compute
nodes does not belong in a home quota). A binary of generation 1 does not
know the key, so the MDB that uses it is generation 2.

Bringing an overlay forward (`~/.cactup/machines/<name>/meta.toml`):

1. Under `[cactup]`, change `mdb-generation = 1` to `mdb-generation = 2`.
2. Nothing else is required. Optionally, add `build-cache-home` under
   `[paths]` (an absolute path; `@USER@` and `@ENV(NAME)@` work as in
   `simulation-home`) if this machine's build cache should live somewhere
   other than `$CACTUP_HOME/cache`.
