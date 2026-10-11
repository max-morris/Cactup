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
mdb-generation = 2
```

`cactup machine create` writes that line. An overlay without it is assumed
to match the running cactup's generation, with a warning. An overlay from an
older generation is refused when it is loaded; the entry for each newer
generation below says what to change to bring it forward.

## Generation 1

The first published generation. Overlays add `mdb-generation = 1` under
`[cactup]`.

## Generation 2

"Variant" used to name two unrelated things, and both are renamed. An
optionlist is now a **flavor** (`cactup build --flavor`), and a machine's
flavors are simply the `.toml` files in its `optionlists/` directory. The
submit, run and build-submit scripts are routed by queue and get tables named
after their directories. Where a flavor's binary may run is grouped under
`[cactup.runs-on]` in its optionlist. That table is checked only when the
binary runs (`sim`, `test`), never for the queue a build is submitted to.

To bring a generation-1 overlay forward:

1. In `meta.toml`, rename the script tables. Their entries keep the same
   shape:
   - `[variants.submitscript]` becomes `[submitscripts]`
   - `[variants.runscript]` becomes `[runscripts]`
   - `[variants.buildsubmitscript]` becomes `[buildsubmitscripts]`
2. Delete the `[variants.optionlist]` table and its `variants = [...]` line.
   Every `optionlists/*.toml` file is a flavor now, so delete any file you
   do not want offered. With more than one flavor, mark the implicit one with
   `default = true` in its `[cactup]` header, as before.
3. Delete `default = true` from every script entry (the queue tables keep
   theirs). A script now serves exactly the queues it lists. There is no
   fallback script, and a kind's only script is no longer implicitly the
   default either. So the normal `[submitscripts]` and `[runscripts]` must
   list every queue, which the load checks. Test scripts serve only the
   queues they list, and a test run never borrows a normal script, so a
   queue no test script lists cannot run the testsuite. The same goes for
   builds and `[buildsubmitscripts]`. An entry left with nothing but its
   queues can return to the array form: `"x" = { queues = ["q"] }` is
   `"x" = ["q"]`.
4. If a script table set `default-universe = "<name>"`, delete that line and
   add `universe = "<name>"` to every entry in that table that did not name
   its own universe. An entry written as an array becomes the table form:
   `"x" = ["q"]` becomes `"x" = { queues = ["q"], universe = "<name>" }`.
5. In every `optionlists/*.toml`, move `gpu` and `compatible-queues` out of
   `[cactup]` into a new `[cactup.runs-on]` table, and rename
   `compatible-queues` to `queues`:

   ```toml
   [cactup]
   description = "CPU-only build"

   [cactup.runs-on]
   queues = ["single", "workq"]
   gpu    = false
   ```

   The `[cactup]` header now rejects keys it does not know, so a leftover
   `gpu` or `compatible-queues` there is an error rather than a silently
   skipped check.

   The same goes for any optionlist of your own that you build from with
   `cactup build --optionlist FILE` and that carries a `[cactup]` header:
   move its keys the same way before the config's next rebuild.

   `[cactup.runs-on]` may also name a `default-queue`: where a simulation or
   test of that flavor goes without `-q` when there is no `queue` knob, or
   the knob names a queue the flavor may not run on, ahead of the machine's
   default. It must be one of `queues`. Set it
   on a flavor that may not use the machine's default queue.
6. Set `mdb-generation = 2` under `[cactup]` in `meta.toml`.
