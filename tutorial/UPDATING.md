# Updating the tutorial for a new cactup

The tutorial runs a pinned cactup. `tutorial/cactup.pin` names one commit of
this repository, and `image/build.sh` builds everything cactup-shaped in the
image from that commit, never from the checkout:

- the two static binaries the update site serves ("previous" and "current",
  for notebook 1's auto-update demo; both from the pinned sources, stamped as
  the last two commits that touched them, as CI stamps releases);
- the installer, `cactup-init.sh`;
- the machine database (`mdb/`), including the tutorial's own machine
  `mdb/cactup-tutorial`.

So cactup can change freely on any branch without changing the tutorial.
Moving the pin is the only way a cactup change reaches it. This file is the
procedure for doing that, for an agent or a person.

The pinned commit must stay reachable: `build.sh` refuses a commit this
repository doesn't have. A merge commit or a fast-forward keeps a branch's
commits; a rebase or a squash merge leaves them behind, so after one, move
the pin to the commit that carries the same tree.

## When to move the pin

- The tutorial needs a cactup change: a fix for something a notebook trips
  over, or a feature a notebook should teach. Land the change in cactup
  first, as a commit of its own, then move the pin to it. Never patch cactup
  inside the image.
- Before a workshop, to catch up with cactup. Freeze the pin some days
  before, so a late cactup change can't reach the attendees untested.
- Not on every cactup commit. While cactup moves fast, the tutorial is the
  stable thing.

## The procedure

Run the `just` commands from `tutorial/` or any directory below it. In
short:

```sh
just pin NEW      # steps 1 to 3: read, check the commands, move the pin
just build        # step 3: the image, rebaking what the new cactup needs
just check-all    # step 4
```

then steps 5 to 7 by hand.

1. **Pick the commit, and read what changed.** It must be in this repository
   (fetch it if it's on another branch, and push it before committing the
   pin). `just pin NEW` starts by listing every commit since the old pin
   that touched cactup's inputs. Read each (`git show --stat ID`), and list
   the changes that touch anything in "What the tutorial depends on" below.
   That list is what you check by hand in step 5.

2. **Check the commands still parse.** `just pin` then builds the new cactup
   outside the checkout (in `~/.cache/cactup-tutorial/pin`; a few minutes
   the first time, about a minute after) and runs
   `tools/check-cactup-usage.py` against it. That takes seconds, and it
   catches renamed or removed subcommands and options before the hour-long
   run. If it reports any, the pin stays where it was: fix each command in
   the notebooks or the image's scripts, and run `just pin NEW` again.

   (`python3 tools/check-cactup-usage.py --image cactup-tutorial:tutorial`
   checks an already built image instead.)

3. **Move the pin and build.** Once the check passes, `just pin` writes the
   new commit's full hash into `tutorial/cactup.pin`, and renames the cactup
   build the notebooks' prose names (`cactup-` and the last commit that
   touched the binary's inputs, in notebook 7). It lists anything else
   under `tutorial/` that still names the old cactup, to check by hand.
   Then run `just build`, and watch its `bake:` lines:

   - "is cached" for every bake means the precomputed builds still match.
   - "building" means cactup changed something a bake's fingerprint reads
     (what it records about a build in `build.toml`). build.sh rebakes those
     by itself. That takes about five minutes per partial bake, about
     twenty-five for B1 and the CUDA bake each, so allow an hour or more.

4. **Run everything:** `just check-all` (the pytest suites, the platform
   smoke test and the replayed build, every notebook, and a whole hub). The
   parts on their own: `just test`, `just check-platform`, `just check`
   (`just check --only 5,7` for some notebooks), `just check-hub`.

   Run them on a quiet host. When other heavy Docker work shares the host,
   the notebook run fails in ways that have nothing to do with the tutorial
   (SLURM socket timeouts, runs hitting their walltime). Sort the failures:

   - **An expected pattern isn't found** (`tests/run_all.py`'s EXPECT and
     RERUN). If cactup only reworded something, update the pattern, and the
     prose that quotes it.
   - **Forbidden output** ("not precomputed", "the source tree has moved", a
     traceback, an `error:` in a cell not marked `--expect-fail`). That is a
     real problem: a bake miss, a catch-up mismatch, or a cactup regression.
   - **A cell failed.** Find out whether cactup changed behavior (fix cactup
     and pin again, or change the notebook on purpose) or the notebook relied
     on something it shouldn't.

