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
mdb-generation = 3
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

## Generation 3

The phase environment setups are renamed after their phase, a provisioning
step of its own sits beside each setup, and `[scheduler]` gains two keys for
telling whether a job is still alive.

- `env-build-setup`, `env-submit-setup` and `env-run-setup` are now
  `build-env-setup`, `submit-env-setup` and `run-env-setup`, in
  `[environment]` and in `[universes.*]`. `env-setup` keeps its name. The old
  names are unknown keys: a machine that still has one fails to load.
- New keys, in `[environment]` and in `[universes.*]` (where they replace the
  machine's key by key, as the setups do): `provision`, run in every phase
  wherever `env-setup` is, and `build-provision`, `submit-provision` and
  `run-provision`, run in their phase only. In each phase cactup runs the
  setup (`env-setup`, then the phase's `*-env-setup`), then `provision`,
  then the phase's own `*-provision`. Each provisioning step runs in a
  subshell of its own, under `set -e`: what it exports or changes is lost
  when it ends, and a failure stops the build, the submit script or the run
  script, exactly as under `set -e` (a failing command or `exit N` does; a
  false `a && b`, even on the step's last line, does not). For a queued
  job, `provision` runs twice, as `env-setup` does: in the submit script
  and in the run or build script the job runs. Provisioning is for what makes or fetches what a phase needs (a
  software environment, a file under `$HOME`); a setup now only sets the
  environment, and cactup's tests require every machine's build setup to run
  to its end anywhere. `@ENV_SETUP@` carries the provisioning steps after the
  setup, so a `.py` script that prints `ENV_SETUP` provisions with no change.
  Provisioning is not run before the scheduler commands (`submit` and
  `blocking-submit`, which get the submit universe's setup; `get-status`,
  `get-status-many`, `stop` and `exec-host`, which get the machine's own
  `env-setup` and `submit-env-setup`), by the `make <config>-realclean`
  script a build adds when the Cactus flesh has moved, or by
  `cactup cache doctor`.
- A `.py` build-submit script's `ENV_SETUP` is now the submit phase's block
  in the submit universe (the route's `universe`, else a declared `host`),
  the same block a `.sh` build-submit script gets at its top. Before, it was
  the submit phase's block in the build universe, so a build universe's
  `env-setup` and `submit-env-setup` reached a `.py` build-submit script
  (and its provisioning keys would have); they no longer do. The other way
  round, the submit universe's keys (the route's `universe`, else a
  declared `host` universe) now reach it, where before they did not. Put
  what the batch script needs in the machine's `submit-env-setup` (or the
  submit universe's).
- New, optional, in `[scheduler]`: `job-ids = "scheduler" | "pids"` (default
  `"scheduler"`; `"pids"` declares that a job id is the pid of a process on
  the host the job was submitted from, as on a machine whose `get-status` is
  `ps @JOB_ID@`), and `unknown-job-pattern`, a regular expression matching a
  line of `get-status`'s output (stdout or stderr) that says the scheduler
  does not know the job, with `@JOB_ID@` substituted as in the other
  patterns. The first binaries of this generation do not act on either;
  later ones do, with no further generation: with `"pids"` they record the
  submitting host and the process's start time with each job id and ask
  `ps`/`stop` only there and only while the pid is still that process, and
  stop only a job whose process group holds nothing but the job and the
  processes it started (so such a machine's `submit` should start each
  job in a group of its own with `setsid`, as the in-repo ones now do);
  with `unknown-job-pattern`, a status query whose output has neither a
  line for the job nor a line the pattern matches is a failed query, which
  `stop` and `delete` refuse on, so the pattern must match what the command
  really prints for a job the scheduler no longer knows.

To bring a generation-2 overlay forward:

1. In `meta.toml`, rename every `env-build-setup` to `build-env-setup`,
   `env-submit-setup` to `submit-env-setup` and `env-run-setup` to
   `run-env-setup`, in `[environment]` and in each `[universes.<name>]`
   table.
2. Read each setup (`env-setup` and the three above, in `[environment]` and
   in each universe). Move anything that makes or fetches something (creates
   a directory or a file, downloads, installs or builds a package, writes a
   cache) into the provisioning key of the same scope: from `env-setup` into
   `provision`, from `build-env-setup` into `build-provision`, and so on,
   in the same table. Keep in the setup what the phase must see: `export`s
   (of directories the provisioning step creates, too), module loads, the
   module system's init file. A provisioning step runs after the setup, so
   it can use what the setup loads, but nothing it exports reaches the
   phase. The setup may name what provisioning creates (`PATH` and other
   exports of directories it makes) but must not source or run it: on a
   host where it does not exist yet, `. ~/venv/bin/activate` (or a conda
   `activate`) left in the setup is fatal before provisioning ever makes
   it. Replace such a line with the exports it would make (`export
   PATH=~/venv/bin:$PATH`, as rostam's `arm-conda` does). For example, a build setup that wrote a stub file and put its
   directory on `PERL5LIB`:

   ```toml
   build-env-setup = """
   stub=$HOME/.cache/my-perl5
   [ -e "$stub/diagnostics.pm" ] || { mkdir -p "$stub"; printf '...' > "$stub/diagnostics.pm"; }
   export PERL5LIB=$stub${PERL5LIB:+:$PERL5LIB}
   """
   ```

   becomes

   ```toml
   build-env-setup = """
   export PERL5LIB=$HOME/.cache/my-perl5${PERL5LIB:+:$PERL5LIB}
   """
   build-provision = """
   stub=$HOME/.cache/my-perl5
   [ -e "$stub/diagnostics.pm" ] || { mkdir -p "$stub"; printf '...' > "$stub/diagnostics.pm"; }
   """
   ```

   Universes: a universe that sets a provisioning key replaces the
   machine's, key by key, as with the setups, and one that does not
   inherits the machine's. So check every universe that set its own copy of
   a setup you moved lines out of (its own `env-setup` when you moved lines
   from the machine's `env-setup` into `provision`, and so on): in
   generation 2 its copy replaced the machine's whole setup, so the moved
   lines never ran there; now it inherits the machine's provisioning key
   and runs it (in a build or run script inside its wrapper: a container,
   which may lack the `python3` or `curl` the step uses). Unless the step
   should run there
   too, set the key empty in that universe: `provision = ""` (or
   `build-provision = ""`, and so on) turns the machine's off there.

   A provisioning step runs under `set -e`, so a command in it that fails
   stops the phase; a false test in `a && b` or `a || b` does not. For a
   build that is no change: the build script always ran its setup under
   `set -e`. For a submit or run script it is. There the setup is put at
   the top of the script, ahead of any `set -e` of the script's own (every
   submit and run script in the MDB, `.sh` or `.py`, puts it there). A run
   script runs in the interpreter its first line names, `bash` in the MDB.
   A submit script runs as the scheduler or `[scheduler].submit` starts
   it: under `sbatch` and `exec nohup @SCRIPTFILE@` (or `setsid`) in the
   interpreter its first line names (`bash` in the MDB); under PBS or
   Torque `qsub` (crux, qb, smic, thornyflat) in the user's login shell,
   since none of their scripts sets `#PBS -S` (usually `bash`); and under
   `sh @SCRIPTFILE@` (hal1, hal2, hedges, mel5, spine) in `/bin/sh`. So in
   generation 2 a failing line of `env-setup`, `submit-env-setup` or
   `run-env-setup` there was not fatal, unless the script ran under
   `set -e` from its first line, with one exception: in `/bin/sh` (those
   five machines' submit scripts, or a login shell that is `sh`) `.` of a
   missing file ends the script, where `bash` only prints an error and
   goes on. A step
   moved into `provision`, `submit-provision` or `run-provision` now stops
   the submit or run script whenever it fails, in any shell. Make sure it
   succeeds wherever it runs, or end what may fail with `|| true`.
3. If a `.py` build-submit script of yours prints `ENV_SETUP`: it now gets
   the submit universe's block, as a `.sh` one always did, not the build
   universe's. If it relied on a build universe's `env-setup` or
   `submit-env-setup` reaching it, move what the batch script needs into
   `submit-env-setup` (the machine's, or the submit universe's); and check
   that the submit universe (the route's `universe`, else a declared `host`
   universe), whose keys never reached it before, does not now set
   something it should not.
4. Optionally, in `[scheduler]`: on a machine whose jobs are background
   processes (`get-status = "ps @JOB_ID@"`), add `job-ids = "pids"`; on a
   batch system, add `unknown-job-pattern` with what its status command says
   of a job it does not know: `'slurm_load_jobs error: Invalid job id
   specified'` for Slurm's `squeue -j @JOB_ID@`, `'Unknown Job Id'` for
   Torque's `qstat @JOB_ID@`, and `'Unknown Job Id|Job has finished'` for
   PBS Pro's (the second for a finished job, when job history is on).
5. Set `mdb-generation = 3` under `[cactup]` in `meta.toml`.
