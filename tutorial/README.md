# Cactup notebook tutorial — maintainer guide

This directory builds a Docker image that serves a hands-on, notebook-based
Cactup tutorial in JupyterLab, and a JupyterHub deployment that gives every
workshop attendee their own container. It is modeled on the Einstein Toolkit
tutorial server (`einsteintoolkit/jupyter-et`), with one notebook per topic.

Attendees never see this file. Everything below is about how the image works
and how to maintain, rebuild and deploy it.

## Layout

| Path | What it is |
|---|---|
| `notebooks/*.md` | Notebook sources in jupytext MyST format (reviewable diffs). The image converts them to `.ipynb`. |
| `jupyter/` | Python package `cactup_tutorial`: the IPython magics (`%%shell`, `%%file`, `%show`), Pygments lexers, and a prebuilt JupyterLab 4 labextension (TypeScript source in `jupyter/labext/`). |
| `image/` | The image: `Dockerfile`, `build.sh` (orchestrates base image → mirrors → bakes → final image), and `rootfs/` (entrypoint, SLURM config, the `make` shim, the update server, the home skeleton, the catch-up and reset tools). |
| `bake/` | Drives real installs and builds inside a bake container and harvests their results. |
| `mirrors/` | Turns thornlists into local bare git mirrors, a lock file of mirrored commits, and per-repository `insteadOf` rules. |
| `deploy/` | JupyterHub + DockerSpawner + Caddy compose file, the choose-your-own-login authenticator, the session-token tool. |
| `tests/` | pytest suites and the headless notebook runner `run_all.py`. |

The tutorial machine itself is a regular MDB entry, `mdb/cactup-tutorial/`
(discovered on hostname `cactup-tutorial`), like `mdb/et-juphub/` before it.
The image's mirror of Cactup's `mdb` branch is built from this checkout's
`mdb/` directory, so the image works before the entry is published; the entry
still has to pass `cargo test` and `ci/mdb-generation-guard.sh`.

## Notebooks

The first six follow the topics as originally specified; the rest were added
after. Each opens with what it teaches, a time budget, and a
**catch-up cell** (below), and ends with links to the matching cactupdocs
pages.

| # | Notebook | Budget |
|---|---|---|
| 1 | Intro and tour: the terminal cells, installing cactup with the documented installer, auto-update, a SimFactory-to-cactup cheat sheet, `releases`, installing `ET_2026_05_v0` | 30 min |
| 2 | Build a config, submit a short simulation, look at and plot the results | 30 min |
| 3 | A second install to experiment on; point the flesh and CarpetX at the mixed-precision forks; refetch | 30 min |
| 4a | Multiple installations: the CarpetX-only thornlist `carpetx.th`, `master`, switching, provenance | 25 min |
| 4b | Multiple configs: a GPU variant and a debug build, config management | 25 min |
| 5 | Topology on submit, the delta checker while hacking on thorns and the flesh, optionlist variants and universes | 40 min |
| 6a | MDB overlays, and a first machine entry of your own | 30 min |
| 6b | Complex entries: queues, per-queue script variants, `.py` scripts, knobs, optionlist headers, universes | 40 min |
| 7 | Monitoring and restarts: logs, the follow view, walltime chaining, checkpoint and recovery | 30 min |
| 8 | Parfile templates, Python-generated parfiles, a small parameter sweep | 25 min |
| 9 | Test suites, and troubleshooting a broken build and a broken run | 30 min |
| 10 | SimFactory migration, in depth | 20 min |

Notebook 1 installs the full `ET_2026_05_v0` release, but the config the
tutorial builds and hacks on, `tutorial`, uses a curated **CarpetX /
Cottonmouth subset** of it (`/opt/cactup-tutorial/thornlists/tutorial.th`,
passed with `--thornlist`): CarpetX and its test thorns, Cottonmouth's Z4c
evolution (`CottonmouthZ4c4m`) with its linear-wave initial data, and the
thorns and ExternalLibraries they need. The release links all of them (it
does not link SpacetimeX's own Z4c). That is what CarpetX users really build, it
teaches `--thornlist`, and it keeps the object tree, the restore and real
incremental rebuilds small enough for a room of attendees on one VM. The file
is read-only: a config records its thornlist's path and keeps using it, so an
edit to it would change every later build's fingerprint and miss every bake.

Notebook 3 works in a **separate install**, a custom one from the subset
thornlist (`cactup install --thornlist /opt/cactup-tutorial/thornlists/tutorial.th
--alias et-mp --symlink-name et-mp --silent`; cactup refuses a release name
together with `--thornlist`, and `inst show` names `tutorial.th` as its
source), so the stock install keeps the build that notebooks 5 and 9 edit and
rebuild incrementally, and the second install is small and quick.

In commands below, `tutorial.th` abbreviates
`/opt/cactup-tutorial/thornlists/tutorial.th`; cells and catch-up spell the
full path out.

Constraints the notebooks must respect (each found by reading the code):

- **Every install after notebook 1's passes `--symlink-name <alias>`.** With
  `--silent`, `cactup install` creates a symlink in `$HOME` named after the
  source tree's root (`~/Cactus`) and silently replaces an existing one, and
  `uninstall` leaves it behind. So `~/Cactus` always means the stock install,
  which notebooks 5 and 9 and catch-up address through it, and each other
  install gets its own link (`~/et-mp`, `~/carpetx`, `~/et-master`). 4a points
  this out, and removes an uninstalled install's link. Catch-up puts
  `~/Cactus` back if it is missing or points elsewhere.

- **1:** cactup is not preinstalled. The notebook shows the one-line
  installer from the documentation's landing page, says the VM has its own
  update site, and runs the same `cactup-init.sh` against it:
  `curl -sSf http://127.0.0.1:8765/cactup-init.sh |
  CACTUP_UPDATE_ROOT=http://127.0.0.1:8765 sh`, answering the installer's
  confirmation prompt through the cell (the landing page's
  `--proto '=https'` has to go for a plain-http localhost URL). The kernel's
  `PATH` already contains `~/.cactup/bin`, so the next cell finds `cactup`.
  The installer's shell-profile edit is what puts it on the Terminal's
  `PATH` (Debian's `/etc/profile` resets `PATH` for the login shell the
  Terminal runs), which notebook 7's follow view needs. The installer ends by
  printing documentation links on `127.0.0.1:8765`, which is the container,
  not the attendee's laptop; the notebook says so and links the public
  documentation instead. See Auto-update for why this installs the older
  build. The notebook also says openly that the VM serves the Einstein
  Toolkit's repositories from local mirrors (a whole release installs in
  seconds, and plain git shows it: `git remote -v` and `git fetch -v` print
  `file:///opt/cactup-mirrors/...`), while cactup, `origin` and the
  thornlists all name the upstream repositories, as they would on a cluster
  whose site mirrors GitHub.
