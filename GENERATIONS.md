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