5. **Read the notebooks against the new outputs.** A passing run proves the
   commands work and the key lines appear. It doesn't prove the prose is
   still true. For every item on step 1's list, find the notebooks it
   touches (the table below says where) and read their executed copies:
   `in-order/` in the directory `just check` names at the end ("executed
   notebooks:"; `just check --out DIR` picks it).

6. **Review.** A notebook whose prose or cells changed goes through the
   tutorial's review: Reviewer A (fidelity and engineering) and Reviewer B
   (learner and presentation), until both approve in the same round. A pin
   move that changed only patterns needs Reviewer A alone.

7. **Commit.** One commit: the pin, and everything the move needed. Name the
   old and new commits in its message, and add a line to the history at the
   end of this file.

## What the tutorial depends on

Each row is a part of cactup the tutorial relies on, where it relies on it,
and what catches a change to it.

| In cactup | In the tutorial | Caught by |
|---|---|---|
| Subcommand and option names | every notebook's cells, catch-up, `cactup-tutorial-sources-clean`; notebook 10's prose and tables, which name many commands no cell runs | `check-cactup-usage.py` (cells only), and reading (step 5) for notebook 10 |
| Output wording (status lines, errors, notes) | prose that quotes it; run_all's EXPECT, RERUN, NEVER and ERRORS | run_all, and reading (step 5) |
| `cactup list` and `config list` line formats (`- NAME … (active)`), `--version` (`(BUILD `), `database.json`'s `detected` | catch-up's parsing (`ensure_install`, `active_is_tutorial`, `installed_build`, `machine`) | catch-up printing odd lines or failing in run_all |
| `configs/NAME/cactup-config.toml` (`[sources]`, `[thorn-providers]`); the install's `.cactup/fetch-state.toml` | `cactup-tutorial-sources-clean` | `test_catch_up.py` (formats copied there), notebooks 5 and 7 |
| `build.toml` fields (config, cactus-root, machine, build-env, flags, `config-meta` sources, thorn-shapes and thorn-providers, options, thornlist); the attempt dir `.cactup-builds/NNNN`, `build-script` and its steps | the make shim's fingerprint, restore and replay (`image/rootfs/usr/local/lib/cactup-tutorial/make_shim.py`) | bake misses (step 3), "not precomputed" (NEVER), `test_make_shim.py`, `tests/platform/replay.sh` |
| Simulation layout (`output-NNNN`, the `-active` link, `log.txt`), `restart.toml` keys (`CACTUP`, `EXECUTABLE`, `CHECKPOINT_WALLTIME_SECONDS`, `queue`/`QUEUE`, `[universe]`), `submit-script`, `run-script`, `.cactup/ENVIRONMENT` | notebooks 5, 6a, 6b and 7 read these files | run_all patterns |
| Test layout (`~/.cactup/tests/…/results-NNNN/<config>/…`), test selection rules | notebook 9 | run_all |
| Template variables, `@KNOB(…)@`/`@ENV(…)@`, the `.py` script and parfile API, `CactupError` | notebooks 6b, 8 and 10 (10 also maps SimFactory's commands, keys and variables onto cactup's) | run_all, and reading (step 5) |
| The machine database: `mdb/GENERATION`, `meta.toml`'s schema, `machine create`'s output, `machine show`'s, the `qbd` entry | notebooks 6a and 6b (they `%show` line ranges of `meta.toml` files), notebook 10 (`machine show qbd`, `qbd/meta.toml`'s `[build]` table, and prose describing the entry; its `.ini` excerpt comes from the mirrors, not cactup), catch-up's `MYLAB_LINES`, `mdb/cactup-tutorial` | run_all, `test_catch_up.py`, reading (step 5) |
| The installer and the update protocol (`cactup-init.sh`, `latest.json`, `cactup.sha256`), `cactup update`'s output | notebook 1, catch-up's `ensure_cactup`, `image/assemble-update-root.sh` | run_all (notebook 1), `tests/platform/smoke.sh` |
| Known bugs the notebooks work around or describe | notebook 5 (`--overwrite` and the 60-second live window); notebook 7 (`sim stop` on a chain stops one job; single-threaded processes for TerminationTrigger); notebook 9 (a failed run shows FINISHED; test selection by `Thorn` or `Thorn/test` only); notebook 6b (the optionlist header isn't strict); notebook 10 (a submit refused for its parameter file leaves the simulation made) | reading (step 5): when cactup fixes one of these, a notebook's explanation becomes wrong |

Not pinned: the notebooks' links to the documentation site
(max-morris.github.io/Cactup) show the live docs. A pin move is a good time
to check that the pages they name still exist.

## Pin history

| Date | Commit | Why |
|---|---|---|
| 2026-10-07 | 0ea33983 | First pin: the cactup the tutorial was written against (notebooks 1 to 9), including the `inst show -I` fix it needed. |