- **3:** after installing, the notebook runs `cactup use et-mp` (installing
  makes a new install active only when none is), previewing what 4a teaches.
  Its last cell is an optional "start over" (`cactup uninstall et-mp
  --force`, then removing `~/et-mp`), since running it again from the top
  otherwise shows only "already exists" and "up to date". It edits only
  `et-mp`'s live thornlist, never
  `tutorial.th`, and et-mp's `tutorial` config is built without `--thornlist`,
  so it follows that live list (and after the refetch cactup's own report
  says to run `cactup build tutorial`, not to pass a thornlist). The forks
  keep their upstream repository names (cactup names a repository's
  directory after its URL's basename), so the flesh stays at the same path
  and cactup sees it move by commit. The refetch skips a repository whose
  origin differs from the thornlist ("remote URL changed") unless forced, so
  after the dry run the cell passes `--overwrite` for the flesh and CarpetX
  repositories, and the output shows cactup's "origin re-pointed" block.
  B2a and B2b are baked exactly this way. (Pending bake B2b: stock NewRadX
  may not compile against the fork's CarpetX, in which case SpacetimeX joins
  them; see `thornlists/NOTES.md`.)
- **4b:** flags are sticky but `cactup build EXISTING --debug` on an
  up-to-date config does nothing (the up-to-date check returns before flags
  are recorded), so the debug build is a new config, `tutorial-debug`.
  `--force-queue` is explained, never run. The container has no CUDA toolkit
  (only the bake container for B3 does), so the notebook says openly, as
  notebook 1 does for the mirrors, that the GPU build was prepared ahead of
  time for a GPU node, and the `gpu` optionlist it reads describes the
  toolkit that build used. Its GPU config goes to the `gpu` queue with `-q
  gpu` (not `--force-queue`, whatever cactup's refusal suggests), which
  `sinfo` shows inactive without a GPU: the submit is then refused by SLURM
  ("Required partition not available (inactive or drain)"), and the refused
  simulation stays listed as ACTIVE with no job, so the cell uses a
  throwaway simulation name and deletes it afterward. The same goes for the
  cell showing cactup's own refusal to put the GPU config on a CPU queue:
  cactup creates the simulation before it refuses, and running the cell again
  would otherwise fail with "already exists".
- **5:** its variant build is a separate config in a build universe,
  `build tutorial-pinned --universe pinned --thornlist tutorial.th` (bake
  B5), whose wrapper is `taskset -c @ENV(CACTUP_TUTORIAL_CPUS)@`: each
  container's cpuset has different CPU numbers, which the entrypoint
  exports. It comes before the hacking section, while
  the stock install's repositories are still clean (its fingerprint includes
  their state, like every bake's). The prose explains that changing an
  existing config's universe forces a full rebuild, and that the universe is
  not sticky (a plain `build` resolves it afresh and calls any difference a
  full rebuild), so `tutorial-pinned` is never rebuilt without `--universe
  pinned`; its simulations run in `pinned`, which the notebook shows. It
  points back to 4b for optionlist variants rather than baking another.
- **5 and 9:** their hacking builds only the stock `tutorial`. The other
  configs keep partial trees (no objects), so after a source edit their next
  build would be a real from-scratch one. The `.ccl` edit is in a small test
  thorn, and the flesh edit is to a `.c` file, not a widely included header.
  Opening a thorn's files in the editor must not change the thorn: see
  Editor files below.
- **6a/6b:** `machine create` without `--no-discover` claims this host (it
  writes a `hostname.regexp` and caches itself as the detected machine), and
  it autodetects `[hardware]` from `/proc`, which reports the VM's cores, not
  the container's cpuset. So: `machine create mylab --from-existing
  cactup-tutorial --no-discover --silent` (without `--silent` it prompts for
  homes and knobs), `--machine mylab` on the commands that use it, fixing
  `[hardware]` as the first exercise, and `machine delete mylab` plus
  `machine forget` at the end. `--from-existing` copies the optionlists and
  scripts verbatim but writes `mylab/meta.toml` afresh, without the
  original's comments, with `[paths]` spelled out and `[hardware]`
  autodetected; 6a shows the commented original (the file `machine show
  cactup-tutorial` names) next to the attendee's copy, since its comments
  explain what the first exercise fixes. The copy also keeps
  `nickname = "cactup-tutorial"`, which the first exercise fixes along with
  `[hardware]`. A config built for `cactup-tutorial` is
  refused on `mylab` (`check_machine`), so submits there use
  `--ignore-machine` on the stock `tutorial`. 6b builds nothing. `mylab`
  lives from the start of 6a to the end of 6b: `machine create` refuses an
  existing name, so catch-up 6a moves a leftover `mylab` to
  `~/tutorial-saved/<time>/mylab` (and forgets it) and says so, and that the
  notebook continues from its `machine create` cell. Catch-up 6b
  creates `mylab` with 6a's finished entry if it is missing, and otherwise
  checks the keys 6a sets (`[hardware]`: cores and memory, and the rest of
  6a's edits): any that differ from 6a's finished values, for example because
  the `[hardware]` exercise was skipped and `/proc`'s values are still there,
  it sets, after saving the attendee's `meta.toml` to `~/tutorial-saved/`,
  with one line saying so. It touches only 6a's keys, so running it again in
  the middle of 6b keeps 6b's additions.
- **7:** walltime chaining runs on the `short` partition (3 minutes), in two
  segments; the parfile's checkpoint and recovery path is validated before the
  notebook is written. Every `short` submit passes `--checkpt-buffer
  00:01:00`: cactup's default buffer (at least ten minutes) is longer than a
  whole segment, and cactup doesn't warn about it. The notebook also presents
  what happens to a run when the container stops (the idle culler over lunch):
  its job is requeued and runs its segment again from the start (a
  checkpointing run loses at most that segment), `sim show` shows it queued
  on the same restart, and its log keeps the interrupted attempt; the
  simulation's `log.txt` records the second "compute-node run". Its prose
  keeps the two meanings of "restart" apart: cactup's restarts are
  `output-NNNN` (what `sim list` counts), a requeue reruns the same one (the
  log's "Times SLURM requeued this job").
- **No random tips:** the skeleton turns cactup's after-command tips off
  (`wisdom-frequency off`): every cell is a terminal, so they would appear
  in one cell in eight, some wrong for this machine, and the notebooks'
  output would differ run to run. The bake home does the same. A notebook
  may still show `cactup wisdom` on purpose.
- **No pipes into `head`:** cactup panics on a closed pipe ("failed
  printing to stdout: Broken pipe"), so cells never pipe it into `head` or
  the like.
- **Follow modes** (`sim log -f/-o/-e`, `build log -e`) run until
  interrupted, so any cell using one passes `%%shell --timeout`.
- **8:** sweep points run 1 task × 1 thread so all three share the node at
  once.
- **9:** tests run with `test submit`: a foreground `test run` is refused
  outside an allocation because the machine sets `allocation-env`. The run
  is a small subset (a few CarpetX tests on 1 process), with a target of
  5 minutes, not the full CarpetX and Cottonmouth suites. If the attendee skipped the
  fix cell, the closing revert is "up to date" with no make (the failed build
  recorded nothing), so the notebook's text doesn't promise a recompile.
  `build log -e` follows the log until interrupted, so its cell uses
  `%%shell --timeout`. The fix for the deliberately broken file differs from
  the original text: a failed
  build records no sources, so restoring the original bytes would make cactup
  call the config up to date and run no make (correct, but not the rebuild
  the cell is there to show). Debugging is taught with `build log -e`,
  `--trace` and `-v`, never by rebuilding `tutorial-debug`.
- **Every build runs in the foreground:** `mdb/cactup-tutorial` sets `[build]
  default-action = "run"` (with a build-submit script variant and a
  `[scheduler].submit`, `cactup build` would otherwise go to the queue).

### Editor files

A thorn's shape, which cactup fingerprints to notice added and removed files,
counts every file under the thorn's `src/`. Opening a file in JupyterLab's
editor saves a checkpoint of it, by default next to it in
`.ipynb_checkpoints/`, and vim keeps a swap file beside the file it edits.
Either would change the thorn's shape, recompile the whole thorn and, since
the shape is part of the bake fingerprint, turn every later hit into a
from-scratch miss. Two things prevent it:

- The server's checkpoints class (`cactup_tutorial.checkpoints`) keeps
  checkpoints under `~/.local/share/jupyter/checkpoints`, in the server's
  directory layout, so source trees never get an `.ipynb_checkpoints`.
- cactup honors `.cactupignore` files (`.gitignore` syntax, per repository
  and directory, plus a user-global `~/.cactup/cactupignore`) that exempt
  files from the shape. The home skeleton's global file lists
  `.ipynb_checkpoints/`, vim's swap files (`.*.sw?`) and the backups
  JupyterLab leaves when a save fails (`.~*`). The bake home gets the same
  file, so baked shapes and attendees' shapes are computed alike.

`cactup config delta` compares only repositories' commits and tracked edits,
so it cannot see a shape change; the build can. The end-to-end test therefore,
after notebook 2's build: creates a checkpoint of a thorn source through the
contents API (`POST /api/contents/<path>/checkpoints`, which is what
JupyterLab does on opening a file) and saves the file unchanged; plants a
vim swap file (`src/.x.cc.swp`) beside it; asserts that `cactup build
tutorial` says the config is up to date; and asserts that there is no
`.ipynb_checkpoints` anywhere under the Cactus tree. So the check can't pass
vacuously, it also asserts that the checkpoint request returned 201 and that
the checkpoint is under `~/.local/share/jupyter/checkpoints`. The swap file
stays for the rest of the run, so every later bake hit also shows the global
ignore file at work.

Notebook 5 shows the skeleton's `~/.cactup/cactupignore` and mentions
per-repository `.cactupignore` files while attendees hack on thorns. No
notebook adds a pattern that matches a file the thorns really have: that
would change their shapes and miss every later bake.

### Catch-up and reset

Attendees skip notebooks, restart kernels and arrive late, and the notebooks
depend on state earlier ones created (installs, the active installation,
configs, simulations). So:

- Every notebook's first code cell runs `cactup-tutorial-catch-up N`, which
  brings the container to the state notebook N assumes (installing from the
  mirrors and building from the bakes if needed, selecting the right
  installation and config). It is idempotent and prints one line per thing it
  had to do, nothing when there was nothing to do. It also undoes what would
  derail later notebooks: it removes a `~/.hostname`, a user overlay named
  `cactup-tutorial` and a cached detection of any other machine, and moves
  aside (to `~/tutorial-saved/`) any other user machine whose
  `hostname.regexp` or `discover.py` claims this host, for example one made
  without `--no-discover` (with two claimants cactup refuses to pick one
  without a terminal, and asks in a cell). Its own
  cactup calls never run on a terminal (so they never trigger the update
  check). For N ≥ 2 it runs the installer if cactup is missing and applies
  the pending update with `cactup update` itself, so an attendee who skipped
  notebook 1 doesn't get the update banner in the middle of notebook 2's
  build. It runs the installer with `-y` (the cell's terminal would otherwise
  get the confirmation prompt) and without `--no-modify-path` (see notebook
  1). If `~/.cactup/bin/cactup` is neither the "previous" nor the "current"
  build (an attendee ran the public one-liner, and containers can reach the
  internet), it reinstalls from the local update site: a foreign build's
  output differs from the notebooks', and a build-record change would miss
  every bake. `cactup-tutorial-catch-up 1` runs no cactup at all.
- Catch-up only ever needs the stock `ET_2026_05_v0` install and the stock
  `tutorial` build (B1): notebooks 2, 4a, 4b and 10 need the install,
  notebooks 5 to 9 also need `tutorial`, and every other install or config
  (`et-mp`, the `carpetx.th` install, `master`, the variant configs) is made by the
  notebook that uses it. So a late arrival waits at most for one install from
  the mirrors (within 10 minutes under load, see Sizing) and one restore.
- Notebooks 5 and 9 edit a fixed list of files in the stock install (a thorn
  source, a small thorn's `.ccl`, a flesh `.c` file, notebook 9's broken
  file). For every N ≥ 2, catch-up puts any of them that are modified back to
  their committed state: the partial-tree configs (the variant configs of 4b,
  `tutorial-pinned`) have no objects, so building one on an edited tree would
  be a from-scratch real build (for `tutorial-gpu`, a CUDA build in a
  container without CUDA), and the bakes all assume clean repositories. This
  covers restarting the kernel and running all of notebook 5 in the middle of
  its hacking section, skipped revert cells, doing notebook 9 before
  notebook 5, and going back to 4b after either. Before restoring a file,
  catch-up saves the attendee's version outside the Cactus tree (a copy
  beside it would change the thorn's shape) and says where, one line per
  file: `put <file> back to its committed state (notebook 5 edits it); your
  version is at ~/tutorial-saved/<time>/<file>`. Restoring always comes before
  building.
- For N ≥ 5, catch-up then always runs `cactup build tutorial --thornlist
  tutorial.th` on the stock install, whether or not it restored anything this
  time: the executable may have been built from edits an earlier catch-up
  (say, 4b's after a stopped notebook 5) put back. It is quiet when cactup
  says the config is up to date (seconds). Otherwise (a restore of B1 for a
  late arrival, or a real incremental build of the restored files when the
  executable was built from edits) it first prints one line saying which and
  why, then passes cactup's output through, so a long build never looks like
  a hang and nothing the shim prints is swallowed. So from notebook 5 on, no notebook
  runs an executable built from notebook 5's edits, and cactup never prints
  its "the source tree has moved since config tutorial was built" note on a
  submit. Passing `--thornlist` means a missing config is built from the
  subset, never from the install's full thornlist. The hacking
  sections of notebooks 5 and 9 say up front that later catch-up cells do
  this, and ask attendees to close the edited files' editor tabs when they
  finish (an open tab would offer to overwrite the restored file on its next
  save). Notebook 9 also ends by reverting its file and rebuilding, so it
  leaves the tree clean itself.
- `cactup-tutorial-reset` cancels the attendee's SLURM jobs and wipes
  `/home/cactus` back to the skeleton (asking first), and
  `cactup-tutorial-reset --notebook N` restores one notebook's pristine copy
  from `/opt/cactup-tutorial/notebooks` — which is also how a fix made
  mid-workshop reaches attendees who already started.

The headless end-to-end test runs every notebook both in order and alone in a
fresh container (the "alone" runs in parallel, relying on catch-up), plus
four out-of-order cases: notebook 3 run twice, with its "start over" cell
between; notebook 9 before notebook 5; notebook 5 run again
from the top after stopping in the middle of its hacking section; and notebook
4b after stopping in the middle of notebook 5, then on through notebook 7,
asserting that cactup's "source tree has moved" note never appears. Every
notebook's cells must be safe to run again: the `tutorial-pinned` cell in
notebook 5, for instance, checks every repository in `tutorial.th` for
modified tracked files (B5's fingerprint covers them all) and, if any is,
lists them and says to run the notebook's catch-up cell (which restores the
files notebooks 5 and 9 edit, saving the attendee's versions) and to revert
anything else by hand, instead of building (a config with no objects on an
edited tree would be a from-scratch build). In every run the shim's "not
precomputed" line must never appear.

## How the illusions work

The tutorial has to look exactly like real Cactup use on a real cluster, but a
workshop can't wait hours for Einstein Toolkit builds or hammer Bitbucket with
thirty simultaneous clones. Three mechanisms make the slow parts fast without
changing what cactup itself does or reports.

### Git mirrors (installs and refetches)

`mirrors/mirror.py` reads every thornlist the tutorial uses (the `ET_2026_05_v0`
release, `master`, the CarpetX mixed-precision forks, the CarpetX-only
thornlist), plus the ET manifest and Cactup's `mdb` branch, and runs
`git clone --mirror` on each repository into
`/opt/cactup-mirrors/<host>/<path>.git`. It then:

- repacks each mirror (`git repack -ad`); cactup clones shallowly, which
  reachability bitmaps don't speed up, so the concurrent-install target under
  Sizing rests on measurement;
- records every mirrored ref in `tutorial/mirrors/mirrors.lock`, which is
  committed: an image build brings the mirrors to the lock, so rebuilding it
  reproduces the same commits (and the same bakes) until the lock is updated
  on purpose (`build.sh --update-mirrors`);
- writes **one `insteadOf` rule per mirrored repository** to the system
  gitconfig, for every spelling of its URL the thornlists use (with and
  without `.git`, case variants):

  ```ini
  [url "file:///opt/cactup-mirrors/github.com/EinsteinToolkit/CarpetX.git"]
      insteadOf = https://github.com/EinsteinToolkit/CarpetX
      insteadOf = https://github.com/EinsteinToolkit/CarpetX.git
  ```

  A URL nobody mirrored is normally left alone, so an attendee who adds their
  own fork gets a real network fetch (or an honest network error), not a
  confusing missing-mirror one. git and gix match `insteadOf` by plain string
  prefix, though, so a `.git`-less rule for `…/CarpetX` would also capture
  `…/CarpetXFoo.git`; `mirror.py` refuses to write a rule that is a proper
  prefix of any other URL the thornlists use unless a longer rule covers it. A
  test checks that every URL in every tutorial thornlist resolves to a mirror.

cactup's git layer (gix) applies these rewrites when it connects, so `cactup
install`, `cactup inst refetch` and the MDB sync fetch from the mirrors, with
real progress output, while `origin`, `fetch-state.toml` and refetch's
messages still name the upstream URLs. Everything in cactup that compares or
fingerprints a remote URL (refetch's "remote URL changed" check, the build's
thorn-shape fingerprint) uses the configured URL, not the rewritten one, and
the MDB sync's reachability check probes the rewritten one, so a mirror is
never mistaken for a repoint and the container needs no route to GitHub.

Because clones go through `file://`, the image needs a `git` binary for
`git-upload-pack`.

### Build replay (the `make` shim)

cactup's build script (`/bin/sh`, `set -e`, run in the Cactus root) runs the
machine's `[build].make` command — for `cactup-tutorial`, `make -j@MAKEJOBS@`,
a bare `make` found on `PATH` — as these steps:

```sh
make -jN NAME-realclean          # only for a full rebuild of a configured config
echo yes | make -jN NAME-config options=<cfg>/.cactup-builds/NNNN/cactup-optionlist.cfg \
                               THORNLIST=<cfg>/.cactup-builds/NNNN/cactup-thornlist.th
make -jN NAME-clean              # only with --clean
make -jN NAME
make -jN NAME-utils
```

`image/rootfs/usr/local/bin/make` sits ahead of `/usr/bin/make` on `PATH`. It
starts as a few lines of `sh`. A top-level invocation (`MAKELEVEL` unset)
whose parent process is the `/bin/sh` running an attempt's `build-script`
(exactly that name) goes on to the Python part. Every other invocation
`exec`s `/usr/bin/make` at once, with `argv[0]` set to `/usr/bin/make` so
every recursive `$(MAKE)` inside Cactus's makefiles bypasses the shim
entirely, after one piece of bookkeeping when it is top-level: a
`NAME-realclean` or `NAME-clean` target (the separate `build-script-realclean`
cactup may run, or one typed by hand) removes that config's restore marker,
and a `NAME-config` or plain `NAME` target for an existing config marks it
*built on* (below).
Only the few top-level steps of a build reach Python.

**The fingerprint** is read, not recomputed. The `options=` argument names
the attempt directory, and its `build.toml` already records what decides the
build's product, in cactup's own terms (the key names are `build.toml`'s):

- the config name, the Cactus root and the machine;
- the frozen build-phase environment (`build-env`: machine and universe
  setup, as text);
- the build flags;
- `config-meta.sources`: each repository's HEAD plus whether it has
  tracked-file modifications, exactly as cactup judges them (untracked files,
  such as test output inside source trees, don't count);
- `config-meta.thorn-shapes` and `config-meta.thorn-providers`: each thorn's
  shape (the names of the thorn directory's own files and of everything under
  its `src/`, less what `.cactupignore` exempts, plus the contents of its
  `.ccl` and `make.*` files) and the repository that provides it, so a shape
  change cactup acts on (by dropping that thorn's build state) is also a
  miss;

plus the contents of the option file and a normalized parse of the thornlist
(repository URL, branch and thorn entries; comments and whitespace don't
matter), both with the attempt directory normalized out. A test pins the
`build.toml` fields the shim reads, so a cactup schema change fails loudly
instead of silently missing every bake. The frozen copy of the global
`.cactupignore` (`global-shape-ignore`) is left out on purpose: the shapes
already reflect what it exempts, and fingerprinting its text would make a
harmless edit to it (a comment) miss every bake.

One known gap: `build.toml` holds the sources and shapes as cactup read them
when it prepared the attempt. A build that then waited more than 30 seconds
for the config's lock reads them again without writing them back, so the shim
could fingerprint the older reading. Nothing in the tutorial holds a config's lock
that long, so this is accepted.

**The restore marker** (`configs/NAME/.cactup-tutorial-restored`) has four
states:

- *absent*: the tree is whatever real makes left in it, possibly nothing (a
  new config, or one a realclean or clean has emptied);
- *restoring, bake X*: a restore of bake X has started swapping entries in
  and not finished;
- *pristine, bake X*: the tree is exactly bake X's;
- *built on, bake X*: a real make has run on bake X's tree since.

A passed-through `NAME-realclean` or `NAME-clean` removes the marker before it
execs make. A passed-through `NAME-config` or `NAME` turns *pristine* into
*built on* before it execs make. A restore writes *restoring* before its first
swap and *pristine* last.

The shim decides at the config step. An attempt compiles *from scratch* when
its `build.toml` says `full-rebuild`, when the parent's `build-script` (its
path is in the parent's argv) has a `NAME-clean` step still to come, or when
there are no objects under `build/`. Then:

- a hit with the marker *restoring* (any bake): restore, then replay;
- a hit with the marker *pristine* for this bake: replay only if the attempt
  compiles from scratch (re-placing the executable if it is missing);
  otherwise pass through, since the tree is already current and the real make
  reruns CST and configure and finds nothing to compile;
- any other hit that compiles from scratch: restore, then replay;
- any other hit: pass through. This is the "edit, rebuild, revert, rebuild"
  case of notebooks 5 and 9: the reverted files carry new mtimes, so only they
  recompile (the whole thorn after a `.ccl` revert, whose shape changes back,
  so cactup drops that thorn's build state again). Correct, and honest: a
  revert shows a small recompile, not a thirty-second "full" build;
- a miss: pass through. If the attempt compiles from scratch, the shim first
  prints a short note saying the build was not precomputed and will take a
  long time on this VM, and what differs from the nearest bake: the
  repositories with edits (`cactup config delta` lists them) and the thorns
  whose shape differs, by name, since `config delta` can't show those (an
  untracked `.orig` or backup file under a thorn's `src/` is enough).
  Reverting brings the fast build back.

After `-f` or `--reconfig`, the real `NAME-realclean` empties the tree and
removes the marker, and the attempt is full, so the config step's hit
restores the tree again. After a thorn edit the fingerprint misses (the
repository is modified) and the build is real and incremental, on the tree an
earlier restore left.

**The replay** is tied to one build script: the shim keys its decision on the
parent shell's pid and start time (from `/proc/<pid>/stat`, so a reused pid
can't inherit it), so the `NAME-clean`, `NAME` and `NAME-utils` steps of the
same attempt replay too (a replayed `--clean` does not wipe the restored
objects), and nothing leaks into a later attempt.

- The recorded output of each step is replayed with its original relative
  timing, compressed so the whole build takes about
  `CACTUP_TUTORIAL_BUILD_SECONDS` (default 30). The bake's attempt directory is
  rewritten to this attempt's in the replayed text, since Cactus echoes the
  `options=`/`THORNLIST=` paths.
- The config step starts the copy into a staging directory and leaves it
  running. Its standard streams go to a log in the staging directory, never
  to the build's output (cactup notices a process still holding its output
  pipe and says so). It watches the build-script shell's pid and start time
  and, once that shell is gone, stops and removes the staging directory. The
  replay's pacing waits on the copy's progress, so output never stalls into
  an apparent hang, however slow the disk. Only the `NAME` step swaps entries
  in, after the copy has finished.
- If the copy fails (most likely a full disk), the replay stops, the step
  exits non-zero with a message naming the cause, and the staging directory is
  removed. Nothing was swapped, so the marker and the tree are as they were;
  cactup reports a failed build and the next attempt starts over.
- A Ctrl-C during a replay stops it at once and exits 130 with make's own
  interrupt message.

**The restore** never corrupts cactup's state:

- It writes only the entries the bake contains, and the harvest never takes a
  dot-entry or a `cactup-*` file, so cactup's own files in `configs/NAME/` are
  never touched: `.cactup-builds/` (the live attempt's `build.toml`,
  `build.out`, `heartbeat`), the per-config build lock `.cactup-build.lock`
  and its siblings, `cactup-config.toml`, and the `cactup-optionlist.*` and
  `cactup-thornlist*.th` snapshots. `cactup build list` shows only real
  attempts.
- A partial bake (B2–B5) also empties `build/` and `lib/`, so no objects from
  a real build survive beside the restored `config-data`. Otherwise a
  restore after a real build on that config (then `--clean`, or a deleted
  executable, which cactup rebuilds as full without a realclean) would leave
  older objects that the next real build silently recompiles in full.
- It swaps each top-level entry from the staging directory: rename the old
  one aside, rename the new one in, delete the old (a rename cannot replace a
  non-empty directory, and cactup pre-creates `build/`, `lib/`, `scratch/` and
  `config-data/`). An interrupt mid-swap leaves the marker *restoring*, so
  the next hit restores the whole tree again, and a staging directory the
  next run removes.
- `exe/cactus_NAME` is written to a temporary file and renamed over the old
  one, at the end of the `NAME` step's replay. It is never copied over in
  place: simulations hard-link the executable into their cache, and
  overwriting the shared inode would corrupt every earlier simulation's frozen
  executable.
- Restored mtimes keep their relative order: they are mapped monotonically
  into the interval from the attempt's start (or the newest source file, if
  later) to now, so they also look like the thirty-second build they
  replay. Every object ends
  up newer than the freshly fetched sources, without reordering objects and
  headers, so the first real incremental build compiles only what changed.

### Build inventory

Every `cactup build` that any cell or catch-up step runs, what cactup decides,
and what the shim does. The end-to-end test asserts that the shim's "not
precomputed" line never appears. Notebooks 6a and 6b build nothing (their
submits to `mylab` use `--ignore-machine` on the stock `tutorial`).

| Notebook | Command (install) | cactup decides | Shim |
|---|---|---|---|
| 2 | `build tutorial --thornlist tutorial.th` (stock) | new config | hit B1: restore + replay |
| catch-up N ≥ 5, late arrival | `build tutorial --thornlist tutorial.th` (stock) | new config | hit B1: restore + replay |
| catch-up N ≥ 5, after restoring edited files | the same | incremental | hit B1, built on: real, the restored files (all of a `.ccl`'s thorn) |
| catch-up N ≥ 5, otherwise | the same | up to date | no make at all |
| 3 | `build tutorial` (et-mp, before the refetch) | new config | hit B2a: restore + replay |
| 3 | `build tutorial` (et-mp, after the fork refetch) | full rebuild: the flesh moved | realclean real; hit B2b: restore + replay |
| 4b | `build tutorial-gpu --variant gpu --thornlist tutorial.th` | new config | hit B3 |
| 4b | `build tutorial-debug --debug --thornlist tutorial.th` | new config | hit B4 |
| 4b | `build tutorial-debug` again | up to date | no make at all |
| 5 | `build tutorial-pinned --universe pinned --thornlist tutorial.th`, before the hacking | new config | hit B5: restore + replay |
| 5 | `build tutorial` after a thorn source edit | incremental | miss: real, one thorn |
| 5 | `build tutorial` after a `.ccl` edit in a small test thorn | incremental, thorn shape changed | miss: real, that thorn from scratch |
| 5 | `build tutorial` after a flesh `.c` edit | incremental | miss: real |
| 5 | `build tutorial` after reverting the edits | incremental | hit B1, built on: real, the reverted files (all of the `.ccl`'s thorn) |
| 9 | `build tutorial` after a deliberate syntax error | incremental | miss: real, fails after CST and configure |
| 9 | `build tutorial` after the fix (not the original text) | incremental | miss: real, one file |
| 9 | `build tutorial` after reverting to the committed file | incremental | hit B1, built on: real, one file |

cactup writes `cactup-config.toml`, the `sources`/`thorn-shapes` tables and the
build attempt records itself, from the real repositories, so nothing cactup
reports is fabricated; only the compiler's work was done ahead of time. Two
traces are visible to anyone who looks: the Cactus banner's compile date (the
date the image was baked), and a full build that took about thirty seconds, in
the cell's output and in the build attempt's timestamps. The notebooks point
out neither.

### Bakes

`image/build.sh` runs the bakes in a **bake container** started from the
tutorial image with `docker run --hostname cactup-tutorial`, as user `cactus`
with `HOME=/home/cactus` and a bake-only home (never the skeleton, so the
skeleton's `database.json` has no ghost installations and no cached machine;
it does get the skeleton's `~/.cactup/cactupignore`).
The bake container is identical to an attendee's at run time — same paths,
hostname, machine, compilers, mirrors — which is what makes restored trees
valid. It installs from the mirrors, runs real `cactup build`s with the shim in
record mode, and harvests each result into `bakes/<fingerprint>/`, which the
final image copies to `/opt/cactup-bakes/`. Bakes are cached outside Docker's
layer cache, keyed by the fingerprint, so a rebuild only rebakes what changed.

| ID | Build | Kept |
|---|---|---|
| B1 | stock `ET_2026_05_v0` install, config `tutorial` from `tutorial.th` | full object tree (notebooks 5 and 9 rebuild incrementally) |
| B2a | install `et-mp` (from `tutorial.th`), config `tutorial` from its live thornlist, before the fork refetch | exe, `config-data`, `scratch/external` |
| B2b | install `et-mp` after the fork refetch, config `tutorial` | exe, `config-data`, `scratch/external` |
| B3 | `tutorial-gpu` (`gpu` optionlist variant) | exe, `config-data`, `scratch/external` |
| B4 | `tutorial-debug` (`--debug`) | exe, `config-data`, `scratch/external` |
| B5 | `tutorial-pinned` (`--universe pinned`) | exe, `config-data`, `scratch/external` |

Every bake also keeps the `NAME-utils` programs under `exe/NAME/`, since the
replayed output says they were built, and records the output of a
`NAME-clean` run on a throwaway copy of its tree, which is what a replayed
`--clean` step prints.

Notebook 4a installs `carpetx.th` (the CarpetX-only thornlist the mirrors
include) and `master` but does not build them. Notebook 3 already installed
from a thornlist file; 4a is about living with several installations:
`list`, `use`, `-I`, provenance with `inst show`, and `uninstall`. It builds
nothing, so those installs have no bakes.

**Bake cache keys** add the toolchain's identity to the fingerprint: a hash
of `dpkg-query -W` (every package and version in the bake container) and,
for B3, the CUDA version. apt itself is pinned to a `snapshot.debian.org`
date (a build argument), so rebuilding the image months later installs the
same packages and reuses the same bakes; moving the date is a deliberate
rebake. Without this, a point release of HDF5 or OpenMPI would leave objects
compiled against other headers in a tree whose later incremental builds link
them with new ones.

Every bake keeps `scratch/external` because executables link shared libraries
built there. `build.sh` checks with `ldd` that every baked executable resolves
every library, and only inside the image or its own kept tree. `ldd` does not
see what is loaded with `dlopen` (OpenMPI's components, HDF5 and ADIOS2
plugins, `libcuda`), but all of that comes from Debian packages or the GPU
driver, never from a bake.

The toolchain is trixie's: gcc 14 and OpenMPI 5.0. The optionlist uses
Debian's packaged libraries wherever they exist (OpenMPI, HDF5 1.14 with
OpenMPI, FFTW, GSL, hwloc, yaml-cpp, zlib, ADIOS2 2.10, Silo, BLAS/LAPACK), so
`scratch/external` holds only what Debian lacks. For `tutorial.th` that is
AMReX alone (the optionlist would also build openPMD-api and NSIMD, for
thornlists that use them). That keeps the trees, the image and the
per-attendee disk small; see sizing below.

B3 needs CUDA, which exists only in the bake container used for it: CUDA
13.x from NVIDIA's debian13 repository, which supports gcc 14 as nvcc's host
compiler. (Debian's own `nvidia-cuda-toolkit` 12.4 wants g++-13, mixing host
compilers, and NVIDIA's 12.x packages exist only in its debian12 repository,
whose signing key trixie's apt refuses.) The GPU executable links the CUDA
runtime statically (and whatever else AMReX pulls in, such as cuRAND, is
checked the same way), so it runs on a VM that passes an NVIDIA GPU through,
and is never run otherwise (notebook 4b shows how cactup refuses to put it on
a CPU queue, and explains `--force-queue` without running it).

Baked configs embed absolute paths, which is why every container runs as the
same user `cactus` with `HOME=/home/cactus`, whatever the attendee's hub
username is. The notebooks always pass explicit flags (`--silent`, no custom
prefixes) so no prompt answer can move an install away from the baked paths.

### Auto-update

The image builds cactup twice from this checkout as distribution builds: a
"previous" build and a "current" one. The updater orders builds by their
stamped date, so "previous" carries the stamp (id and date) of the commit
before "current"'s: distinct ids, strictly older date.

Both are published in one local update root, served by the update server the
entrypoint starts on `http://127.0.0.1:8765` before Jupyter. The installer and
the updater read different files there: `cactup-init.sh` installs the build
that `<target>/cactup.sha256` names, which is "previous", and cactup's update
check reads `latest.json`, which offers "current". cactup is **not
preinstalled**: attendees run the documented installer in notebook 1 (see its
constraint above), which installs "previous", and the home skeleton points the
`update-url` knob at the same root. The first cactup command after that
performs a genuine auto-update. Notebook 1 says openly that the VM has its own
update site, and pinning both builds this way means an upstream release can't
change the notebooks' output mid-workshop.

A failed update check still stamps `~/.cactup/update-check` for 24 hours, so:
the skeleton never contains the stamp, the entrypoint waits for the update
server's health check before starting Jupyter, and `cactup-tutorial-reset
--update-demo` re-arms the demo (points `bin/cactup` back at "previous" and
removes the stamp).

## The container

- Base image `debian:trixie`. The design spike ran jobs under trixie's Slurm
  24.11 with `CgroupPlugin=disabled` in an unprivileged (rootless Docker)
  container, and could not get Ubuntu 24.04's Slurm 23.11 to start there
  without a cgroup plugin.
- **CPUs.** Docker's `--cpus` is a CFS quota: `nproc`, hwloc, OpenMP and MPI
  would still see every host core, and `-j max` would oversubscribe. So the hub
  gives each container a `cpuset` of 4 cores (assigned round-robin in a
  `pre_spawn_hook`; containers share cores only when the VM has fewer than
  4 × attendees), and everything agrees on 4: the MDB entry's `[hardware]` is
  explicit (no autodetect), slurmd's node is declared `Sockets=1
  CoresPerSocket=4 ThreadsPerCore=1` with `RealMemory` from the memory limit
  (`SlurmdParameters=config_overrides`), and the runscripts bind no threads.
  The entrypoint exports the container's own CPU list (its allowed CPUs,
  from `/proc/self/status`) as `CACTUP_TUTORIAL_CPUS`, since absolute CPU
  numbers differ from container to container.
- **SLURM.** The entrypoint starts `munged`, `slurmctld` and `slurmd` as root
  (`proctrack/linuxproc`, `task/none`, no cgroups, `select/cons_tres` so jobs
  share the node), then drops to `cactus`. `StateSaveLocation` lives in a
  small volume of its own (not the home, which `cactup-tutorial-reset`
  wipes), so job ids keep counting up across container restarts and resets
  and cactup never mistakes a new job for an old one. Partitions:
  - `debug` (default, 30 min)
  - `short` (3 min, for walltime chaining in notebook 7)
  - `batch` (4 h)
  - `gpu` (usable only when the VM passes a GPU through: the entrypoint
    then declares the devices as SLURM GPUs; without one, a job asking for a
    GPU is refused)

  Jobs launch with OpenMPI's `mpirun --bind-to none` inside the allocation
  (OpenMPI binds by default; the container's cpuset already confines it),
  with `pml ob1`, the shared-memory BTL (named `sm` in OpenMPI 5), and a
  shared-memory size set on the container.

  When the container starts again after a stop (the idle culler, a
  re-created container on the same state volume), SLURM restores its queue;
  the entrypoint then takes the node down and up once, so jobs that were
  running when it stopped are requeued, as a cluster does after a node
  reboots, instead of appearing to run and holding the node; it also lifts
  the two-minute hold SLURM puts on requeued jobs, so they don't lose their
  turn to later ones. The simulation submit script appends to a job's output
  (the test one doesn't: every run of a test shares its output files), and
  the runscript prints SLURM's requeue count, so a simulation's log keeps
  the interrupted attempt. A notebook that shows `squeue` explains SLURM's
  alarming pending reason "Nodes required for job are DOWN, DRAINED or
  reserved for jobs in higher priority partitions": here it only means a job
  ahead of it, in another partition, has the node reserved.
- **Hostname** `cactup-tutorial`, set by DockerSpawner or `docker run
  --hostname`, so the MDB entry is discovered like any cluster's. The
  entrypoint refuses to start under any other hostname rather than letting
  cactup quietly create a machine for it and slurmd fail on the node name.
  (cactup also honors a `~/.hostname` file; catch-up removes one if an
  attendee creates it.)

## Sizing

Measured numbers go here as the bake stages land (per-config tree sizes, image
size, restore time). Until then, the estimates the design is checked against,
from the development machine: a full-ET config tree is 6–9 GB with
ExternalLibraries built from source, and an install's sources are about
1.8 GB (83 repositories). The subset thornlist and Debian's libraries are
expected to cut a config tree several-fold.

Per attendee, after every notebook: up to four installs' sources (stock,
`et-mp`, the `carpetx.th` install, `master`; about 1.8 GB each for the full release
and `master`, much less for `et-mp` and the CarpetX-only thornlist), B1's full
tree, B2–B5's partial trees and simulation output. Until the bakes are
measured, budget about 12 GB of disk per attendee, next to the 4 vCPU / 8 GB
of memory.

Acceptance targets, checked in the bake stage and again on the deployment VM:

- a replayed build takes 25–40 s, and its output never pauses for more than
  2 s, with as many restores running at once as the VM is sized for
  attendees (about 30), on the VM's disk class. If plain copies can't meet
  that, the bakes move to a reflink-capable filesystem (XFS with
  `reflink=1`) bind-mounted from the host into the same filesystem as the
  attendees' home volumes, so a restore is a clone. (Bakes inside the image
  can't be cloned into a volume: they sit on a different filesystem.)
- as many concurrent `cactup install ET_2026_05_v0` runs from the mirrors
  finish within 10 minutes each, inside notebook 1's 30; the subset installs
  (notebook 3's `et-mp`, 4a's named thornlist) within 3 minutes, and 4a's
  `master` within 10, inside 4a's 25;
- every "miss: real" build in the inventory (a one-file thorn edit, the small
  thorn's `.ccl` edit, the flesh `.c` edit, the reverts, notebook 9's broken
  and fixed builds, each of which re-runs `NAME-config`, Cactus's CST and
  configure) finishes within 2 minutes on 4 cores, and compiles only what the
  inventory says.

## Building

```sh
tutorial/image/build.sh                  # the image, tagged cactup-tutorial:lab
tutorial/image/build.sh --update-mirrors # the same, moving the mirrors to upstream's tips
```

The git mirrors live outside Docker, in `$CACTUP_TUTORIAL_MIRRORS` (default
`~/.cache/cactup-tutorial/mirrors`; about 3 GB, a quarter of an hour to fetch
the first time), and the image is built with that directory as its `mirrors`
build context. `build.sh` first brings them to the committed lock,
`tutorial/mirrors/mirrors.lock` (`mirror.py pin`: seconds when they already
are, a fetch only for commits they lack), so every build serves the same
commits. `--update-mirrors` instead moves them to upstream's current tips
(`mirror.py sync`) and rewrites the lock in the checkout; commit it, knowing
the bakes will be redone. The `mdb` branch in the image's mirror of Cactup is
always built from this checkout's `mdb/`, uncommitted edits included.

The base image is pinned by digest and the Debian archive by a
snapshot.debian.org date, both in the Dockerfile's base stage; move them
together, on purpose (it changes the toolchain, and so every bake).

The two cactup builds get their stamps from git: "current" is the last commit
that touched cactup's inputs, "previous" the one before, so a build needs two
such commits.

Checks:

```sh
PYTHONPATH=tutorial/jupyter python -m pytest tutorial/tests   # magics, lexers, session, mirrors
tutorial/tests/browser/run.sh cactup-tutorial:lab             # the frontend, in headless Chromium
tutorial/tests/platform/smoke.sh cactup-tutorial:lab          # install, update, SLURM, a simulation
```

To try the image locally as one attendee:

```sh
docker run --rm -p 8888:8888 --hostname cactup-tutorial \
    --cpuset-cpus 0-3 --memory 8g --shm-size 1g cactup-tutorial:lab
```

(Rootless Docker ignores `--cpuset-cpus` unless the cpuset cgroup controller
is delegated to the user's systemd slice; without it the container sees every
host CPU.)

## Deploying to a VM

(Filled in with the deployment stage: compose, choose-your-own-login, session
token, sizing.) Two things the deployment must provide, found while building
the platform: a named volume for `/var/spool/slurmctld` per attendee (SLURM's
state; without it a re-created container restarts job ids at 1, and cactup
could mistake a new job for an old simulation's), and a cpuset per container
that Docker actually applies (see the rootless note above). The idle culler
judges idleness by Jupyter activity only, so its timeout should be at least
the `batch` partition's limit (4 hours), or jobs still running are stopped
and requeued. Only a clean stop (`docker stop`, the hub's stop, the culler)
saves SLURM's state fully: a `docker kill`, `docker rm -f` or host crash can
lose submits from its last couple of seconds (and reuse their job ids), and a
kill in the first seconds of a start leaves a job it had requeued on SLURM's
two-minute hold. A job whose launch failed on a drained node ends up
"launch failed requeued held"; `scontrol release <job>` lets it run again.
After a restart, `scontrol show job` shows the jobs the entrypoint requeued
with a high priority (100000 and down) that keeps their turn; every other
job has priority 1.
