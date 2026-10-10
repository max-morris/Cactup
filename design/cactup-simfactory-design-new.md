# cactup: Subsuming SimFactory — Design Specification
<!-- spelling-check: skip -->

Status: draft for review
Audience: cactup implementers
Companion documents:
- `simfactory-docs.txt` — authoritative behavioral spec of simfactory2 (the system being replaced)
- `cactup-simfactory-design.txt` — the original cactup design notes (concepts, CLI sketch, MDB reorg)

This document is the consolidated, implementation-ready design for replacing
simfactory2 entirely with cactup. Where simfactory behavior is preserved, this
spec points at the relevant section of `simfactory-docs.txt` rather than
restating it. Where cactup diverges, the divergence and its rationale are stated
explicitly.

---

## 0. Decisions baked into this spec

These were settled during design review and are treated as fixed below.

| # | Question | Decision |
|---|----------|----------|
| D1 | Remote execution / source sync / `login` | **Dropped.** cactup is a local, per-machine tool. No `rsync` sync, no `--remote` SSH dispatch, no `login`, no trampoline/iomachine tunneling. You run cactup on the machine where the work happens. |
| D2 | Archive subsystem (petashare/uberftp) | **Dropped entirely.** No archive command, no drivers. |
| D3 | Cactus test-suite support | **Kept, as a first-class `cactup test run`/`test submit` command tree** separate from `sim` (§11). A testsuite runs against any built config — default the active config, or `--config C` — with **no separate test-config kind**. Test output lives under a dedicated, configurable **test-home** (`tests/`), *not* inside `simulations/`. Test run/submitscripts are marked `test = true` in the MDB and resolved by the §11.2 rules (this is the only test-marking; optionlists just use the §4.4 `default = true` picker). simfactory's overloading of `sim` (empty-parfile sentinel, monster `output-NNNN/exe/` copytree) is *not* ported. |
| D4 | Restart / chaining & on-disk metadata | **On-disk simulation output preserved** (numbered `output-%04d` restarts, the `output-NNNN-active` symlink, `CACHE/`, `TRASH/`). simfactory's `SIMFACTORY/` metadata dir and `properties.ini` are an **implementation detail and are NOT preserved** — cactup uses its own TOML metadata. Per-simulation state lives in the simulation's own folder; the global cactup database holds only global cactup state and the installation registry. Checkpoint recovery is **out of scope**: the parfile and Cactus own it end to end (§8.8). |
| D5 | Where simulations live | `<sim-home>/<config>/<SimName>/...`, where the per-alias `<sim-home>` = `<machine simulation-home>/<alias>` (falling back to `~/.cactup/simulations/<alias>` when the machine omits `simulation-home`). The chosen sim-home is fixed at install time and recorded per-installation; a single simulation's directory may be overridden at create time with `--sim-dir` (see §8.1). There is no `--basedir` flag. |
| D6 | Config-level metadata storage | Per-installation **on-disk TOML**, not the global DB (see §7.4). |
| D7 | `@VAR@` substitution engine fidelity | **Literal `@NAME@` replacement plus two computed token families, `@ENV(…)@` and `@KNOB(…)@`**, each in a required form (unset or empty = hard error) and two `-OPTIONAL` forms (empty, or a quoted/bare default) — see §6.1. Comments (as the target file kind spells them) are copied through untouched. `ENV` reads the named environment variable at substitution time; `KNOB` reads the knob snapshot frozen with the variable set (§5). Everywhere (TOML and shell templates, parfiles). simfactory's `@(expr)@` Python-eval and ternary/word-operator sugar are **not** ported. Scripts and parfiles needing further logic use the Python `.py` variant escape hatch (see §6). |
| D8 | Machine-level thorn enable/disable toggles | **Kept** (see §7.5). |
| D9 | Optionlist on-disk format | **TOML + render step.** Optionlists are authored as TOML; cactup renders them to the native Cactus `NAME = value` optionlist before `make`. Render rules, the `VERSION` semantics, and ordering are specified in §7.8. |
| D10 | Pre-existing simfactory simulation dirs | **Greenfield / ignore.** cactup manages only simulations it created. A directory is recognized as a cactup simulation **iff** it contains `.cactup/simulation.toml`. cactup neither reads nor migrates legacy `SIMFACTORY/` simulations. |
| D11 | Global-DB locking | **Brief lock around DB access only.** The exclusive lock is held only while reading/mutating/persisting the database — never across a compile or a simulation run. Per-simulation coordination uses the simulation's own on-disk state, not the global lock (see §2.3). |
| D12 | OptionList-variant ↔ queue compatibility | **The optionlist variant declares its compatible queues** (and therefore which run/submit variants it can pair with). `sim submit`/`sim run` enforce it (see §4.4, §7.4). |
| D13 | Linking & system libraries | **Fully static MUSL binary.** cactup deploys to clusters as a single copyable binary: the release artifact targets `x86_64-unknown-linux-musl` and must stay fully statically linked (`ldd`: "statically linked"). No crate that binds a system shared library (no openssl/native-tls — reqwest uses rustls; no libgit2 — git is pure-Rust gix; no pkg-config'd C deps); C code a dependency compiles in statically at cargo-build time is fine. Our own code never uses the `libc` crate directly — OS facts come from `/proc` or std (e.g. §2.3's pid probing). The one thing neither offers is sending a signal to another process and waiting on it by pid, which the build cache's compiler wrapper must do (§18.4); it uses `rustix` for exactly `kill(2)` and `waitpid(2)`. `rustix` makes Linux system calls itself, without libc, so the binary stays static. |
| D14 | Distribution, self-update & MDB generations | **CI builds, publishes and deploys; installed binaries keep themselves current.** Every passing `master` push builds static musl binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` (reused unchanged when no build input changed), publishes `mdb/` as the `mdb` git branch, and deploys docs + binaries + `cactup-init.sh` + `latest.json` to GitHub Pages. A binary is a **dist** build iff CI stamped it (`CACTUP_DIST=1`, build id, date); any local cargo build is a **dev** build (repo `mdb/`, no sync, no self-update). Dist builds clone the MDB into `~/.cactup/mdb`, sync the newest commit of their own **MDB generation** (warning loudly when a newer generation exists), and update themselves per the `autoupdate` knob (`auto`/`notify`/`off`). Each build lives at `~/.cactup/bin/cactup-<build>` behind a `bin/cactup` symlink and substitutes that path for `@CACTUP@`, so a job runs the exact build it was submitted with. The published MDB is the one on-disk artifact with a compatibility promise, and the generation is its only mechanism (§17). |
| D15 | Shared build cache | **A compiler wrapper inside cactup, over an instance-wide store.** Objects one installation or configuration of an instance built are reused by the others whenever, and only when, compiling afresh would produce the same bytes. No existing compiler cache fits (none handles Fortran module files, and none shares a directory between hosts over NFS without POSIX locks), so cactup is its own: it stands in front of Cactus's object compiles, injected by a makefile fragment that leaves the optionlist and `config-data` untouched. The cache may only ever cost a miss: anything it does not fully understand compiles as if it were not there. Objects will be keyed by the cactup machine and by the host that compiles them, so that one filesystem spanning several architectures never mixes them, and nothing will be evicted automatically. Being brought up in stages (§18); so far only the interposition exists: nothing is keyed, stored or served. |

Everything marked **ASSUMPTION** in this document is a smaller decision made to
keep the spec complete; flag any you want changed.

---

## 1. Overview & goals

cactup is a single, global command-line tool installed once per machine. It
manages multiple independent Cactus installations, builds configurations,
creates and runs simulations, and handles the cluster-specific drudgery
(scheduler interaction, templated submit/run scripts, machine detection) that
simfactory used to handle — but with a cleaner CLI and a global multi-install
model instead of one embedded simfactory per Cactus tree.

Hard goals:
1. **Preserve the on-disk simulation-output contract** so existing analysis
   tools that read simulation directories keep working (§9 is the binding
   contract).
2. Subsume every *user-facing* capability of simfactory that survives the §0
   decisions: build configs, create/run/submit/manage simulations, restart
   chaining, test suites, scheduler abstraction. (Checkpoint recovery is
   deliberately *not* on this list — it belongs to the parfile and Cactus, §8.8.)
3. Replace simfactory's embedded-per-tree model with one global tool managing
   N installations, each with M configs, each producing simulations.

Non-goals (dropped per §0): remote/SSH execution, source-tree sync, archiving.

### 1.1 Terminology (from `cactup-simfactory-design.txt`)

- **Cactus installation** — a self-contained Cactus tree. Identified by a unique
  **alias**. Typically lives at `~/.cactup/cacti/<alias>/Cactus`; the
  "installation directory" is one level above the Cactus root. `Cactus` is the
  ordinary case, not a fixed name: it is the thornlist's `!DEFINE ROOT`
  (`root-dir`, recorded once at install time — §8.1), which must name a real
  subdirectory of the installation directory — a `!DEFINE ROOT` that is
  absent, `.`, absolute, or contains `..` is a hard error at install.
- **Active installation** — the default target for installation-local commands.
- **config** — a particular build of Cactus within an installation (compile
  flags + thornlist). Each installation has an **active config**.
- **simulation** — a run of a config with a given parfile; owns its output
  directory and supports checkpoint/restart.
- **test run** (**test-sim**) — one execution of a config's testsuite (`make
  <config>-testsuite`); the simplified, one-shot analog of a simulation,
  living under **test-home** (§11.5). Runs against any built config (default the
  active config); there is no separate test-config kind (§11.1).
- **MDB (machine database)** — per-cluster scripts and metadata.
- **knob** — a global default value (allocation, email, queue, …).
- **task** — one MPI rank / process. Always. (`--tasks`, `TASKS`,
  `TASKS_PER_NODE` count ranks.)
- **CPU** — one core / hardware thread. Always. (`--cpus`, `CPUS_PER_TASK`,
  `MAX_CPUS_PER_NODE` count cores.) A task uses `CPUS_PER_TASK` CPUs.
- **availability vs. request** — hardware facts a node/queue *has* (the `max-`
  `[hardware]` keys, e.g. `MAX_CPUS_PER_NODE`) are distinct from what a job
  *asks for* (the §8.5 topology flags). The fill-the-node defaults bridge the
  two: a request left unset is filled from availability.

---

## 2. Process model & global state

### 2.1 The global database (`~/.cactup/database.json`)

Already implemented (`src/database.rs`). It is the **only** global mutable state
and is concerned **exclusively** with global cactup state:

- `cactup-version`
- `installations`: alias → `{ alias, release, path, thornlist?,
  current-release?, current-thornlist?, unfetched-repos? }`. `release` is
  `null` for a **custom installation** (`install --thornlist`), in which case
  `thornlist` records the absolute path it was installed from — the only thing
  that identifies such an installation, and what `show`/`list` name in place of
  a release. Omitted entirely for release installs.
  `release`/`thornlist` are **install-time provenance and are never rewritten**.
  `installation refetch --release TAG` / `refetch THORNLIST` instead record the
  optional `current-release` / `current-thornlist` keys (absent until the first
  explicit-source refetch; serde `default` + `skip_serializing_if`, so no
  `schema` bump), and `list`/`show` render both, e.g. "ET_2026_05, now on
  ET_2026_11". They are recorded whenever the refetched thornlist is adopted
  on disk, even if some repos were skipped as dirty or failed — the
  installation now tracks that list as authoritatively as the fetch allowed.
  `unfetched-repos` (optional, same serde `default` + `skip_serializing_if`,
  so no `schema` bump) is a map of repo name → `{ reason, thorns, detail? }`,
  recording exactly which repos the refetch did **not** fetch and why:
  `reason` is `skipped` (dirty local state, preserved on purpose — a
  supported workflow) or `failed` (an error — the user asked for the fetch
  and did not get it), `thorns` the thorns that repo backs (a failed
  download/external/symlink component backs itself, so counts stay
  consistent), and `detail` the dirty-state description or error text.
  Non-empty means the tree only **partially** conforms to the recorded
  thornlist — those thorns on disk still hold their previous contents, which
  `show`/`list`/`config show` report as a conformance caveat, failures
  angrier than skips (§3.2). It is cleared by any refetch that fetches every
  repo the thornlist names — the DB must not assert a tree state that does
  not exist; it now records the shortfall explicitly rather than withholding
  the provenance.
- `active-installation`
- **knobs** (new; see §5) — global defaults, one flat map (a `~/.cactup` lives
  on exactly one machine).
- **`detected`** (new; see §4.3) — the discovered machine, stamped with the
  hostname and login session that last verified it:
  `{ "name": "mike", "hostname": "mike1.hpc.lsu.edu", "session": "19840:590094" }`.
  One record, not one per hostname: a `~/.cactup` logically lives on one
  machine, and login and compute nodes of a cluster share it — but a
  `~/.cactup` on a filesystem shared between clusters does not get to carry
  one cluster's machine onto another unnoticed, because the stamp is what
  decides whether the name is trusted or re-verified.

**Version guard & on-disk schema policy (backward compatibility
required).** On-disk state is versioned by an integer **`schema`** (the DB and
every cactup TOML carry one; §9.3), *separate* from the human `cactup-version`
string. The binding rule is: **a newer cactup binary MUST be able to read any
older `schema` it ever shipped** — reads are backward-compatible by contract, so
a cactup upgrade never orphans existing installations, configs, or in-flight
simulations/chains (this is what makes the compute-node `sim run --restart-id`
safe across an upgrade). cactup refuses only when the on-disk `schema` is
**newer** than the running binary understands (a genuinely forward-incompatible
read), with guidance to upgrade. The `schema` integer is bumped **only** on a
breaking on-disk change; during active prototyping it stays fixed, so iterating
on the code needs no migration and no data churn. In-place *up-migration* of old
schemas to the newest is still out of scope for v1 (the requirement is that new
binaries *read* old schemas, not that they rewrite them); when a breaking bump
finally happens, a migration story is designed then.

The database does **NOT** store config metadata or simulation state. Those live
on disk next to the things they describe (D4, D6). Rationale: a simulation
directory is portable and self-describing; analysis happens long after the run;
the global DB must not become a single point of failure for per-sim state.

### 2.2 Path constants

| Constant | Value |
|----------|-------|
| `CACTUP_ROOT` | `$CACTUP_HOME` when set (non-empty, absolute), else `~/.cactup` (D14; the installer honors the same variable) |
| Installations root (default) | `<install-home>/<alias>/`, where `install-home` defaults to `~/.cactup/cacti` when the machine omits it (§4.2) |
| System MDB (production = dist build) | `~/.cactup/mdb/gen-<N>` → `<sha>/`: a per-generation symlink to a tree exported from `~/.cactup/mdb/repo/` (a bare fetch-only clone of the `mdb` branch); **read-only** to cactup, swapped atomically on sync (D14, §17.4) |
| System MDB (development = dev build) | `<project root>/mdb` (`CARGO_MANIFEST_DIR` at compile time) |
| Built-in `generic` | `~/.cactup/mdb-builtin/<hash>/generic` (extracted from the binary, keyed by a content hash of `mdb/generic`; used only when the system MDB lacks `generic/`) |
| cactup binaries | `~/.cactup/bin/cactup` → `cactup-<build>` (symlink to the current versioned build; retired builds are removed only by `cactup update --prune` — §17.2) |
| **User MDB (writable overlay)** | `~/.cactup/machines/` (user-created/customized machines; never touched by MDB updates) |
| Database | `~/.cactup/database.json` |

~~**ASSUMPTION:**~~ **Resolved by D14 (§17).** MDB source resolution mirrors the manifest repo handling already
in `src/manifest.rs`: in production cactup clones/updates the MDB git repo into
`~/.cactup/mdb`; in development a compile-time-selected constant points at the
in-repo `mdb/`. A `--mdb-path` global flag overrides for testing. "Production"
vs "development" is the CI build stamp (dist vs dev), not the cargo profile.

### 2.3 Locking & long-running commands (D11)

**The existing `Database::load()` holds the exclusive lock for the entire
process lifetime; this must change.** cactup now owns commands that run for
minutes-to-days — `cactup build` (compiles Cactus) and `sim run` (runs the
simulation in the foreground) — and the generated submit script re-invokes
`cactup sim run` on the **compute node** (§8.3). A machine-wide, whole-command
lock would (a) serialize all cactup activity on the machine, and (b) let a
login-node invocation collide with a compute-node `cactup sim run`, causing the
queued job to die on `try_lock`.

**Single-instance intent, best-effort safety.** The design assumes a user runs
**one** cactup instance at a time for a given `~/.cactup`; the locking below is a
best-effort guard to keep a careless second invocation (or a login/compute-node
overlap) from corrupting the database or per-sim state, not a full concurrent
multi-writer story.

Required model (D11):

1. The global DB lock is acquired, the DB is read (and possibly mutated +
   persisted), and the lock is **released before** any long-running work begins.
   A command that needs to write a result at the end **re-acquires the lock,
   re-reads the on-disk DB, and applies a field-scoped write** (mutating only the
   specific keys it owns — e.g. one `installations` entry or one `knobs` entry —
   then persisting), rather than serializing a stale in-memory snapshot over the
   whole file. This prevents a long-running command from clobbering an unrelated
   change another invocation made while it worked. Field-scoped writes are
   the reason the persist path must re-read first: the whole-file
   `serde_json::to_string_pretty(self)` in `src/database.rs` is replaced by
   read-modify-write under the held lock.
2. **`cactup sim run --restart-id N` (the compute-node path) does not depend on
   the global DB at all.** Everything it needs — the Cactus root, the
   executable, the config, the parfile, topology — is read from
   the simulation's own on-disk metadata (§9.3) and the explicit flags the
   submit script passes (§8.3). This decouples compute-node execution from
   login-node state and from the lock.
3. Per-simulation mutual exclusion (two `submit`s racing the same simulation)
   uses a lock file inside the simulation's `.cactup/` dir, not the global lock,
   so unrelated simulations never contend.
4. **Per-config build lock.** `cactup build` releases the global lock before
   invoking `make`, so two `cactup build <name>` for the *same* config in one
   installation could otherwise both write `configs/<name>/` and corrupt it. A
   lock file at `configs/<name>/.cactup-build.lock` serializes builds of the same
   config; builds of *different* configs never contend.
5. **Per-installation lock.** The per-installation on-disk TOML files
   (`installation.toml` — active config, sim-home; and `simulations.toml` — the
   name→dir registry, §8.1) are mutated by `sim create`, `sim delete`,
   `config use`, and `config delete`. A `link()`-based lock file at
   `<installation home>/.cactup/.cactup-install.lock` serializes those mutations
   so two commands in the same installation cannot race the registry or the
   active-config pointer; different installations never contend. These
   per-installation writes are **not** covered by the global DB lock (the global
   DB does not hold per-installation state — D6).
6. **Per-installation fetch lock.** `install`'s checkout and `installation
   refetch` mutate `<root-dir>/repos/` and the arrangement symlinks for
   minutes-to-an-hour. That is *not* a mutation of the item-5 TOML files, so it
   does not hold the per-installation lock (which would block `sim create`
   etc. for the whole fetch, against this section's premise). Instead a
   `link()`-based lock at `<installation home>/.cactup/.cactup-fetch.lock`,
   held **with a heartbeat** (like the item-4 build lock) for the duration of
   the fetch, serializes concurrent fetches of one installation. If a refetch
   ultimately changes a TOML or the DB, it acquires those locks briefly at the
   end, in the item-1 field-scoped style.

**NFS-safe locking (required).** `~/.cactup` and the sim-home are frequently on
NFS/Lustre, where `flock`/POSIX advisory locks are unreliable or silently a
no-op. All cactup locks — the global DB lock, the per-simulation lock, the
per-config build lock, and the per-installation lock — therefore use the
**`link()`-based** protocol (create a unique temp file, `hard-link` it to the
canonical lock path; the link succeeding is the atomic acquire; unlink on
release), which is atomic on POSIX including over NFS. cactup does not rely on
`flock` for correctness on shared filesystems.

**Stale-lock detection & cross-host liveness.** Each lock file records
the holder's `hostname` **and** `pid` and is (re-)stamped by touching its mtime.
Reclaiming a lock:

- **Same host** (recorded `hostname` == ours): the holder is alive iff `kill(pid,
  0)` succeeds; a dead pid means the lock is stale and may be broken immediately.
- **Different host** (the login-node-vs-compute-node case — you cannot probe a
  remote pid): liveness falls back **solely** to the mtime timeout. A lock whose
  mtime has not advanced for **`LOCK_STALE_SECS = 900`** (15 min) is treated as
  stale and broken. A live holder must re-stamp its lock mtime well within that
  window (see heartbeat cadence below). This is best-effort and assumes NFS
  clock skew between a cluster's nodes is bounded (minutes, not hours); cactup
  compares against the *file's* mtime as seen on the shared filesystem rather
  than mixing in local wall-clock, which keeps the comparison on one clock
  domain.

**Concrete liveness constants.** A live `sim run` (§8.4) touches its restart
`heartbeat` file and re-stamps any lock it holds every **`HEARTBEAT_SECS = 60`**;
the stale-restart reaper (§8.3) considers a restart's heartbeat stale only after
**`HEARTBEAT_STALE_SECS = 300`** (5× the cadence, to tolerate a missed touch or
NFS mtime lag) *and* only when live job status is `U` *and* the `running.lock`
is unheld — all three must agree. `LOCK_STALE_SECS` (900) is deliberately
larger than `HEARTBEAT_STALE_SECS` (300) so lock-breaking is strictly more
conservative than restart-reaping. These constants live in one place in the code
and are documented here as the source of truth.

**Persistence on exit.** The signal-/drop-based whole-file persistence in
`src/database.rs` is **removed for the long-running commands**, because under the
early-release model the process no longer holds the lock at exit and its
in-memory DB is stale — a drop/signal `persist()` would overwrite whoever holds
the lock now. Instead: every DB mutation is completed as a self-contained
locked read-modify-write (item 1) *before* the long-running work starts, so there
is no pending in-memory state to flush at exit. The `Drop`/signal hook is kept
only for the short, DB-only commands that legitimately hold the lock for their
whole (brief) lifetime, and it persists **only while the lock is still held**;
otherwise it is a no-op.

### 2.4 Interrupts & progress (required for every long-running path)

**Interrupt contract.** `main.rs` installs the process signal handler with a
grace count of 1: the **first** Ctrl-C prints a notice ("stopping…, Ctrl-C
again aborts instantly") and sets the global interrupt flag
(`gix::interrupt::is_triggered()`); the **second** aborts on the spot. The
notice is a promise, so every long-running path — any loop over
repos/thorns/files, child-process wait, network operation, or polling loop —
**must poll the flag** at per-item/per-tick granularity and stop within a
fraction of a second:

- Fail with an explicit "interrupted" error rather than returning partial
  results shaped like success. Where results are per-item reports
  (`fetch::execute`), unstarted items are recorded as *failures*, never
  silently dropped — an interrupted run must not masquerade as complete.
- `par::parallel_map` implements the contract for fan-out work; prefer it.
- Foreground children receive the terminal's SIGINT themselves; wait ~2 s,
  then kill (`sim::start::spawn_and_wait`). gix APIs take
  `&gix::interrupt::IS_INTERRUPTED` and abort in-flight work internally.
- Graceful wind-down is also what releases §2.3's locks and cleans
  tempfiles; an abort leaves lock corpses that block other hosts for up to
  `LOCK_STALE_SECS`.

**Progress contract.** No silent multi-second phases — a quiet pause reads
as a hang. Any phase that can plausibly exceed ~1 s on a real tree (~80
repos / ~400 thorns on NFS) renders prodash progress: a phase-scoped
renderer (`manifest::setup_prodash*`, shut down before normal printing
resumes), an item init'ed with a count and unit, a short-lived child naming
each in-flight unit, `inc()` per completion. The renderer's 500 ms initial
delay means fast runs never flash a bar, so "usually quick" is not a reason
to skip it. `setup_prodash_if_tty` is only for phases whose items never call
`info()`/`fail()` — those messages are printed by the render thread even on
a non-tty, and skipping the renderer would drop them.

One line per unit of work, and no nested bars. gix reports a whole progress
subtree — a status line it renames per fetch phase, a bar per phase, and
per-thread delta-resolution workers under those — and narrates throughput as
`info` messages; drawn verbatim, four concurrent component fetches are a
dozen flickering bars and a wall of `done 12.3MB in 1.2s` lines. Anything
handed to gix is therefore wrapped in a `progress::Line`, which collapses
that subtree onto its one item: labeled with the phase running now, filled
by whichever phase is actually *moving* (the server's "Compressing objects"
until the pack starts, then the pack's bytes, then the checkout's files),
and silent about gix's own chatter. The renderer's level filter stops at the
wrapped item. Every line is `<name>  <phase> <numbers>` anchored left, then
a bar of one constant width anchored right. prodash cannot lay a line out
that way on its own — it right-aligns each line's numbers against the widest
line drawn and gives the bar whatever is left, so both drift as the phases
change — so one `progress::Layout`, built per renderer from every name the
batch will carry, composes the numbers into the item's name, padded or
clipped to one text column, and gives the item a unit that prints nothing:
prodash is left drawing the bar alone, from the item's own step and bound.
The layout sizes the columns to the terminal (a third for the bar), fixes
the width of the name column across all of its lines (the headline and the
scrollback included), and renders the component's name louder than the
phase it is in, since prodash paints a task's whole name in one style and
the only way to split the two is to end that style inside the string. A
layout's clock rewrites every live line's numbers ten times a second, which
is what keeps gix's checkout current — it counts through the counter it
takes, never through the handle. Lines under a headline hang from it by
`├─`, the last one by `└─`: the headline numbers its children so the corner
lands on the line prodash draws last, and it is passed on the moment that
line finishes. Work that counts for itself with no phases — a plain download
— uses `Line::counting` instead. Every finished unit of work leaves exactly
one line in the scrollback: `succeeded` (green), `warned` (yellow: it
changed something the user did not ask for, like overwriting a dirty repo or
re-pointing an `origin`) or `failed` (red). Work that did nothing — a repo
already at the wanted commit — leaves nothing; the command's summary reports
those counts.

**§-reference hygiene.** Annotate code with spec section references (`§8.5`)
liberally — they are how contributors (human or agent) jump from a feature to
its contract here. But they are internal navigation, never user-facing: no
§-refs in anything cactup shows or emits — clap help (in `src/args.rs`, every
`///` doc comment and `help =`/`about =` string IS help output), `bail!`/
`anyhow!`/`println!` message strings, or generated file content (e.g. the
`.py` preamble). Put the reference in a plain `//` comment adjacent to the
string instead — trailing on the line, or just above the statement. Test
assertion messages, `#[cfg(test)]` fixtures, and comments in `mdb/` source
files are internal and may carry §-refs freely.

---

## 3. CLI surface

Top-level (clap, mirroring `src/args.rs` style). Global flags: `-v/--verbose`,
`--manifest-url`, `--mdb-path` (new), `--machine <name>` (new; overrides
discovery — §4.3), `--installation <alias>` (new; target a non-active
installation for one command instead of `cactup use`-ing it first), and
`-K/--knob NAME=VALUE` (new, repeatable; overlay a knob for this one command —
§5.1). `-K` also accepts the two-token spelling `-K NAME VALUE`: a pre-pass
over argv folds it into `NAME=VALUE` before clap parses (clap's own variable
arity would swallow the subcommand name as the value), stopping at `--` and
never pairing a flag-like next token.

**`-f/--force` is per-command, not global.** The existing global `force` flag in
`src/args.rs` is removed in favor of per-subcommand `-f`, because its meaning
differs by command (overwrite an existing config/sim, force-stop a running job,
permanently delete instead of trash). Each use is defined at its command below.
Where a single command has more than one thing to force, `-f` is a **generic
"bypass all nagging"** umbrella that implies the specific flags; the specific
flags exist for callers who want to force exactly one thing. On `sim submit`/`sim
run`: `--overwrite` (replace an existing sim on implicit create) and
`--force-queue` (bypass the optionlist↔queue compatibility check, §4.4) are the
two specific flags, and `-f` implies both.

There is no `--basedir` flag. A simulation's directory is fixed at create time
(defaulting under the installation's sim-home) and may be overridden only then,
with `sim create --sim-dir <path>` (§8.1, §8.2). Later `sim` subcommands locate
the simulation by name via the per-installation registry (§8.1), so they need no
path flag.

```
cactup install [release] [--alias …] [--silent] [--install-prefix …] …   (existing)
cactup uninstall <alias> [-f]                                             (new; see §3.1)
cactup show                        (aggregate state view: active installation +
                                    active config + resolved machine; read-only,
                                    never prompts, never writes the DB)

cactup installation list [--all]                  (list installations)
cactup installation show [<alias>]                (details of one installation)
cactup installation use <alias>                   (set active installation)
cactup installation refetch [THORNLIST | --release TAG] [-f]
       [--overwrite-modified] [--overwrite NAMES] [--replace-thornlist]
       [--prune] [-s|--silent] [-n|--dry-run]     (re-run the component fetch —
                                                   see §3.2)
cactup installation delta [<alias>]               (how the source trees diverged
                                                   from the last fetch — §3.3)
cactup inst …                      (short form of `installation`, a duplicate
                                    clap variant routed identically, so the docs
                                    generator renders both)
cactup list [--all]                (top-level equivalent of `installation list`)
cactup use <alias>                 (top-level equivalent of `installation use`)

cactup build [<name>] [-f] [--thornlist P] [--variant V] [--universe U | --no-universe] [BUILD FLAGS] [BUILD TOPOLOGY]
                                    (auto-selects foreground vs. queued per the
                                    machine's MDB entry — §7.9)
cactup build run    [<name>] …    [--config-dir P --attempt-id N]
                                    (force foreground build; the flagged form is
                                    the compute-node re-invocation — §7.9.1)
cactup build submit [<name>] … [--follow | --block]
                                    (force a queued build — §7.9)
cactup build list   [--long] [--all]
cactup build show   [<name>] [--long]
cactup build log    [<name>] [-f|--follow] [-o|--follow-out] [-e|--follow-err]
cactup build stop   [<name>] [-f]
cactup build prune  [<name>] [--keep N]
                                    (`build run`/`submit`/`list`/`show`/`log`/
                                    `stop`/`prune` monitor and manage a build
                                    attempt exactly as the `sim` equivalents
                                    monitor a simulation — §7.9. `<name>`
                                    defaults to the active config everywhere,
                                    like `config show`/`config delta`.
                                    **`cactup config build` no longer exists** —
                                    `cactup build` is the only spelling.)
cactup config show [<name>]
cactup config use <name>
cactup config delete <name>
cactup config delta [<name>]         (how the sources diverged from the last
                                      build of this config — §3.3)

cactup sim create [-f] <sim> <parfile> [--config C] [--sim-dir P]
cactup sim submit [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <sim> [<parfile> --config C] <TOPOLOGY…> [--checkpt-buffer W]
cactup sim run    [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <sim> [<parfile> --config C] <TOPOLOGY…> [--debug] [--checkpt-buffer W] [--restart-id N --sim-dir P]
cactup sim stop   <sim> [-f]
cactup sim clean <sim>
cactup sim delete <sim> [-f]
cactup sim show [<sim>] [--long] [--all]           (--all: across every installation — §8.1)
cactup sim output-dir <sim> [--restart-id N]      (prints active/Nth restart dir)
cactup sim log <sim>                              (tail stdout/err / formaline)

cactup test run    [--config C] [--variant V] [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <TOPOLOGY…> [<test>…]   (§11)
cactup test submit [--config C] [--variant V] [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <TOPOLOGY…> [<test>…]
cactup test list       [--long] [--all]
cactup test show       <name>
cactup test stop       <name> [-f]
cactup test delete     <name> [-f] [--purge]

cactup knob [<name> [<value>]]                    (§5)

cactup update [--check] [--prune]                 (D14, §17.3: install the newest
                                                   build, then force an MDB sync;
                                                   --check compares and modifies
                                                   nothing; --prune also removes
                                                   builds retired > 30 days, §17.2;
                                                   dev builds refuse)

cactup machine show [<name>]                      (replaces print-mdb / list-machines)
cactup machine whoami                             (which machine am I on)
cactup machine create [<name>] [--from-existing [B]] [--silent] [--no-discover]   (§4.7)
cactup machine delete <name>                      (§4.7; user-MDB machines only)
cactup machine forget                             (clear the cached detected machine — §4.3)
```

Most simulation/config commands operate on the **active installation** and its
**active config** unless overridden. `config`/`sim` commands fail fast if no
installation is active.

### 3.1 Command mapping from simfactory

| simfactory command | cactup equivalent | Notes |
|--------------------|-------------------|-------|
| `sim build` | `cactup build` (foreground or queued, auto-selected — §7.9) | §7, §7.9 |
| `sim create` | `cactup sim create` | §8.2 |
| `sim submit` | `cactup sim submit` | §8.3 |
| `sim run` / `run-debug` | `cactup sim run [--debug]` | §8.4 |
| `sim create-submit` / `create-run` | `cactup sim submit/run <sim> <parfile> …` | implicit create (§8.3) |
| `sim stop` | `cactup sim stop` | §8.6 |
| `sim cleanup` | `cactup sim clean` | §8.6 |
| `sim purge` | `cactup sim delete` | moves to `TRASH/` (§8.7) |
| `sim get-output-dir` | `cactup sim output-dir` | |
| `sim show-output` | `cactup sim log` | |
| `list-simulations` / `list-sim` | `cactup sim show` | |
| `list-configurations` / `list-conf` | `cactup config show` | |
| `sim create --testsuite` / `--select-tests` / test-suite run | `cactup test run`/`test submit [--config C] [<test>…]` | Own command tree, not a `sim` flag (§11). Runs any built config (default active); selection is the positional `[<test>…]` (default all); no empty-parfile sentinel. |
| `list-machines` / `print-mdb*` / `whoami` | `cactup machine show` / `whoami` | §4 |
| `interactive` | *dropped* | **ASSUMPTION**: rarely used; reintroduce later if needed. |
| `sync`, `--remote`, `login`, `execute` | *dropped* (D1) | |
| `checkout` | `cactup installation refetch` | Native CRL fetcher (§3.2); the Perl `GetComponents` is no longer downloaded or invoked anywhere. `install` uses the same fetch path. |
| `get-archived-simulation`, `list-archived-simulations` | *dropped* (D2) | |
| `setup` / `setup-silent` | `cactup machine create [--silent]` (§4.7); `install` calls the silent form on an unrecognized host | Creates a local machine in the user MDB from `generic` with autodetected hardware. No per-tree `defs.local.ini`. |
| `remove-submitscript` | *dropped* | Submit/run scripts are resolved at submit time from the MDB, not baked into the config (§7.6), so there is nothing to remove. |

### 3.2 `installation refetch` and the native fetcher

cactup owns the component fetch natively (`src/thornlist.rs` CRL 1.0 parser +
`src/fetch/`): git via gix, http/https/ftp via reqwest, svn/cvs/hg/darcs by
invoking the system tool. The Perl `GetComponents` is neither downloaded nor
invoked; `install` and `installation refetch` share this one fetch path.

**Where the source tree lives.** The thornlist's `!DEFINE ROOT` names the
source-tree directory, and must name a real subdirectory of the installation
home: `install` validates the value before fetching anything, and a
`!DEFINE ROOT` that is absent, `.`, absolute, or contains `..` is a hard
error — the installation home holds `.cactup/` and the source tree side by
side, which is why the source tree cannot be the home itself (and why
escaping the home, via an absolute path or `..`, is likewise refused). The
resolved value is recorded **once, at install time**, as `root-dir` in
`installation.toml` (§8.1) and never re-derived — an installation's source
tree cannot move; the remedy for a thornlist that wants a different one is a
fresh install (and `installation refetch` runs the same validation, so a
thornlist that newly omits `ROOT` fails there too, before the recorded-value
comparison below). From here on this section writes `<root-dir>` for the
resolved directory (`Cactus` in the ordinary Einstein Toolkit case) and
`<installation home>` for the directory one level up. `install`'s optional
convenience symlink (`--symlink-prefix`/`--symlink-name`/`--no-symlink`)
follows suit: its default *name* is `<root-dir>`'s final component (every
valid `root-dir` has one).

Because `root-dir` cannot change after install, `installation refetch`
refuses — before touching anything, `--dry-run` included — a thornlist whose
`!DEFINE ROOT` differs from the recorded value; the error names both and
points at a fresh install as the remedy.

Refetch decisions come from **live git state only** (per-repo probe), never a
diff against the previously recorded thornlist. Classification per repo:
absent → clone; clean on the thornlist branch → fetch + fast-forward; clean
with the desired branch absent locally → fetch + create + check out. Everything
else — modified/staged/deleted tracked files, local commits, detached HEAD or
mid-rebase/merge, HEAD on another branch, changed remote URL, or a failed
probe — is **skipped and reported**, and fetched only under
`--overwrite-modified` (every skipped repo) or a targeted `--overwrite NAMES`
(just the named ones: a repo dir under `<root-dir>/repos/`, a full thorn
checkout, or a bare thorn name — case-insensitive, comma/space-separated,
repeatable).
Either way modified files are backed up first, to
`~/.cactup/refetch-backups/<alias>/<ts>/<repo>/`. Untracked files never block
a fetch but do block `--prune`.

The remote-URL comparison is canonical, not textual: scp-style
(`git@host:user/repo`), `ssh://`, `git://`, `https://`, `http://`, and
`file://`/local-path forms all normalize to the same key (userinfo, a numeric
port, host case, and a trailing `/` or `.git` are ignored); the path past the
host is still compared exactly, so a different owner is a genuinely different
repo. Forcing a repo skipped for a URL change — via `-f`/`--overwrite-modified`
or by naming it in `--overwrite` — rewrites its `origin` fetch URL to the
thornlist's URL, persisted to `.git/config` and always reported, before
fetching and checking out from it; this is the fork-adoption path (point the
thornlist at a fork of a component, then `refetch --overwrite <that repo>`).

**The arrangement link pass.** After every fetch, each git component's
`$TARGET/$CHECKOUT` is materialized as a **relative symlink** into
`<root-dir>/repos/<repo>[/<REPO_PATH>]`. This is what actually puts a thorn into
the build — a repo checkout on its own does nothing — so the links are as much
part of a conforming tree as the repos are. (Only git components get one:
downloads and external checkouts land straight under their `!TARGET`.)

cactup is deliberately stricter here than GetComponents' `ln -nsf`, which
overwrites whatever it can `unlink()` — any symlink, including a hand-made one
pointing somewhere unrelated — while its plain-checkout branches skip relinking
whenever *anything* already exists at the path, even a symlink resolving to the
wrong repo. cactup inspects instead: a correct link is left alone, a link into
`repos/` pointing at the wrong place is repointed, and anything else — a real
file or directory, or a symlink resolving outside `repos/` — is **never touched
and always reported**. Components whose target deliberately routes *through*
another component's arrangement symlink (the Einstein Toolkit list has one) are
linked last, so the path they resolve through already exists.

A fetch only reports what it encountered while running. `installation delta`
(§3.3) reports the standing state of these links at any later time, which is
where a link that was hand-replaced *after* the fetch shows up.

Flags follow the §3 umbrella rule: `-f` implies `--overwrite-modified`,
`--replace-thornlist`, and the prune confirmation, but not `--prune` itself
(a mode, not a nag); it supersedes any `--overwrite` selection (naming both
is not an error, just a note that `-f` already covers everything). An
`--overwrite` name matching nothing in the thornlist is a hard error, checked
before anything is fetched (so `-n` catches it too); a name matching a repo
that isn't skipped is not an error, just a note that there's nothing to
overwrite there. `-s/--silent` silences the skip-warning block only — it
never authorizes deletion. `-n/--dry-run` prints the full classification —
marking exactly the repos that would be forced under the given flags — and
touches nothing.

Reporting follows the decision, so the two can never disagree: the force
selection is resolved **before** anything about the plan is printed, and each
dirty repo is then announced exactly once, under the verdict this run will
actually apply to it. Repos being fetched over are reported as such (red,
"local state NOT preserved") and are never first announced as "skipped (local
state preserved)" and quietly overwritten later; the skipped group keeps the
yellow headline and is the only group offered the `-f`/`--overwrite` remedy,
which therefore always names a repo the remedy still applies to. `-n` marks
the same split with `FORCE:`/`SKIP:` labels rather than tagging a "SKIP:" line
with a contradicting "would be fetched" suffix.

**Selecting what to fetch: releases and `master`.** `install`'s positional
argument and `refetch --release` name a release tag in the manifest repo — or
the literal `master`, the tip of the manifest's master branch, which is newer
than every release. The manifest is fetched before either resolves, so
`master` always means the newest commit; it resolves through
`refs/remotes/origin/master`, never the local `master` that a clone leaves
frozen at clone time. `master` is never a default: `install` with no argument
— and the default its interactive prompt offers — is still the newest
release, and `cactup releases` lists tags only, closing with a line saying
master is there for the asking. The DB records the selector itself (the tag
name, or `master`); printed messages additionally name master's commit, since
`master` alone pins nothing.

A refetched thornlist (from `--release` or a positional `THORNLIST`) is
written verbatim to `<root-dir>/thornlists/installation-default.th` (the
**live, editable copy** — what a build reads by default) and
`<installation home>/installation-source.th` (the **pristine as-fetched
copy** — the source the installation was fetched from); divergence of the
live copy from the pristine one (compared as parsed component sets, not
text) means a hand edit → refetch refuses to replace it without
`--replace-thornlist`/`-f`. The DB records `current-release`/
`current-thornlist` (§2.1) without touching
install-time provenance, and does so on adoption even when repos were skipped
as dirty or failed; the shortfall is recorded in `unfetched-repos` (§2.1) and
reported by `show`, `list`, and `config show` alike, in two tones: **failed**
repos are an error (red, reported first, header word `INCOMPLETE`, remedy:
retry the refetch) while **skipped** repos are a supported choice (yellow,
header word `PARTIAL` when nothing failed, remedy: `refetch -f`/
`--overwrite-modified` if the skip was unintended). Refetch itself prints a
loud "Partial adoption" block with the same FAILED/skipped split whenever this
happens. A refetch does not rebuild configs; it warns per config (§7.4).

Both names are recent: before the rename both copies were called
`einsteintoolkit.th`, which said nothing about which was which and read as
"stock Einstein Toolkit" even when it held a custom list. An installation
created under the old scheme is migrated in place — both files renamed, and
any config that recorded the old live path retargeted — by the first `cactup`
command that resolves it; the migration is best-effort and idempotent, and a
read falls back to the old name if it hasn't run yet, so nothing breaks if it
can't.

### 3.3 `installation delta` and `config delta`

Two read-only views of "how does the tree on disk differ from what cactup
believes about it". Neither takes a lock, writes anything, or touches the
network. They differ in the **baseline** they compare against:

- **`installation delta [<alias>]`** — against **the last fetch**
  (`<installation home>/.cactup/fetch-state.toml`): *what have I changed
  since cactup put these sources here*. An installation with no fetch record
  says so rather than rendering every repo as diverged; local modifications
  are still reported.
- **`config delta [<name>]`** — against **the last build of that config**
  (`sources` in its metadata, §7.4): *what would rebuilding pick up*. This is
  exactly the input the rebuild decision acts on (§7.8 rule 5), so the two can
  never disagree.

**`installation delta` reports two independent things, because the tree can
diverge in two independent ways.**

1. **Per repo**, from a git status walk of `<root-dir>/repos/<repo>`: a moved
   HEAD, a changed branch, modified tracked files, untracked files (which never
   affect a build but do block `--prune`), a repo the fetch recorded that is no
   longer on disk, and a repo directory that **cannot be inspected as a git
   repo at all** — most often because someone replaced the checkout with a
   hand-built variant that has no `.git/`.
2. **Per thorn**, from the arrangement symlink each git component owns (the
   link pass, §3.2). A repo can be a pristine checkout while the arrangement
   entry that puts its thorn into the build is something else entirely, and no
   per-repo walk can see that — the divergence is not *inside* any repo. Each
   link is classified without touching it: sound; **a real file or directory
   standing in for the link** (a hand-placed thorn — cactup will never
   overwrite it and the build compiles it as-is); a symlink pointing **outside
   `repos/`** (hand-made or another tool's, equally untouchable); a symlink
   into `repos/` at the **wrong thorn** (a refetch would repoint it); a link
   whose **target is not there**; and **no link at all** for a thorn the
   thornlist names.

   The fetcher collapses the first two into one "not mine, don't touch"
   outcome, which is right for *acting* on them and wrong for *reporting* them:
   a hand-placed thorn and a foreign symlink call for different responses, so
   this view keeps them apart. Both the report and the fetcher resolve the link
   path through the same code, so they cannot disagree about which path on disk
   is a given thorn's link.

Resolving ~400 thorn links walks each path component with a `canonicalize` at
each step, so both phases fan out over the parallel pool under progress, like
every other whole-tree walk.

## 4. The machine database (MDB)

Reorganized per `cactup-simfactory-design.txt`. The goal is **minimal divergence
from simfactory's MDB content** — only the structural/feature-driven changes
below.

### 4.1 Layout

```
mdb/
  <machine>/
    meta.toml                       # machine metadata (TOML + literal @templating@)
    hostname.regexp                 # one Rust regex claiming this machine's hosts (optional)
    discover.py                     # def is_machine(hostname: str) -> bool (optional)
    optionlists/
      <variant>.toml                # build option lists (TOML + @templating@)
    runscripts/
      <variant>.sh                  # or <variant>.py
    submitscripts/
      <variant>.sh                  # or <variant>.py
```

**No top-level index (D1).** There is no `mdb/meta.toml`; the authoritative data
is the per-machine `<machine>/meta.toml` only. `cactup machine show` and
discovery enumerate machines by listing the (few) per-machine directories and
reading each `meta.toml` — a directory scan whose cost is negligible for the
machine counts cactup deals with, and one fewer file to keep in sync. (The
top-level `GENERATION` and `GENERATIONS.md` files are not an index; they
version the published MDB — D14, §17.5 — and machine enumeration skips them.)

**Two-layer MDB (read-only system + writable user overlay).** cactup resolves
machines from two roots:

1. **System MDB** — `~/.cactup/mdb` (prod, git-cloned) or the dev path. Read-only
   to cactup; an MDB update overwrites it. Ships the built-in `generic` machine
   (§4.6) and any ported clusters.
2. **User MDB** — `~/.cactup/machines/`. Writable; holds machines created by
   `cactup machine create` (§4.7). Never modified by MDB updates.

A machine present in **both** layers resolves from the **user MDB** (it wins),
so a user can override a shipped machine without editing the cloned repo (which
would be clobbered on update). Discovery (§4.3) and `machine show` scan both
layers, but name-shadowing is applied **first**: when a machine name exists in
both layers, only the user-MDB copy participates — only its matchers are run
for that name, and only it is listed — so overriding a shipped machine never
produces a spurious double-match (§4.3) against its own system-layer original.
Each layer has the same internal structure (per-machine directories; no
top-level index — D1).

### 4.2 `<machine>/meta.toml`

TOML port of simfactory's `mdb/machines/<name>.ini` (`simfactory-docs.txt` §8).
**All recognized simfactory machine keys are carried over**, with these changes:

- Keys that only existed for the dropped subsystems are removed: `iomachine`,
  `trampoline`, `rsynccmd`, `rsyncopts`, `sshcmd`, `sshopts`, `localssh*`,
  `archivetype`, `archiveuser`, `archivebasepath`, `archivehostname`,
  `archivetoolspath`. (Remote/archive are gone — D1, D2.)
- `aliaspattern` (hostname regex) is **removed**; replaced by the
  `hostname.regexp` / `discover.py` matcher files (§4.3).
- `submitscript` / `runscript` / `optionlist` single-filename keys are
  **replaced** by the per-machine `optionlists/`, `runscripts/`,
  `submitscripts/` directories and the **variant→queue mapping** described in
  §4.4. (This is the headline MDB change.)
- `env-setup` (shell setup, usually module loads) is kept verbatim. Unlike
  simfactory (where it fed only submit/run scripts), cactup applies `env-setup`
  to **all three** execution phases by default — **build**, **submit**, and
  **run** — because a Cactus build usually needs the same module environment as
  the eventual run (compilers, MPI, etc.), and requiring the user to duplicate it
  was a common footgun. For phase-specific additions there are three optional
  companion keys — **`env-build-setup`**, **`env-submit-setup`**, and
  **`env-run-setup`** — each *appended* to `env-setup` for that phase only. The
  effective environment for a phase is `env-setup` followed by the matching
  `env-<phase>-setup` (empty when unset). This is opt-in granularity: a machine
  that needs no per-phase difference sets only `env-setup`; a machine that (say)
  loads a debugger module only when building sets `env-build-setup`. §6.1
  specifies how each block is injected for `.sh` vs `.py` templates and for the
  build's `make` invocation.
- **Universe-level env-setup overrides.** A `[universes.<name>]` table (§4.8)
  may itself carry `env-setup` / `env-build-setup` / `env-submit-setup` /
  `env-run-setup`, alongside its wrapper keys (or with no wrapper at all — an
  identity universe, §4.8). Where set, a key in the universe table **replaces**
  the machine `[environment]` key of the same name, **key-by-key**, for any
  phase executed in that universe; a key the universe table omits still falls
  back to the machine's `[environment]` value. The concatenation that produces
  the effective per-phase block (`env-setup` then `env-<phase>-setup`, §6.1) is
  otherwise unchanged — it is simply evaluated against whichever of the two
  (universe or machine) key is in force. See §4.8 for the full resolution
  story (including which universe is "in effect" for a given phase).
- Scheduler keys carried over (semantics unchanged; names kebab-normalized —
  see the key-naming note below): `submit`, `interactive-cmd`, `get-status`,
  `stop`, `submit-pattern`, `status-pattern`, `queued-pattern`, `running-pattern`,
  `holding-pattern`, `exec-host`, `exec-host-pattern`, `stdout`, `stderr`,
  `stdout-follow`, `max-queue-slots`.
- One scheduler key is new, with no simfactory ancestor: **`blocking-submit`**
  (§7.9, §10) — the exact analog of `submit` except that it does not return
  until the job it queues has *finished*: `sbatch --wait @SCRIPTFILE@` where
  `submit` is plain `sbatch @SCRIPTFILE@`, `qsub -W block=true @SCRIPTFILE@` on
  PBS Pro, or (on a machine whose `submit` backgrounds the script and echoes
  `$!`) that same command with `wait $pid` appended. It is optional and read
  only for `cactup build submit --block`; a machine that omits it still
  supports `--block`, via the emulated poll-until-outcome loop instead of a
  native blocking command. It is a *flavor* of `submit`, never a replacement:
  every other submission on the machine still goes through `submit`, so
  declaring `blocking-submit` without it fails validation rather than
  producing a machine that submits nothing.
- **Walltime ceiling (single, unambiguous rule):** the per-queue
  `[queues.<q>].max-walltime` is authoritative. The machine-level `max-walltime`
  (under `[scheduler]`) is the **fallback** used only for a queue that omits
  `max-walltime`. The effective ceiling for auto-chaining (§8.8) is therefore:
  chosen queue's `max-walltime` → machine `max-walltime` → built-in
  `365-00:00:00` (one year). Both keys share the same kebab name `max-walltime`
  and are disambiguated purely by **table scope** (`[queues.<q>]` vs
  `[scheduler]`), not by spelling. Both use the canonical walltime form defined
  in §8.5 — `(DD-)?HH:MM:SS` — parsed to an integer number of seconds internally,
  so comparisons and the chaining division are unit-consistent regardless of how
  the value was written.
- Hardware/capacity keys **stripped to what submit/run scripts actually
  consume**, and renamed for what they mean to cactup (honoring the task=rank
  / CPU=core terminology): `ppn` → `max-cpus-per-node` (it is CPUs/cores per
  node, **not** ranks), `num-threads` → `default-cpus-per-task` (the default
  `CPUS_PER_TASK` when `--cpus` is omitted), `num-smt` → `threads-per-cpu`,
  `memory` kept (plus the `autodetect` control flag, §4.6). Two keys are new,
  with no simfactory ancestor: `max-gpus-per-node` and `default-gpus-per-task`,
  the GPU counterparts of the two CPU keys (§8.5). simfactory's
  informational keys (`spn`, `mpn`, `nodes`, `max-num-threads`, `max-num-smt`,
  `min-ppn`, `cpu-freq`, `flop-per-cycle`, cache descriptors, `efficiency`,
  `quota`, `cpu`) are dropped on port — nothing consumed them. Each kept key
  may be set machine-wide in `[hardware]` and/or overridden per-queue in
  `[queues.<q>]` (§4.2).
- **`[paths]` values resolve at use time, not at MDB load** — `@USER@` plus
  any `@ENV(NAME)@` reads (§6.1; unset or empty env var = hard error). Use-time
  resolution lets an entry whose paths need the machine's own environment
  (e.g. `simulation-home = "@ENV(SCRATCH)@/simulations"` for TACC's hashed
  scratch root) load and validate on any host; the hard error fires only when
  a path is actually needed (`install` / `sim create` / `test`). `machine
  show` prints resolved paths when the host can resolve them and the raw
  templates otherwise.
- **`simulation-home`** (the renamed successor to simfactory's `basedir`) and
  **`install-home`** (the renamed successor to simfactory's `sourcebasedir`) are
  the two per-machine root-path keys; both are **optional** and behave the same
  way — a machine may set either, both, or neither, and each has its own fallback:
  - `simulation-home` is the per-machine **simulation-output root**; omitted →
    fall back to `~/.cactup/simulations`. Simulations live under
    `<simulation-home>/<alias>/…` (§8.1). Real clusters set it to fast/large
    storage (e.g. `simulation-home = "/work/@USER@/simulations"`) so output does
    not land on the small-quota home filesystem.
  - `install-home` is the per-machine **default install prefix** — the
    machine-scoped default for `cactup install` when `--install-prefix` is not
    given; omitted → fall back to `~/.cactup/cacti`. Installations live under
    `<install-home>/<alias>/` (as today). A cluster whose `$HOME` is too small to
    hold a Cactus build sets `install-home` to roomier storage (e.g.
    `install-home = "/work/@USER@/cacti"`), the same home-quota escape valve
    `simulation-home` provides for output. `--install-prefix` on `cactup install`
    still overrides per-install. (This subsumes simfactory's `sourcebasedir`,
    whose other jobs — remote-sync paths and machine disambiguation — are gone
    under D1/§4.3.)
- **`test-home`** is the per-machine **testsuite-output root**, the direct
  analog of `simulation-home` for the `cactup test` subsystem (§11.5); optional,
  omitted → falls back to `~/.cactup/tests`. Test runs live under
  `<test-home>/<alias>/…`, kept entirely out of `simulation-home`. Clusters set it
  to fast/large storage (e.g. `test-home = "/work/@USER@/tests"`) for the same
  home-quota reason as `simulation-home`.
- `scratch-home` kept. If set, it is exposed to scripts as `@SCRATCH_HOME@`
  (with `@USER@` substituted) for run scripts that stage to fast scratch; it is
  otherwise unused by cactup itself. A machine that does not set it leaves
  `@SCRATCH_HOME@` empty.
- **`build-cache-home`** (optional) is where the build cache keeps its
  objects (§18.7); omitted → `<install-home>/.cactup-build-cache`, beside the
  installations (the `install-home` fallback applies, so
  `~/.cactup/cacti/.cactup-build-cache`). A serving build writes a cold
  Einstein Toolkit's worth of objects (about half a gigabyte in some three
  thousand files) there, from compute nodes too; next to the installations is
  where builds already do their I/O, and storage a site gave them is purged
  less than scratch. No machine in the MDB sets it: the default follows
  `install-home`. It is for a site whose builds belong somewhere else.
  `@USER@` and `@ENV(NAME)@` work as in `simulation-home`; a per-user
  directory, since anyone who can write in a store can put objects into
  every build that reads it. Unlike the other paths it is resolved leniently
  (`objcache::store_root`): a value that cannot be resolved (an `@ENV(…)@`
  unset) is passed over for the next place with one line, never failing a
  build, a simulation or an install. A purge of the store's storage costs
  misses later, never a wrong object.
- **Every `[paths]` key has a knob of the same name** (§5): `install-home`,
  `simulation-home`, `test-home`, `scratch-home`, `build-cache-home`, an
  absolute path that wins over the machine's value for the user who sets it
  (`Meta::path_for`/`Meta::paths_for`; a machine value a knob overrides is
  not resolved at all). They are read where the machine's are, on the login
  node: `simulation-home` and `test-home` when an installation's homes are
  fixed (install time, or `cactup use` backfilling them), `install-home` for
  the install-prefix default and the build cache's place, `scratch-home`
  when a restart's, a test's or a build's variables are assembled — so a
  compute-node run sees the value frozen into its metadata (D11).
- Thorn-toggle keys kept (D8, §7.5): `enabled-thorns`, `disabled-thorns`, and
  their `-default`/`-local` variants collapse to plain `enabled-thorns` /
  `disabled-thorns` arrays in TOML (the `-default`/`-local` split existed only
  for the `defs.local.ini` layering, which is gone — §5.3).
- `make` / `make-jobs` kept (§7.7).
- **Queued builds (§7.9): new `[build]` keys and a new script kind.** Some
  clusters forbid `make` on a login node — the build itself has to go through
  the batch queue, the same way a simulation does. `[build]` gains seven
  optional keys for this: **`default-action`** (`"run"` or `"submit"`; forces
  an unqualified `cactup build` to one or the other instead of the §7.9
  auto-selection) and six job-shape defaults consulted only when a build is
  submitted — **`queue`**, **`walltime`**, **`nodes`**, **`tasks`**,
  **`cpus-per-task`** (falls back to `make-jobs` when unset, so a machine that
  already tunes `make-jobs` for its node size gets a matching job shape for
  free), and **`gpus-per-task`**. None of the six affect a foreground build.
  Alongside them, a machine that can submit builds declares a
  **`buildsubmitscript`** script kind — a fourth sibling of `optionlist` /
  `submitscript` / `runscript`, with its own `buildsubmitscripts/<variant>.{sh,py}`
  directory and `[variants.buildsubmitscript]` table (identical shape to
  `[variants.submitscript]`: queues, an optional `universe`/`build-universes`,
  a `default` marker — §4.4). It is the **only** optional script kind — a
  machine that never hands a build to the scheduler declares no
  `buildsubmitscript` variants at all, and MDB load validation skips the usual
  queue-coverage check for a kind with none (§7.9 states the precise rule for
  when submitting is even possible).

**Key-naming convention (kebab-case everywhere).** Every user- and disk-facing
identifier is **kebab-case**: TOML keys (`meta.toml`, optionlists, and cactup's
own `cactup-config.toml`, `installation.toml`, `simulations.toml`,
`simulation.toml`, `restart.toml`), **JSON keys** in the global DB
(`database.json` — `cactup-version`, `active-installation`, `detected-machine`;
serialized via serde `rename_all = "kebab-case"` so Rust struct fields stay
idiomatic snake while the on-disk keys are kebab), **CLI long flags**
(`--install-prefix`, `--sim-dir`, `--restart-id`, `--mail-type`, …), and **knob
keys** (`allocation`, `mail`, `mail-type`, `queue`). simfactory's smushed
spellings are normalized on port (`getstatus` → `get-status`, `submitpattern` →
`submit-pattern`, `exechost` → `exec-host`, `maxqueueslots` → `max-queue-slots`,
`envsetup` → `env-setup`, `makejobs` → `make-jobs`, `maxwalltime` →
`max-walltime`, `scratchbasedir` → `scratch-home`, …), and cactup's own keys
use hyphens too
(`config-id`, `build-id`, `job-id`, `chained-job-id`,
`simulation-id`, `compatible-queues`). The **only** identifiers that keep
underscores are **template substitution variables**, which stay `UPPER_SNAKE`
inside `@…@` (a deliberately separate namespace — `@JOB_ID@`, `@SCRATCH_HOME@`,
`@TASKS_PER_NODE@`).

**meta.toml table structure.** Scalar keys are grouped into tables for clarity
(TOML requires top-level keys before any table, so grouping avoids ordering
pitfalls). The tables are: `[machine]` (descriptive + access), `[paths]`
(`install-home`, `simulation-home`, `test-home`, `scratch-home`, `build-cache-home`), `[hardware]` (`max-cpus-per-node`,
`default-cpus-per-task`, `threads-per-cpu`, `memory` — machine-wide defaults, each overridable per-queue; see below), `[build]` (`make`, `make-jobs`,
`enabled-thorns`, `disabled-thorns`, and the queued-build keys `default-action`,
`queue`, `walltime`, `nodes`, `tasks`, `cpus-per-task`, `gpus-per-task` — §7.9),
`[environment]` (`env-setup` and the
phase-specific `env-build-setup` / `env-submit-setup` / `env-run-setup` — §6.1;
grouped here rather than under `[scheduler]` because `env-setup` now spans build
as well as submit/run), `[scheduler]` (`submit`, `get-status`, `stop`, the
`*-pattern`s, `exec-host`, `max-walltime`), then `[queues.*]`, `[variants.*]`
(`optionlist`, `submitscript`, `runscript`, and — only on a machine that
submits builds — `buildsubmitscript`, §7.9),
and (optional) `[universes.*]` (§4.8 — a wrapper spec and/or per-universe
`env-*-setup` overrides; the always-available `"host"` universe needs no table
at all unless it is being customized).

**The schema is closed.** Every table above rejects keys it does not model: an
unrecognized key is a load-time error naming it (and listing the accepted
spellings), never a silent no-op — a typo'd `max-cpu-per-node` must not read as
"do nothing". Keys kept purely for the human reader are therefore *modeled*
rather than tolerated (`[machine].webpage`, `[universes.<name>].kind`), and
anything simfactory carried that cactup dropped — `allocation` (a per-user knob,
§5), `stdout`/`stderr`/`stdout-follow`, `max-queue-slots`, the capacity keys
below — belongs in a TOML comment if it is worth recording at all, not in a key.

```toml
[machine]
name = "mike"
nickname = "mike"
status = "production"          # personal|experimental|production|storage|outdated
hostname = "mike.hpc.lsu.edu"
# … location, description, etc …

[hardware]                     # machine-wide defaults; OPTIONAL if every queue
max-cpus-per-node = 16         # CPUs/cores per node (see [queues.*] overrides)
memory = 196608                # MB per node
# default-cpus-per-task = 1    # optional; default CPUS_PER_TASK when -c omitted
# max-gpus-per-node = 4        # optional; GPUs/node (absent on CPU-only machines)
# default-gpus-per-task = 1    # optional; default GPUS_PER_TASK when -G omitted
# threads-per-cpu = 1          # optional; defaults to 1 (§8.5)

[build]
make = "make -j@MAKEJOBS@"
make-jobs = 16
# default-action = "submit"    # optional (§7.9): force `cactup build` to
                                # submit even where running is also possible;
                                # requires [variants.buildsubmitscript] below
                                # AND [scheduler].submit — the queued-build job
                                # shape a submitted build uses:
# queue         = "checkpt"    # optional; else -q, else the default queue
# walltime      = "4:00:00"    # optional; else -w, else the queue's ceiling
# nodes         = 1            # optional; else -n, else 1
# tasks         = 1            # optional; else -T
# cpus-per-task = 16           # optional; else -c, else make-jobs, else 1
# gpus-per-task = 0            # optional; else -G

[scheduler]
submit = "sbatch @SCRIPTFILE@ 2>&1"
get-status = "squeue -j @JOB_ID@"
# … patterns, stop, … …

[environment]
env-setup = """
module load gcc/11 openmpi/4
"""                            # applied to build, submit, AND run (§6.1)
env-build-setup = "module load cmake/3.27"   # appended only for `cactup build`
# env-submit-setup / env-run-setup: appended only for submit / run (optional)

[queues.checkpt]               # one table per queue
gpu = false
max-walltime = "72:00:00"
default = true                 # the queue used when -q is omitted (one queue may be default)

[queues.gpu]
gpu = true
max-walltime = "24:00:00"
max-cpus-per-node = 64         # per-queue hardware override (heterogeneous
threads-per-cpu = 2            # partitions); any key not set here inherits
                               # the [hardware] value (memory, in this example)
# name = "gpu_part"            # optional: the scheduler's REAL queue/partition
                               # name when it differs from the table key. @QUEUE@
                               # resolves to it (default: the key itself). Lets
                               # several cactup queues — e.g. cpu/gpu build
                               # flavors with different D12 gpu flags — map onto
                               # one real queue without hardcoding it in scripts.

# Variant → queue association (§4.4). Every key names a variant; there are no
# reserved keys. A variant is either the array shorthand (queues only) or the
# inline-table form `{ queues = [...], universe = "…", build-universes = […],
# test = …, default = …, tasks = … }` when it carries a universe to run *in*
# (§4.8 step 3), a build-universe COMPATIBILITY list for selection (below),
# a test marker (§11.2), the default flag, or a default task count (`tasks =
# N`: the TASKS used when no -n/-T/-t flag is given, instead of filling the
# node — §8.5). `universe` and `build-universes` are independent and may both be
# set: `universe` is what THIS variant's own execution is wrapped in;
# `build-universes` is which configs' BUILD universes this variant is compatible
# with (omitted = all — the common case).
# `default = true` marks the fallback variant within its partition (see below).
[variants.submitscript]
"slurm-cpu" = { queues = ["checkpt", "single"], default = true }   # normal default: serves these queues + any queue with no explicit mapping
"slurm-gpu" = ["gpu"]
"slurm-test" = { queues = ["checkpt", "single"], test = true, default = true }   # testsuite submit + test-partition default (§11.2)

[variants.runscript]
"cpu" = { queues = ["checkpt", "single"], default = true }
"gpu-sing" = { queues = ["gpu"], universe = "et-sif" }   # runs inside a universe (§4.8)
"sing" = { queues = ["gpu"], build-universes = ["et-sing", "et-sing-cpu"] }
                               # selected only for configs whose BUILD universe
                               # (§7.4) is "et-sing" or "et-sing-cpu" (§4.4,
                               # §4.8); replaces the old trick of minting a
                               # synthetic queue per build flavor purely to
                               # route script selection (see the db1 MDB port)
"test-cpu" = { queues = ["checkpt", "single"], test = true, default = true }   # drives make <config>-testsuite (§11.6)
# default-universe = "et-sif"          # optional: universe for runscript variants that omit one

# OPTIONAL — only on a machine that hands builds to the scheduler (§7.9): same
# shape as [variants.submitscript], one buildsubmitscripts/<variant>.{sh,py}
# per key. Omitted entirely (no table at all) ⇒ this machine can never submit a
# build; `cactup build`/`build submit` always run in the foreground.
[variants.buildsubmitscript]
"slurm-cpu" = { queues = ["checkpt", "single"], default = true }

# Each variant just names the optionlist file under optionlists/<variant>.toml;
# that file declares its own gpu flag, compatible queues, and an optional
# `default = true` marker in its [cactup] header (D12, §7.8, §4.4). With one
# variant that variant is implicit; with several, the default-marked one is the
# implicit pick and the rest need --variant.
[variants.optionlist]
variants = ["cpu", "gpu", "cpu-debug"]   # selected at build time via --variant;
                                         # cpu.toml carries [cactup].default = true (§4.4)
```

**Hardware keys.** `[hardware]` carries **only** the keys cactup actually
feeds to submit/run scripts, named for what they mean to cactup (task = MPI
rank, CPU = core — §1.1): `max-cpus-per-node` (simfactory's `ppn`; the
availability fact = **cores** per node, **not** ranks and **not** hardware
threads — a node may boot with SMT disabled and the process layout must not move
under it, so SMT is the separate `threads-per-cpu`; drives the §8.5
process-layout defaults and `@MAX_CPUS_PER_NODE@`), `default-cpus-per-task`
(simfactory's `num-threads`; the request-side default for `CPUS_PER_TASK` when
`--cpus` is omitted — §8.5), `max-gpus-per-node` (the GPUs available per node;
a **ceiling** §8.5 refuses to exceed, plus `@MAX_GPUS_PER_NODE@` — it does not
set `GPUS_PER_TASK`, which defaults to 1; routinely **absent**, since most
machines and most partitions have no GPUs, and absence simply means nothing is
checked), `default-gpus-per-task` (the request-side default for `GPUS_PER_TASK`
when `--gpus-per-task` is omitted — worth setting only on a partition that wants
something other than one GPU per rank),
`memory` (`@MEMORY@`, per-node MB), and
`threads-per-cpu` (simfactory's `num-smt`; `@THREADS_PER_CPU@`, default 1) —
plus the `autodetect` control flag (§4.6). simfactory's other
capacity/documentation keys (`nodes`, `min-ppn`, `spn`, `mpn`,
`max-num-threads`, `max-num-smt`, `cpu-freq`, `flop-per-cycle`, …) are dropped:
nothing consumed them (`@CPUFREQ@` is likewise no longer produced — no script
ever used it). Each hardware key may **also** be set inside a
`[queues.<name>]` table as a per-queue override for machines with
heterogeneous partitions (e.g. fatter GPU nodes). Resolution is key-by-key:
the queue's value where set, else the top-level `[hardware]` value. The
top-level table is therefore **optional** — a machine may define hardware
entirely per-queue — and a queue with no overrides simply inherits everything.
Topology derivation (§8.5) and the machine-derived script variables (§6.3) use
the **queue-effective** values for the queue the job targets.

The default variant. Instead of a reserved `default = "<name>"` pointer key
(which would collide with a variant literally named `default` and force the
token `default` to serve as both a variant name and a directive), the fallback
variant is marked in place with `default = true` inside its own inline table —
exactly parallel to `test = true`. `default = true` designates the fallback
**within its partition**: an un-marked variant is the *normal-partition* default;
a variant carrying both `test = true` and `default = true` is the *test-partition*
default (the old `test-default`). **At most one** variant per partition may carry
`default = true`; a partition with exactly one variant makes that variant the
implicit default, so the flag is optional there (it may still be stated for
clarity, as mel5 does). Because every key names a variant, a variant may now be
named `default` with no special meaning.

Queue/variant consistency requirement: every queue listed in `[queues.*]` must
be served by some `submitscript` variant and some `runscript` variant (an
explicit mapping or the `default = true` variant). cactup validates this at MDB
load and errors on a queue with no resolvable script variant, rather than failing
cryptically at submit time. When a machine defines any **test-marked** variants
of a kind (`test = true`), the same coverage check is applied **within the test
partition** (against the test-partition default); a kind with no test variants is
fine — tests borrow the normal partition (§11.2).

**Build-universe compatibility list (`build-universes`).** The inline-table
variant form may carry `build-universes = ["name", …]` — the set of build
universes this variant is compatible with (§4.8). Omitted (the default) means
compatible with every universe. An explicitly empty list (`build-universes =
[]`) is a validation error (omit the key instead). Every name listed must be a
declared `[universes.<name>]` or the literal `"host"` (§4.8's always-available
implicit universe). Selection filters on the **config's build universe** —
`"host"` when the config records none — never on the run/submit universe
context; this is the parity `build-universes` enforces: a config built in
universe X gets X-compatible run/submit scripts (§4.4). The `build-` prefix on
the key name records exactly this: the gate is on the universe a config was
*built* in, not the one it is run or submitted in.

**Queues carry the same key.** A `[queues.<name>]` table may likewise declare
`build-universes = ["name", …]` (same semantics, same validation), so a machine
that unifies several upstream clusters behind one set of scheduler queues can
restrict a queue to the build flavors it serves. When `-q` is omitted, the
default-queue pick is restricted to the queues compatible with the config's
build universe; naming an incompatible queue explicitly (via `-q`, the `queue`
knob, or a config's compatible-queues) is a **hard error** naming the universe
— there is no `--force-queue` escape, since the gate is structural (like
variant compatibility) rather than advisory (like the GPU / compatible-queues
guards). A queue with no `build-universes` list serves every build universe.

Validation adds an **ambiguity check per universe context**: for each *u* in
{`"host"`} ∪ the machine's declared universes, and separately within each
partition (normal/test, §11.2), no queue *compatible with u* may be served by
more than one *u*-compatible variant (a variant with no `build-universes` list
is compatible with every *u*, and so counts toward every *u*'s check; a queue
gated away from *u* is skipped in that context). Queue **coverage** (every
queue served-or-default, above) is enforced at MDB load time only for *u* =
`"host"`, exactly as before, and only for the queues compatible with host; for
any other declared universe, a queue left unserved by that universe's compatible
variants is instead a **selection-time** error — "no `<kind>` variant
compatible with universe \"X\" serves queue \"Y\" and none is a compatible
default" — since exhaustively checking every declared universe against every
queue at load time would reject machines that intentionally scope a universe
to a subset of queues. A machine with no `build-universes` lists anywhere
validates and selects **byte-identically to today**.

The `@templating@` inside `meta.toml` values uses **literal `@NAME@`
substitution only** (D7): the only variables meaningful here are the install-
context ones (`@USER@`, etc.). Example: `simulation-home = "/work/@USER@/simulations"`.

### 4.3 Discovery: `hostname.regexp` and `discover.py`

Replaces simfactory's `aliaspattern` hostname regex (`simfactory-docs.txt` §9).

- cactup determines the local `hostname` (`--hostname` override → `~/.hostname` →
  system FQDN) and asks each machine whether it claims that host. A machine
  may ship either or both of two **matcher files**:
  - `mdb/<machine>/hostname.regexp` — the whole file, trimmed, is one pattern
    in the Rust `regex` dialect (there is no comment syntax; a `#` is part of
    the pattern). It claims the host when it matches the hostname as resolved
    **or** its short form (the first label), so `^ln[1-4]$` claims both `ln1`
    and `ln1.cosma.dur.ac.uk` — the two probes every ported simfactory pattern
    made. A file that is empty or not a valid regex is an authoring bug: cactup
    warns (always, not only under `-v`) and treats it as "did not match".
  - `mdb/<machine>/discover.py` — `def is_machine(hostname: str) -> bool:`.
    The implementation may use the supplied string or ignore it and do its own
    probing (e.g. read an env var, check a sentinel file). A script that raises
    (or calls `sys.exit`) is "did not match", with a warning under `-v`.
- The regexp is tried first and is decisive when it matches; `discover.py` runs
  only for machines whose regexp is absent or did not match. Every shipped
  machine's pattern is a regexp, so a stock MDB never spawns Python for
  discovery; `discover.py` is the escape hatch for a site that needs more than
  the hostname. A machine with neither file is simply not discoverable — it is
  selectable only via `--machine`.
- Exactly one claim → that machine. More than one → **prompt the user to
  disambiguate** — **except in non-interactive mode** (`--silent`, or no tty),
  where cactup cannot prompt: it then **errors and requires `--machine <name>`**
  to pick one explicitly. (Recall name-shadowing already removes the
  common self-collision, §4.1, so a genuine multi-match means two distinct
  machines both claim this host.)
- **Zero claims → fall back to `generic`, do not fail (§4.6).** An unrecognized
  host (e.g. a personal laptop with no scheduler) is the *common* case for
  newcomers, not an error. cactup uses the built-in `generic` machine — with
  hardware autodetected at runtime — so the command works out of the box, and
  prints a one-line notice suggesting `cactup machine create` (§4.7) to persist a
  tuned local machine. (`generic` ships no matcher file, so it never
  participates in matching; it is *only* the zero-match fallback.)
- `--machine <name>` overrides outright and skips discovery entirely (e.g.
  `--machine generic`).

**The result is cached persistently, with a verification stamp.** The resolved
machine (including a disambiguation choice) is one record in the global DB —
**not** one per hostname — stamped with the hostname it was resolved for and
the login session that did it:

```jsonc
// database.json
"detected": { "name": "mike", "hostname": "mike1.hpc.lsu.edu", "session": "19840:590094" }
```

The session is the session leader's pid and start time from `/proc/self/stat`
(field 6, then field 22 of the leader) — for an interactive login that is the
login shell; a `setsid`/cron/`nohup` invocation is its own session, and a
platform without `/proc` records none, which never compares equal. On every
resolution:

1. `--machine` given → use it; the cache is neither read nor written.
2. The stamp names this hostname **and** this session → trust the name. No
   matcher runs; this is every command after the first in a shell.
3. Otherwise **re-verify**: run only the cached machine's own matchers against
   the current hostname. Still claimed → restamp and use it. That is what a new
   login shell, or the first command on another node, costs: one regex, or one
   `python3` if the machine only has a `discover.py`.
4. Not claimed → say so and run full discovery. A *different* machine claiming
   the host replaces the cached one ("Detected machine changed from A to B").
   **Nobody** claiming it keeps the cached machine, with a one-time note and a
   restamp so the session stays quiet: an interactive allocation puts the user
   on a compute-node hostname that a login-only pattern never claims, and that
   is not a different machine — dropping to `generic` there would be wrong.
   `cactup machine forget` is the way out when it really is one.

Rationale for one record: a given `~/.cactup` logically belongs to one machine,
so keying by hostname would only fragment the cache across a cluster's many
login/compute node FQDNs (`mike1`, `mike2`, …) and re-prompt on each. The stamp
is what closes the gap that a bare string left open: a `~/.cactup` on a home
filesystem *shared* between two distinct clusters used to carry cluster A's
machine onto cluster B silently; now the first command on B re-verifies, finds
A does not claim B's host, and discovers B. `cactup machine forget` clears the
record.

**ASSUMPTION (Python runtime):** cactup shells out to a `python3` on `PATH` to
evaluate `discover.py` and `.py` script variants (cactup is Rust; embedding
CPython is heavier than needed).

**Security & cost note.** `discover.py` (and `.py` script variants) are arbitrary
code from the MDB git repo, executed on the user's machine. The MDB repo is
therefore a **trust boundary**: cactup runs only the MDB it cloned from the
configured `manifest`/MDB URL, and the spec assumes that repo is trusted. Cost:
the regexps are evaluated in-process; every `discover.py` that still needs to
run is evaluated in **one** `python3` process (each script in its own `runpy`
namespace, its stdout diverted to stderr so it cannot corrupt the protocol) —
interpreter start-up is the whole cost (~15 ms per spawn against well under a
millisecond per module; 38 machines went from ~570 ms to ~20 ms), so there is
nothing to gain from stitching the scripts into one module and a lot of
fragility (name collisions, imports, error isolation) to lose. And matchers run
only on a stamp miss (step 3 above), never on the steady-state command. The
`.py` calling convention (§6.1) likewise batches all variable injection into a
single `python3` invocation per script.

### 4.4 Variants (the headline new feature)

From `cactup-simfactory-design.txt`. simfactory faked variants with separate
machine definitions (e.g. `db-sing-cpu.run` / `db-sing-nv.run`); cactup makes
them first-class within one machine.

- **OptionList variants** select compile configuration. If a machine has exactly
  one optionlist variant, it is used implicitly. If it has more than one, the
  user **must** pick one at `cactup build` time via `--variant`; there is no
  default. The chosen variant is recorded in the config metadata (§7.4) and is
  **sticky**: later rebuilds of that config reuse it without the flag, and
  `--variant` is repeated only to switch flavors, or to move a config back off a
  `--optionlist` file (§7.8). A recorded variant the machine has since dropped is
  a hard error naming it, never a silent fallback.
  `cactup build --optionlist PATH` displaces this selection entirely, building
  from a file outside the MDB — an MDB-shaped `.toml`, an `[options]`-only
  `.toml`, or a native Cactus `.cfg`. It is mutually exclusive with `--variant`,
  and a file that carries a `[cactup]` header is validated by it exactly as an
  MDB variant would be; §7.8 has the forms, the detection rule, and what gets
  recorded.
- **OptionList variant ↔ queue compatibility (D12).** Each optionlist TOML
  declares, in its `[cactup]` header (§7.8), a `compatible-queues` list and a
  `gpu` flag. At `sim submit`/`sim run`, cactup checks the chosen queue against
  the built config's `compatible-queues`; a mismatch is a **hard error**
  (overridable with `--force-queue`/`-f` for experts). This is what prevents
  submitting a GPU binary to a CPU queue. The **gpu/binary cross-check is
  one-directional**: refuse (absent `--force-queue`/`-f`) only when the
  config's `gpu` flag is set and the effective context (`-g/--gpu`, or else the
  chosen queue's `gpu` flag) is **not** — a GPU-built config on a non-GPU queue
  is refused, but a non-GPU config on a GPU queue is **allowed**: nothing about
  running a CPU-only binary on GPU hardware is wrong, and some machines' only
  real partition is GPU-flagged (a cluster like this makes GPU flags on
  otherwise-CPU queues a routing convenience rather than a hardware
  distinction — the db1 MDB port, §4.8, is the worked example). cactup prints
  an advisory note in that (allowed) case only when the machine has at least
  one non-GPU queue the user could have used instead; a machine with no
  non-GPU queue at all never sees it. `-g/--gpu` itself still defaults from
  the chosen queue's `gpu` flag when not passed explicitly.
- **SubmitScript and RunScript variants** are each associated with one or more
  **queues** (the `[variants.*]` tables in §4.2) and, optionally, a
  **build-universe compatibility list** (`build-universes = […]`, §4.8) — a
  second, orthogonal routing dimension alongside queue: queue narrows *which
  partition* a variant serves; `build-universes` narrows *which build
  universe's* configs it serves. At submit/run time cactup first filters the
  queue's candidate variants to those compatible with the config's **build
  universe** (§4.8 — `"host"` when the config records none), then picks among the
  survivors exactly as before: the variant mapped to the chosen `-q/--queue`,
  or (absent an explicit mapping) the variant marked `default = true`.
  Omitting `build-universes` (the common case, and the only case before this
  mechanism existed) means "compatible with every universe," so a machine with
  no universe-routing need is unaffected. An explicit `--variant` naming a
  variant incompatible with the build universe is a **hard error** naming the
  universe. The **queues** themselves (`[queues.*]`, §4.2) accept the same
  `build-universes` key with identical semantics, gating which build flavors may
  target a queue at all. (A machine with a single variant makes it the implicit
  default, so the `default` flag is optional there.) The submit-script and
  run-script variant maps are independent of each other but must each cover
  every queue, for every universe context in play — validated at MDB **load**
  time for `"host"`; for any other declared universe, an uncovered queue is
  instead a **selection-time** error (§4.2). A variant entry is written either
  as the **array shorthand** (`"<v>" = ["q1", "q2"]` — queues only) or, when it
  must carry a field beyond its queues, the **inline-table form**
  `"<v>" = { queues = ["q1", …], universe = "<name>", build-universes = ["…"], test = true, default = true }`
  where `universe` (the universe this variant's *own* execution runs inside,
  §4.8), `build-universes` (the build-universe compatibility list, above), `test`
  (§11.2), and `default` are each optional and independent of one another; a
  table entry with only `queues` is equivalent to the shorthand. Every key names
  a variant — there are no reserved keys — so a variant may be named `default`
  with no special meaning; defaultness is carried solely by `default = true`.
- Script variants may be `.sh` (literal `@NAME@` templating) or `.py` (emits a
  shell script to stdout; variables are injected per the §6.1 calling
  convention).

### 4.5 Reference port: `mel5`

For development/testing, exactly one machine is ported to the new format and
committed under the dev MDB (`<project root>/mdb/`); **no other machine is
ported** (MDB porting is otherwise out of scope). `mel5` (Melete 05) is a
single-node CCT workstation with no batch scheduler, which makes it a good
end-to-end test target (you can actually build and run locally). The port:

```
mdb/
  mel5/
    meta.toml                    # grouped tables (§4.2); single "local" queue;
                                 #   normal + test-marked script/optionlist variants (§11.2);
                                 #   test-home under [paths] (§11.5)
    hostname.regexp              # ^melete05(\.cct\.lsu\.edu)?$
    optionlists/default.toml     # ported from mel5.cfg; [cactup] gpu=false + [options]
    optionlists/debug.toml       # DEBUG optionlist; reached with cactup build --variant debug (§4.4)
    runscripts/default.sh        # @NUM_PROCS@→@TASKS@, @NUM_THREADS@→@CPUS_PER_TASK@
    runscripts/test.sh           # test=true; drives make <config>-testsuite → test-home (§11.6)
    submitscripts/default.sh     # @SIMFACTORY@→@CACTUP@, +--installation, PID-wait chaining
    submitscripts/test.sh        # test=true; re-invokes `cactup test run`, no chaining (§11.6)
```

It exercises every load-bearing new mechanism: discovery matcher, grouped
`meta.toml`, single-queue/single-variant resolution, the optionlist
TOML→native render (§7.8), the renamed topology variables (§6.3), the
compute-node re-invocation locator (§8.3.1), and — via the `test`-marked
variants — the whole `cactup test` subsystem (§11): the `test = true` marking and
its respective-defaults resolution (§11.2), the testsuite-only variables (§11.9),
and the results-into-test-home redirect (§11.6). Because mel5 has no scheduler,
its `submit` backgrounds the script and echoes a PID; chaining is emulated by
waiting on that PID in plain bash (no `.py` variant needed), and the testsuite
`submit` re-invokes `cactup test run` directly (a testsuite is one-shot — no
chaining, §11.6).

### 4.6 The `generic` machine & runtime hardware detection

`generic` is a built-in machine shipped in the system MDB — the port of
simfactory's `generic.ini`/`.cfg`/`.run`/`.sub` (`simfactory-docs.txt` §9, §20).
It describes a single-node workstation with **no batch system**:

- `submit` = `exec nohup @SCRIPTFILE@ … & echo $!` (background, echo PID as job
  id), `get-status` = `ps @JOB_ID@`, `stop` = `pkill` the process group.
- A single `local` queue (`gpu = false`, `default = true`) and single `default`
  variant of each script.
- It ships no matcher file — `generic` is never auto-matched; it is the
  explicit zero-match fallback (§4.3) and is always selectable via
  `--machine generic`.

**Hardware autodetection.** `generic` declares `[hardware].autodetect = true`
instead of fixed core/RAM counts. When a machine has `autodetect = true` (or
some queue would otherwise resolve no `max-cpus-per-node`/`memory` value —
counting both the top-level `[hardware]` keys and the per-queue overrides,
§4.2), cactup fills the missing top-level values at load time from the OS.
A **CPU is a core** here as everywhere (§1.1), so a hyperthreaded node detects
as its core count with the SMT factor recorded separately — a thread count would
inflate every derived layout by it, and would move if the node rebooted with
hyperthreading off:

| Var | Linux | macOS |
|-----|-------|-------|
| `max-cpus-per-node` | `/proc/cpuinfo`, counted in **cores** — distinct `(physical id, core id)` pairs, not `processor` lines | `sysctl -n hw.physicalcpu` |
| `threads-per-cpu` | `/proc/cpuinfo` threads ÷ cores, when it divides evenly | `sysctl -n hw.logicalcpu` ÷ cores |
| `memory` (MB) | `/proc/meminfo` `MemTotal` | `sysctl -n hw.memsize` |
| `max-gpus-per-node` | `/proc/driver/nvidia/gpus`, amdkfd topology, or PCI class `0x0302` | not detected |

`threads-per-cpu` and `max-gpus-per-node` are filled *opportunistically* only:
neither joins the "would some queue resolve no value" trigger, since no SMT and
no GPUs is the ordinary case, and §8.5 already falls back to one thread per CPU
and one GPU per task. A kernel that reports no CPU topology at all (some VMs)
falls back to one core per thread and claims nothing about SMT.

This is the same detection simfactory's `CREATE_MACHINE` did at setup time
(`simfactory-docs.txt` §20), but done at runtime so the built-in `generic`
works on any laptop with zero configuration. Explicit `meta.toml` values always
win over autodetection. `generic` omits both `simulation-home` and
`install-home`, so it takes the `~/.cactup/simulations` and `~/.cactup/cacti`
fallbacks respectively (§8.1, §4.2).

### 4.7 `cactup machine create` / `delete`

Replaces simfactory's `sim setup`/`setup-silent` machine-creation path
(`simfactory-docs.txt` §20). Persists a tuned local machine into the **user
MDB** (§4.1) so it survives MDB updates and is auto-detected next time.

```
cactup machine create [<name>] [--from-existing [<base>]] [--silent] [--no-discover]
cactup machine delete <name>
```

- Copies a base machine into `~/.cactup/machines/<name>/`. The base is `generic`
  by default; `--from-existing <base>` clones the named machine as a starting
  point, and `--from-existing` with **no argument** clones the currently-detected
  machine (§4.3). This is the coarse-override path (D2): the whole machine
  directory is copied.
- **Override provenance & staleness warning (D2).** When a machine is created
  with `--from-existing`, cactup records the source machine name and a
  content hash of the source machine dir at clone time in the new machine's
  `meta.toml` (`[cactup.origin] from = "<base>"`, `hash = "…"`). Whenever that
  overriding machine is subsequently *used*, cactup re-hashes the current
  system-MDB source; if it differs, cactup prints a one-line warning that the
  upstream machine has changed since the override was taken (the override will
  **not** pick up the upstream change — coarse override is an accepted limitation;
  the user may re-create to refresh). If the source no longer exists, the warning
  says so instead.
- `<name>` defaults to the local hostname's short form.
- **Autodetects and writes concrete hardware** (`max-cpus-per-node`, `memory`
  via §4.6) so the persisted machine is stable rather than re-detecting each run.
- Sets `simulation-home`/`install-home` (prompted; defaults are the
  `~/.cactup/simulations` and `~/.cactup/cacti` fallbacks, §4.2) and prompts for
  `user`/`email`/`allocation` knobs unless `--silent` (which takes defaults — the
  successor to `setup-silent`).
- **Generates a `hostname.regexp`** claiming the current host (exact FQDN or
  short name, escaped) so the machine is auto-detected on subsequent runs,
  *and* sets the `detected` record for this host and session (§4.3). The
  base's own matcher files are never copied — a clone of `mike` must not be
  claimed by `^mike\d+`. Pass `--no-discover` to write no matcher at all: the
  machine is then only selectable via `--machine` (useful for cloning a remote
  cluster's def to inspect/edit locally).
- `machine delete <name>` removes a **user-MDB** machine (refuses to delete a
  system-MDB machine; suggest overriding instead).

**Install integration.** `cactup install` (existing) calls
`cactup machine create --silent` when the host is unrecognized — the direct
successor to its current `sim setup-silent` invocation (`src/main.rs`). This is
where the unprompted persist happens: at install time, an operation that is
already interactive and already writes files, rather than as a side effect of an
arbitrary later `sim`/`config` command.

> **Zero-match default behavior (open).** As specified (§4.3), a
> bare `sim`/`config` command on an unrecognized host that was *not* set up via
> `install` uses `generic` in-place (with autodetect) and merely *suggests*
> `machine create`; it does not silently write a machine file mid-command. An
> alternative policy — auto-persisting a local machine on first touch (zero
> friction, one unprompted write) — is a one-line change if preferred.

### 4.8 Universes (wrapped / containerized execution)

A **universe** lets a cactup-driven command run in a different execution context
than the invoking user's — the motivating case being **building and running
Cactus inside a Singularity/Apptainer image** rather than against the host
toolchain. The wiring is deliberately generic: a universe is nothing more than a
**command-wrapper** that cactup applies around a command it would otherwise run
directly.

**Scope: the three execution seams cactup owns.** A universe can attach at any of
the seams where *cactup itself* spawns a process:

- **build** — the `make` invocations of `cactup build` (§7.2).
- **run** — the runscript execution of `sim run` (§8.4), both the interactive path
  and the compute-node `sim run --restart-id` (§8.3.1). This is the meaningful
  case for containerized simulations.
- **submit** — the machine `submit` command, e.g. `sbatch @SCRIPTFILE@` (§10).
  Wired for generality, but rarely useful: `submit` normally just hands the job to
  the scheduler, which then runs `sim run` on a compute node — so the container
  you actually care about is the **run** universe, recorded at submit time and
  applied on the compute node (below), not a wrapper around `sbatch` itself. A
  submit universe only makes sense when the *scheduler client* lives in a context
  the login shell lacks (e.g. `sbatch` only inside an image, or an
  `ssh headnode …` wrapper).

The mechanism is phase-agnostic by construction — nothing below is
phase-specific except which seam consults the resolved universe. Discovery and
`.py` variant evaluation (cactup's own plumbing) are never wrapped.

**Registry (`meta.toml`, machine-level).** Universes are declared per machine
(images/contexts are machine-specific) and are user-MDB-overridable like anything
else in `meta.toml` (§4.1):

```toml
[universes.et-sif]
kind = "apptainer"                    # documentation only; cactup does not switch on it
wrapper-argv = ["apptainer", "exec", "--bind", "@SOURCEDIR@", "/work/@USER@/et.sif"]

# Template power-form (mutually exclusive with wrapper-argv):
# [universes.head-ssh]
# wrapper = "ssh headnode 'cd @SOURCEDIR@ && @COMMAND@'"

# A universe may ALSO (or ONLY) customize env-setup — see "env-setup runs
# inside the universe" below. With no wrapper key at all it is an identity
# universe: no wrapping happens, only the env override applies. This is the
# shape a machine uses to customize the implicit "host" universe (below)
# without introducing any wrapping:
# [universes.host]
# env-build-setup = """
# module purge
# module load gcc/9.3.0 cuda/12.4.0
# """
```

**Two representation forms, or neither:**

1. **`wrapper-argv` (prefix form — default, recommended).** An argv *prefix*.
   cactup runs `<wrapper-argv…> /bin/sh -c <inner>`, where `<inner>` is the
   env-setup-prepended build snippet (below). cactup owns the trailing
   `/bin/sh -c`, so there is **no quoting for the author to get wrong**. Covers
   `apptainer exec`, `docker run`, `chroot`, `env -i`, `numactl`, `taskset`, etc.
2. **`wrapper` (template form — power option).** A single shell-command string
   containing exactly one reserved **`@COMMAND@`** placeholder. cactup
   shell-quotes `<inner>` and substitutes it for `@COMMAND@`. Needed only when the
   command must not sit at the end (e.g. `ssh host 'cd … && @COMMAND@'`). The
   author owns any surrounding quoting.
3. **Neither (identity universe).** A universe may declare **neither**
   `wrapper-argv` nor `wrapper`. It then wraps nothing: cactup runs `<inner>`
   via a plain `/bin/sh -c <inner>` — exactly what every no-universe path does
   already. This is not a degenerate/error case; it is how a universe whose
   only job is the env-setup override below (typically `[universes.host]`,
   below) is declared. Declaring **both** `wrapper-argv` and `wrapper` remains
   an error.

Both forms (when present) are first `@NAME@`-substituted with the full §6.3
variable set (so `@SOURCEDIR@`, `@USER@`, `@SCRATCH_HOME@`, … resolve in bind
specs); `@COMMAND@` is reserved, filled **last**, and meaningful only in the
template form. It is an error for a universe to define both `wrapper-argv` and
`wrapper`, or for `wrapper` to omit `@COMMAND@`.

**A wrapper MUST propagate the wrapped command's exit status.** cactup decides a
build failed by the wrapper's exit code; a wrapper that exits 0 while the wrapped
command failed makes a failed build look successful, and only the §7.2
completeness backstop then catches it (and only for builds — a lying run wrapper
is not caught at all). This is a real hazard for **scheduler-fronting `wrapper`
scripts**: a site may replace `sbatch` with a shim that returns 0 even when
submission fails or the job dies (observed on LONI QB4, whose lua shim swallowed
both a submission error and a failed job's exit code). Do **not** trust such a
shim's rc — derive the outcome yourself: require a `Submitted batch job <id>`
match (no id ⇒ submission failed), and after `sbatch --wait` returns query the
scheduler for the job's real result (`sacct -j <id> --format=State,ExitCode`,
falling back to `scontrol show job <id>`) rather than the shim's exit code. The
`[universes.compute]` wrapper in `mdb/qbd/meta.toml` is the worked example.

**The implicit `host` universe.** `"host"` always exists as a universe name —
even on a machine whose `meta.toml` has no `[universes.host]` table at all —
as cactup's name for "the invoking context, unwrapped." Any reference to a
universe name (`[build].universe`, a variant's `universe`, `default-universe`,
or a variant's `build-universes` compatibility list, §4.4) may say `"host"` and it is
never an unknown-universe error, declared or not. A machine may *optionally*
declare `[universes.host]` to **customize** host — almost always to attach the
env-setup overrides below, since host is by definition the identity case and
so has no reason to also carry a wrapper (though it legally could). When
`[universes.host]` is undeclared, `Meta::universe("host")` resolves to a
built-in identity universe (no wrapper, no env overrides) — i.e. bare
execution, indistinguishable from "no universe at all" — which is why the
resolution precedence below only changes *materially* for machines that do
declare it (see step 5).

**`env-setup` runs *inside* the universe.** The `<inner>` snippet cactup wraps is
the phase's effective env-setup followed by the phase command — build:
`<ENV_SETUP>\n<make …>`; run: the substituted runscript (with its `ENV_SETUP`
already prepended for a `.sh` variant, §6.1); submit: the `submit` command — so
the phase's `env-<phase>-setup` module loads resolve against the universe's
environment (e.g. the container's modules), which is almost always what a
containerized phase wants. Authors needing host-then-universe ordering use a
`.py`-emitted script as usual.

**Universe-level env-setup overrides (§4.2).** A `[universes.<name>]` table
may itself carry `env-setup` / `env-build-setup` / `env-submit-setup` /
`env-run-setup`, on top of any wrapper keys — or, per the identity case above,
with no wrapper at all. Where set, a universe's key **replaces** the machine
`[environment]` key of the same name, **key-by-key**, for any phase executed
in that universe; a key the universe table omits still falls back to the
machine's `[environment]` value. The §6.1 concatenation that produces the
effective per-phase block (`env-setup` then `env-<phase>-setup`) is otherwise
unchanged — it simply reads whichever of the two (universe- or
machine-scoped) key is in force for each half. Concretely, the effective
env-setup for phase P in universe U is `(U.env-setup ?? machine.env-setup)`
followed by `(U.env-<P>-setup ?? machine.env-<P>-setup)`. The env block used
for a given phase always resolves against **the universe in effect for that
phase** — the same universe the resolution chain below picks — so a
build-phase env override, for instance, only ever applies while building.
This is what lets a machine give its native build the modules it needs
without a stray `module purge` in the machine-wide `env-setup` clobbering a
*different* universe's own loads (a real problem for a machine whose
container universes rely on the container's own module state): scope the
purge-and-load sequence to `[universes.host].env-build-setup` instead of
`[environment].env-setup`, and leave the machine-wide block minimal. See the
MDB porting guide's db1 write-up for the full worked example.

**Resolution precedence.** Each phase (build / run / submit) resolves its own
chain independently, highest first. `--no-universe` is a **true bare escape
hatch**: at any phase it always resolves to no universe at all, skipping every
step below — **even when the machine declares `[universes.host]`**.

1. CLI `--universe <name>` / `--no-universe` (on `cactup build`, `sim run`, or
   `sim submit`).
2. **(run only) Config build-universe coercion.** If the config being run was
   *built* in a universe (§7.4 records it) and the optionlist did **not** opt out
   (`[cactup].coerce-run-universe = false`, §7.8), the run defaults to **that same
   universe** — a container-built binary generally needs its build image at
   runtime (its dynamic libs, toolchain, MPI live there), so running it bare is
   usually broken. This coercion is inherited **by name**: cactup re-resolves the
   build universe's name against the current machine's `[universes.*]` and
   re-expands its wrapper with the **run** variable set, so run-specific binds
   (e.g. `@RUNDIR@`) are correct — it does not reuse the build-time expanded
   wrapper. It is deliberately stronger than the per-script/machine defaults below
   (only an explicit CLI flag or the optionlist opt-out escapes it). If a runscript
   variant *also* names a universe that differs from the coerced one, cactup uses
   the coerced (build) universe and notes the override under `-v`.
3. **Per-script association.** For **build**, the optionlist variant's
   `[cactup].universe` (§7.8). For **run** / **submit**, the selected script
   variant's `universe` field in `[variants.runscript]` / `[variants.submitscript]`
   (§4.4) — the inline-table variant form `"<v>" = { queues = [...], universe = "…" }`.
   This is where "an optionlist/script that only works in a given image names that
   image" lives.
4. **Machine phase default.** build: `[build].universe`; run/submit: the
   `default-universe` key of `[variants.runscript]` / `[variants.submitscript]`.
   Applies to any variant that does not name its own.
5. **The machine's declared `[universes.host]`, if present.** The final
   fallback of every chain is now the machine's own customization of `host`
   (above) — but only *materially* when the machine actually declares
   `[universes.host]`; this step exists so that a machine which has opted into
   a host override (e.g. build-phase env-setup scoping, above) gets it applied
   whenever nothing more specific named a universe, without requiring every
   optionlist/script/`[build]` entry to spell out `universe = "host"`. A
   machine that has **not** declared `[universes.host]` falls straight through
   to step 6 — on-disk metadata and behavior for every pre-existing machine
   are therefore unchanged.
6. None — run the phase in the invoking context, bare (today's behavior; and,
   per the `--no-universe` escape hatch above, always the outcome of
   `--no-universe` regardless of a declared host).

An unknown universe name is a hard error at resolution time (listing the
machine's known universes; `"host"` is always in that list, declared or not).
The build-universe coercion (step 2) can therefore fail if the config's build
universe no longer exists on this machine; the error names it and points at
`--no-universe` / a rebuild as the escape.

**Universe-compatibility routing is a separate mechanism from resolution.**
Everything above answers "which universe does this phase run *in*." A
different question — "which run/submit **script variant** does a config get
routed to, given the universe it was *built* in" — is answered by each
variant's optional `build-universes` compatibility list (§4.4), not by this
precedence chain: `sim run`/`sim submit` filter the candidate script variants
for the chosen queue down to those whose `build-universes` list (if set) includes
the config's build universe (default `"host"`), before applying the usual
queue → variant / `default = true` selection (§4.2). This is what lets one
machine ship, say, a native run/submit script and a Singularity run/submit
script over the **same** queue, distinguished purely by the build universe of
the config being run/submitted — replacing the older trick of minting a
synthetic queue per build flavor purely to multiplex script selection (the MDB
porting guide's db1 write-up documents exactly this collapse). `build-universes` is
orthogonal to the `universe` / `default-universe` keys used by this
resolution chain: a variant's `build-universes` list says what build universes it is
*compatible with* (a selection filter); its own `universe` key (if any, step
3) says what universe *its execution runs inside* (a wrapper choice) — a
variant may set either, both, or neither.

**Where the resolved universe is recorded, and when it is applied.**

- **build:** the resolved universe (name + expanded wrapper) is written to
  `cactup-config.toml` (§7.4) and applied immediately around the `make` snippet. A
  build produced in a universe is not interchangeable with one produced on the
  host, so it participates in the rebuild decision (§7.8 rule 5): changing the
  universe forces a full realclean + rebuild.
- **run:** resolved at `sim run` / `sim submit` time and recorded in
  `restart.toml` (§9.3). The interactive `sim run` applies it directly; for a
  submitted job the **compute-node** `sim run --restart-id` re-reads it from
  `restart.toml` and wraps the runscript there — satisfying the D11 rule that the
  compute-node path reads only on-disk metadata, not the global DB (§8.3.1). This
  is why the run universe is resolved on the login node at submit time (where the
  MDB/knobs/CLI are available) and frozen into the restart, not re-resolved on the
  compute node. Note that "resolved" here includes the build-universe coercion
  (step 2): a config built in a universe gets that universe frozen into every
  restart's `restart.toml` by default, so its simulations run in the build image
  even with no `--universe` flag and no runscript-variant universe.
- **submit:** resolved at `sim submit` time and applied around the `submit`
  command on the spot; nothing about it needs persisting (it affects only the
  one-shot `sbatch`-equivalent invocation).

**Known boundary (MPI + containers).** Wrapping the *whole* runscript in a run
universe runs the entire job in one context (correct for single-node / `generic`
and typical single-image workflows). Per-rank containerization
(`mpirun apptainer exec … cactus`, or a container-per-rank launcher) is a
runscript-authoring concern and stays inside the author's runscript — cactup's
universe wrapper is deliberately the coarse "whole command in a context" tool, not
an MPI-launch rewriter.

---

## 5. Knobs

Global default values, replacing simfactory's `defs.local.ini [default]`
section and the per-run `GetMachineOption` overrides.

```
cactup knob                       # print all knobs and values (standard, then custom)
cactup knob <name>                # print one knob
cactup knob <name> <value>        # set one knob
cactup knob -c <name> <value>     # create a CUSTOM knob (first time only)
cactup knob delete <name>         # delete a custom knob / unset a standard one
cactup -K <name>=<value> <cmd> …  # overlay a knob for one command (§5.1)
```

**Two kinds of knob share one map.** The **standard** knobs below always
exist and each has a `KnobSpec`. A **custom** knob is user-named: a
free-form value that exists to be read by `@KNOB(name)@` in parfiles,
scripts and optionlists (§6.1) — a path to initial data, a resolution tag,
anything a user would otherwise hand-edit into a parfile per machine. Knob
names (both kinds) are **kebab-case identifiers**: lowercase `a-z` only,
digits allowed after the first character, dashes allowed anywhere but first
or last (`validate_knob_name`). Creating a custom knob requires
`-c/--custom`; setting a name that is neither standard nor an existing custom
knob is an error naming the standard knobs and the `-c` form, so a typo can
never quietly mint a knob nothing reads. Once a custom knob exists it is set
like any other (no flag). `cactup knob delete <name>` **removes** a custom
knob (creating it again needs `-c`); on a standard knob — which always exists
— it only **unsets** the stored value, so the derived/built-in default applies
again. Deleting a name that is neither standard nor an existing custom knob
is an error, not a no-op. `delete` is a subcommand beside the positionals
(`args_conflicts_with_subcommands`, as `build` does), so it is a reserved
word no knob can be named (`validate_knob_name` rejects it, hence also
`-K delete=…` and `@KNOB(delete)@`). `cactup knob` lists the two kinds under
separate headings.

Recognized knobs (from `cactup-simfactory-design.txt`): `allocation`, `mail`,
`mail-type` (default `all`), `queue`. **ASSUMPTION:** also `account`-style
extras (`user`, `email`) are derived automatically (`$USER`, `git config
user.email`) the way simfactory's `setup` did, but can be overridden as knobs.

Additionally `wisdom-frequency` and `wisdom-kind` (§16). Unlike the free-form
knobs above, these have closed value sets: each knob is described by a
`KnobSpec` — a name plus a `validate` fn (checks + normalizes a user value
into the stored form; the set path rejects bad values with the list of valid
ones) and a `render` fn (stored form → display form). Free-form knobs use
identity fns; future knobs opt into validation by supplying their own.
`wisdom-frequency` accepts `off|rare|normal|chatty|always`, stores the
ordinal `0`–`4`, and is always rendered as the name (`cactup knob` shows
`normal`, `database.json` holds `"2"`). `wisdom-kind` accepts and stores
`relevant|all`.

Additionally three **maintenance** knobs for distribution (D14, §17):
`autoupdate` (`auto`, the default, `|notify|off`; read leniently — garbage
means `auto`), `update-url` (https base of the site serving `latest.json` —
plain `http://` only for a loopback host, `127.0.0.1`/`localhost`/`[::1]`,
since what it serves is installed unasked; trailing `/` stripped; default
`https://max-morris.github.io/Cactup`) and
`mdb-url` (git URL whose `mdb` branch is synced; default
`https://github.com/max-morris/Cactup.git`). Their `KnobSpec` has
`snapshot = false`: they configure the cactup installation, not a job, so
`knob_snapshot()` never freezes them into restart/build/test metadata.

The path knobs, one per `[paths]` key (`install-home`, `simulation-home`,
`test-home`, `scratch-home`, `build-cache-home`; §4.2), are maintenance knobs
too: an absolute path each (validated; read leniently, so a value that is not
one is no value), winning over the machine's value of that key for the user
who sets it. Where a job needs one, it is frozen like the machine's would be.

And maintenance knobs for the build cache (D15, §18): `build-cache`
(`off`, the default, `record`, `serve` or `audit`; read leniently — garbage
means `off`), `build-cache-relocate` (`yes`, the default, or `no`: §18.8) and
`build-cache-size` (§18.9); the store's root is the path knob
`build-cache-home` (below, §18.7). They are not in the snapshot either, but a build does
depend on them, on a compute node included: `prepare` resolves them and
freezes the result into the build attempt (§18.2). `KnobSpec::maintenance` is for exactly that kind of knob: one
that configures cactup rather than a value a job's templates read.

**Storage:** knobs live in the **global database** (`~/.cactup/database.json`)
as a single flat map — a `~/.cactup` lives on exactly one machine, so there is
nothing to key them by. This is consistent with D4 (the global DB holds global
cactup state).

```jsonc
// database.json (excerpt) — standard and custom knobs side by side
"knobs": { "allocation": "hpc_xxx", "mail": "me@lsu.edu", "mail-type": "all", "queue": "checkpt",
           "kadath-initial-data": "/work/me/ID/BHNS.info" }
```

### 5.1 Value precedence

For any value that can come from several places (mirrors `simfactory-docs.txt`
§6.2, adapted):

1. Explicit CLI flag (e.g. `-q/--queue`, `-a/--allocation`) — highest.
2. `-K name=value` overlay (this command only).
3. Knob.
4. Machine `meta.toml` value / default.
5. Built-in default.

**The `-K` overlay** is process-wide state installed by `main` from the parsed
globals (`database::set_knob_overrides`) and applied by `Db::read()` to every
snapshot it hands out — so *every* knob reader (topology resolution, identity,
the build path's own `Db::open()`, wisdom) sees it without threading a
parameter through — and never by `Db::update()`, which re-reads the disk
state inside the lock, so an override can never be persisted. Values are
validated at parse time exactly like `cactup knob` would (a standard knob's
`validate`, a custom knob's name rule); a `-K` may name a custom knob that
was never created, since nothing is stored. `cactup knob` marks overlaid
values as "(-K override, not stored)".

**The knob snapshot.** `Database::knob_snapshot()` is the effective knob map
as `@KNOB(name)@` sees it (§6.1): every standard knob that has a stored or
derived value, in *display* form (`wisdom-frequency` reads `normal`, not `2`),
plus every custom knob, overlay included. It is attached to the variable set
(`VarSet::set_knobs`) wherever one is assembled for submit/run/build/test,
and **frozen** into the corresponding metadata — `restart.toml`, `build.toml`,
`test.toml` `[knobs]` tables (§9.3) — so the compute node resolves
`@KNOB(…)@` from disk and never opens the DB (D11).

### 5.2 No `defs.ini` / `defs.local.ini`

simfactory's two-file config DB and its merge/layering engine
(`simfactory-docs.txt` §10) are **removed**. Their responsibilities are
redistributed:

- `user` / `email` / `allocation` / `queue` defaults → **knobs** (§5).
- `sync-sources` / `sync-parfiles` → gone (no sync, D1).
- `default-configuration-name` → cactup's **active config** per installation
  (§7).
- Per-machine overrides → edit `meta.toml` directly (the machine repo is local
  and editable) or set knobs.
- `enabled/disabled-thorns` → machine `meta.toml` (§7.5).

---

## 6. Templating / variable substitution

Per D7, cactup implements **literal `@NAME@` replacement plus two computed
token families**, `ENV` and `KNOB`, each in three forms:

| Token | Value |
|---|---|
| `@ENV(NAME)@` | environment variable `NAME`; unset or **empty** is a hard error, never an empty splice |
| `@ENV-OPTIONAL(NAME)@` | `NAME`, or the empty string when unset/empty |
| `@ENV-OPTIONAL(NAME, default)@` | `NAME`, or `default` when unset/empty |
| `@KNOB(name)@` | the knob `name` from the set's knob snapshot (§5.1); unset or empty is a hard error |
| `@KNOB-OPTIONAL(name)@` | the knob, or the empty string |
| `@KNOB-OPTIONAL(name, default)@` | the knob, or `default` |

`ENV` arguments are `UPPER_SNAKE`, `KNOB` arguments are kebab-case knob
identifiers (§5); blanks around the argument and default are tolerated; the
required forms reject a default (it would defeat them); the only suffix is
`-OPTIONAL`. The **default** is `"double-quoted"`, `'single-quoted'` — quotes
dropped, `\"`/`\'`/`\\` escape the quote or backslash, any other backslash is
literal — or a **bare** run of ASCII letters and digits; a bare default
containing anything else (a space, `/`, `_`, `.`) is an error that names the
offending character and says to quote it, and an empty default is spelled
`""`. `ENV` is read at substitution time — which always happens on the
machine in question (login node for a submit script or optionlist, compute
node for a run script or parfile). `KNOB` reads the snapshot attached to the
variable set; a set without one (MDB `[paths]`, ad-hoc sets) rejects every
`KNOB` token, optional or not, as "not available in this context" rather than
reading as unset.

`@ENV()@` is the mechanism for machine paths only the machine's environment
knows (TACC/LRZ-style hashed storage roots in `[paths]`, §4.2); because of
it, `[paths]` values are resolved at **use time** (`Meta::resolved_paths`),
not at MDB load, so entries for other machines still load and validate
everywhere. There is no expression evaluation and no ternary sugar. Any MDB
script that needs conditional logic (e.g. simfactory's
`@("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@`) is
rewritten as a Python `.py` variant.

**Submit-time check (`VarSet::check`).** `sim submit`/`sim run` dry-run a
`.par` parfile against the assembled variable set *before* creating the
restart directory: every substitution error — stray `@`, unknown variable,
malformed token, required `KNOB` unset — fails the submit on the spot with
nothing left behind, instead of a queued job dying hours later. The one
exemption is an unset required `@ENV(…)@`, which only the run-time
environment can judge. `.py` parfiles are not checked.

### 6.1 The two template kinds

1. **`.sh` shell templates** (submitscripts, runscripts) and **TOML values**
   (`meta.toml`, optionlists): cactup replaces every `@NAME@` token with the
   string value of `NAME` from the active variable set (§6.3). Unknown `@NAME@`
   tokens are an error (caught at substitution time) rather than silently
   leaking — this fixes the simfactory `@QEUEUE@` class of bug
   (`simfactory-docs.txt` §11). **Escape:** `@@` is the literal-`@`
   escape — the substitution engine collapses every `@@` to a single `@` and
   never scans the result for a token, so a parfile or script needing a literal
   `@` writes `@@`. This is applied in the same single left-to-right pass as token
   replacement (so `@@NAME@@` yields the literal `@NAME@`, not a substitution).
   A lone `@` that is neither part of `@@` nor a well-formed `@NAME@` token is an
   error, so accidental stray `@`s are still caught rather than passed through.

   **Comments are opaque.** A comment is copied through byte for byte: no
   token expands in it, `@@` there stays `@@`, and a lone `@` (an email
   address, a `@NAME@` mentioned in prose) is not an error. What a comment
   *is* depends on what will read the substituted text, so every caller
   names the file kind (`template::Syntax`) — the engine does not guess:
   - **Shell** (`.sh` submit/run/build-submit scripts, `[scheduler]` and
     `[build].make` command lines, a universe `wrapper` string): a `#` that
     begins a word — at the start of the text or after whitespace, `;`,
     `&`, `|` or `(` — outside `'…'`/`"…"` quotes and heredoc bodies, to the
     end of the line. A **scheduler directive is not a comment**: a line
     whose first non-blank character is a `#` glued straight to a word
     (`#SBATCH`, `#PBS`, `#$`) is substituted like code. A commented-out
     command (`#module load @X@`) has that same shape and is scanned too;
     the stray-`@` error on such a line says to write `# ` with a space.
     Backticks and `$(…)` are not modeled — a `#` after whitespace inside
     them is a comment to the shell as well, and a `#` inside double quotes
     is scanned as code, which errs toward substituting.
   - **Parfile** (`.par`, per the flesh's `par.peg`): a `#` outside a `"…"`
     string, to the end of the line. Inside a string, a `#` on any line
     after the string's first also starts a comment (the grammar's
     `stringcomment` — how `ActiveThorns` lists are annotated), ending at
     the end of the line or the closing quote, whichever comes first;
     backslash escapes the next character inside a string.
   - **Optionlist** (the rendered native `NAME = value` file): a `#`
     anywhere, to the end of the line — `setup_configuration.pl` strips
     `#.*` before it even splits the line, and there is no quoting to hide
     behind.
   - **Plain** (`[paths]` values, universe `wrapper-argv` words): no comment
     syntax; every byte is scanned.
   The newline ending a comment is code, so line counting is unaffected;
   substitution errors name the **line**, not a byte offset. The bundled
   MDB comments that had doubled their `@` to survive the old rule were
   un-doubled when this landed.
2. **`.py` script variants**: the escape hatch for conditional/computed logic.
   **Calling convention (interface contract for MDB authors):** cactup invokes
   `python3 <variant>.py` once, passing the entire variable set as a single JSON
   object on **stdin**. cactup prepends a small fixed preamble that reads stdin
   and binds every variable as a module global, so the author's code sees plain
   globals (`NODES`, `QUEUE`, `CHAINED_JOB_ID`, …). The script writes the final
   shell script to **stdout**; a non-zero exit aborts submit/run with the
   script's stderr surfaced. **Types:** every value is provided in two forms —
   the canonical string (e.g. `NODES == "4"`, matching `.sh` semantics) and, for
   numeric/boolean variables, a typed companion under a `typed` dict
   (`typed["NODES"] == 4`) so authors needn't re-parse. **Knobs:** the knob
   snapshot (§5.1) arrives as the `knobs` dict, and the preamble defines
   `knob(name)` / `knob(name, default)` mirroring `@KNOB(name)@` /
   `@KNOB-OPTIONAL(name, default)@` — the no-default form raises `CactupError`
   for an unset or empty knob, with the same message the token would produce.
   The JSON-on-stdin choice keeps values out of the process table and argv
   length limits.

   **Refusing the run.** A `.py` variant may `raise CactupError("…")` (the class
   is provided by the preamble) to reject the request outright: cactup prints the
   message as its own error and stops, so nothing is submitted and no restart
   directory is left behind. This is for a request the machine genuinely cannot
   serve — a scheduler rule the topology violates, a combination of variables the
   site rejects — which cactup cannot check itself because the rule lives in the
   script's own arithmetic. Any **other** exception is treated as a bug in the
   variant and reported with its full Python traceback, so a typo stays
   debuggable instead of masquerading as a site policy. Multi-line messages are
   preserved; say what to change, not just what is wrong.

**`env-setup` handling — the effective block and where it is injected.** For any
given phase (build/submit/run), the **effective env-setup** is the concatenation
of the machine's `env-setup` and that phase's optional `env-<phase>-setup`
(`env-build-setup` / `env-submit-setup` / `env-run-setup`, §4.2), in that order,
joined by a newline (an unset companion contributes nothing). The `ENV_SETUP`
variable (§6.3) always holds the **already-combined** effective block for the
current phase, so scripts and `.py` authors never see the split. Injection then
depends on the artifact:

- **Build (`cactup build`, §7.2):** the build has no submit/run *script* — cactup
  drives `make` directly. cactup sources the effective build env-setup
  (`env-setup` + `env-build-setup`) in the shell it spawns for the `make
  <config>-config` / `make <config>` / `make <config>-utils` invocations, so the
  compiler/MPI modules are loaded exactly as they will be at run time. (This is
  new relative to simfactory, which never applied `env-setup` to builds.)
- **`.sh` run templates:** cactup **auto-prepends** the effective run env-setup
  (`env-setup` + `env-run-setup`) to the generated script **right after the
  shebang line**, exactly as simfactory did for the base `env-setup`
  (`simfactory-docs.txt` §6.4, §18 `ExecuteCommand`) — a runscript carries no
  scheduler directive block, so there is nothing ahead of the shebang that
  insertion could disturb.
- **`.sh` submit templates:** cactup **auto-prepends** the effective submit
  env-setup (`env-setup` + `env-submit-setup`), but **not** necessarily right
  after the shebang. The insertion point is **after the leading run of
  `#`-comment and/or blank/whitespace-only lines** — i.e. immediately before
  the first substantive (non-comment, non-blank) line — rather than
  immediately after the shebang. That leading run is exactly the shebang plus
  any `#SBATCH`/`#PBS`/… scheduler directive block, so this keeps the
  directive block **intact and first in the file**: splicing an executable
  env-setup block in front of or inside it would make the scheduler silently
  ignore every directive after the split (schedulers require their directives
  to be an unbroken run at the top of the script). A blank line **inside** the
  directive block does not end the "header" — the header is the *longest*
  leading run of comment-or-blank lines — so a directive block with a blank
  line in the middle is not split by this rule. A script that is
  comment/blank all the way through (no substantive line at all) gets the
  env-setup block appended at the end instead. A scheduler-less submitscript
  (no directive lines at all — `generic`, mel5) reduces to "right after the
  shebang," identical to the runscript rule above.
- **`.py` submit/run variants:** cactup does **not** auto-prepend — the `.py`
  author has full control over the emitted script and is responsible for placing
  env-setup where they want it. cactup makes the combined value available as the
  global **`ENV_SETUP`** (string) — and, like every variable, as a literal
  `@ENV_SETUP@` token — so the author typically emits `print(ENV_SETUP)` near the
  top of the generated script. (This is the deliberate consequence of the `.py`
  variant being the "cactup gets out of the way" escape hatch.)

### 6.2 Substituted artifacts

- Optionlist (selected variant) → substituted and written into the config's
  build inputs (§7).
- SubmitScript (queue-selected variant) → substituted at submit time, written
  into the restart metadata, and handed to the scheduler.
- RunScript (queue-selected variant) → substituted at run time.
- Parfile → resolved at **run** time (when the topology and restart are known),
  in one of the two template kinds above — exactly like submit/run scripts, and
  chosen by the parfile's extension (§8.2):
  - A **`.par`** parfile is `@NAME@`-substituted with the full §6.3 variable set
    — `@ENV(…)@` read from the compute node's environment, `@KNOB(…)@` from the
    snapshot frozen in `restart.toml` at submit time (§5.1) — and written as
    the ready-to-run `<basename>.par`. (Literal `@` in a `.par` *value* is
    written `@@`, which the run-time substitution collapses to a single `@`;
    inside a `#` comment nothing is scanned, so a bare `@` there is fine —
    §6.1.)
    The parfile is also dry-run at submit time (`VarSet::check`, §6), so a
    required knob it names must be set — or overlaid with `-K` — for the
    submit to succeed.
  - A **`.py`** parfile is the escape hatch for computed/conditional parfiles
    (replacing simfactory's executable-`.rpar` mechanism, §8.4). It is invoked
    per the §6.1 `.py` calling convention — cactup runs `python3 <parfile>.py`
    once, passing the full §6.3 variable set as a single JSON object on stdin,
    with the fixed preamble binding every variable as a module global. Its stdout
    is written as `<basename>.par` and used **as-is**: cactup does **not**
    post-substitute it (the author already has every variable as a global —
    including the typed companions — and interpolates in Python). Unlike
    submit/run `.py` variants there is no `env-setup` to place; `ENV_SETUP` is a
    submit/run-script concern only.

### 6.3 The canonical variable set

cactup defines one variable namespace, used for both `.sh`/TOML `@NAME@` tokens
and `.py` globals. The **topology variables are named to match the §8.5 CLI
flags exactly** — this is the primary divergence from simfactory's names.

> **Breaking change from simfactory (intentional, accepted).** simfactory's
> proc-layout variable names are **removed and renamed**, not aliased. Existing
> run/submit scripts that referenced the old names must be updated when ported.
> Porter's map:
> `@NUM_PROCS@`→`@TASKS@`, `@NODE_PROCS@`→`@TASKS_PER_NODE@`,
> `@NUM_THREADS@`→`@CPUS_PER_TASK@`, `@PROCS@`/`@PROCS_REQUESTED@`/`@PPN_USED@`→
> removed (compute from `@TASKS@`/`@CPUS_PER_TASK@`/`@MAX_CPUS_PER_NODE@` if
> needed), `@PPN@`→`@MAX_CPUS_PER_NODE@`, `@NUM_THREADS@` (machine default) →
> `default-cpus-per-task`, `@NUM_SMT@`→`@THREADS_PER_CPU@`,
> `@CPUFREQ@`→removed (no script used it), `@SIMFACTORY@`→`@CACTUP@`.

**Topology (canonical — one variable per §8.5 flag):**
`NODES` (`-n`), `TASKS` (`-T`, total MPI ranks), `TASKS_PER_NODE` (`-t`/tpn),
`CPUS_PER_TASK` (`-c`/cpus), `GPU` (`-g`; `1`/`0`),
`GPUS_PER_TASK` (`-G`/gpus-per-task; always `0` when `GPU` is `0`), `ALLOCATION` (`-a`),
`QUEUE` (`-q`; the scheduler-facing name — the selected queue's `name` override
when set, else its `[queues.<q>]` key — §4.2), `MAIL` (`-m`), `MAIL_TYPE` (`-M`),
`JOB_NAME` (`-J` — not `-j`; see below),
`WALLTIME` (`-w`; the scheduler wall for **this job**, `(DD-)?HH:MM:SS`),
`STDOUT_FILE` (`-o`), `STDERR_FILE` (`-e`).

cactup exposes **two distinct walltimes** for a parfile to consume, so an author
can wire the *hard scheduler wall* and the *when-to-checkpoint hint* to different
Cactus parameters:

- **Hard wall** — the walltime cactup reserves with the scheduler. `WALLTIME`
  (canonical `(DD-)?HH:MM:SS`), plus the components `WALLTIME_HH`, `WALLTIME_MM`,
  `WALLTIME_SS`, `WALLTIME_SECONDS`, `WALLTIME_MINUTES`, `WALLTIME_HOURS`.
- **Checkpoint hint** — the hard wall minus the checkpoint buffer (§8.8), i.e. a
  suggested "it's time to checkpoint and wrap up" deadline. `CHECKPOINT_WALLTIME`
  (canonical form), plus `CHECKPOINT_WALLTIME_SECONDS`, `CHECKPOINT_WALLTIME_HOURS`.

Both are **exposed as substitution variables only**. cactup computes the numbers
but neither reads nor injects any Cactus termination parameter itself; the author
decides which variable feeds which parameter (§8.8). (These names are literal
`@NAME@` tokens — there is no `*`/wildcard form, per D7; where this doc writes
`WALLTIME_*` in prose it just means "that family of names.")

**Identity / paths:**
`SIMULATION_NAME`, `SHORT_SIMULATION_NAME`, `SIMULATION_ID`, `RESTART_ID`,
`RUNDIR` (the active restart dir), `SOURCEDIR` (Cactus root),
`EXECUTABLE` (absolute path to the simulation's frozen binary — its
`.cactup/exe` hard link, §8.1, **not** the live `<Cactus root>/exe/...`),
`PARFILE`, `SCRIPTFILE`, `CONFIGURATION`,
`SIM_HOME` (the per-alias simulation home, §8.1),
`SIMULATION_DIR` (absolute path to this simulation's dir — passed to the
compute-node re-invocation as `--sim-dir`, §8.3.1), `SCRATCH_HOME`,
`ALIAS` (installation alias — needed by the compute-node re-invocation, §8.3),
`CACTUP` (absolute path to the cactup binary, used by the submit template to
re-invoke `@CACTUP@ sim run …`; renamed from simfactory's `@SIMFACTORY@`).
For a dist build this is the **frozen** versioned copy
`~/.cactup/bin/cactup-<build>` (`freeze::frozen_cactup()`, created next to a
plain-file `bin/cactup` if missing), never the `bin/cactup` symlink, so a
queued job runs the exact build it was submitted with across a self-update
(D14, §17.2); a dev build uses `current_exe()`.

**Identity / machine:**
`MACHINE`, `HOSTNAME`, `USER`, `EMAIL`, `EXECHOST`, `JOB_ID`, `CHAINED_JOB_ID`.

**Machine-derived** (read from `meta.toml` — the **queue-effective** hardware
values for the job's queue (§4.2), available to scripts but not topology
flags): `MAX_CPUS_PER_NODE` (CPUs/cores available per node — an availability
fact, **not** MPI ranks; from `max-cpus-per-node`),
`MAX_GPUS_PER_NODE` (GPUs available per node; from `max-gpus-per-node`, and
**`0`** when undeclared — unlike CPUs, an absent GPU count means none/unknown
rather than one),
`MEMORY` (per-node MB), `THREADS_PER_CPU` (from `threads-per-cpu`, default 1;
§8.5 assumption), `ENV_SETUP` (the **effective** env-setup
block for the current phase — `env-setup` plus the phase's `env-<phase>-setup`,
already combined; auto-prepended for `.sh`, author-emitted for `.py` — §6.1).

**Build-time only** (optionlists/build): `MAKEJOBS`, `DEBUGGER`, `RUNDEBUG`.

**Build-submit-only** (§7.9; present only for `buildsubmitscript` variants and
the frozen var set a build attempt carries — never leaked into normal
sim/config substitution): `CONFIG_DIR` (the config's absolute on-disk
directory, `<Cactus root>/configs/<name>` — passed to the compute-node
re-invocation as `--config-dir`, the build analog of `SIMULATION_DIR`) and
`ATTEMPT_ID` (this build attempt's `%04d` id under `CONFIG_DIR/.cactup-builds/`
— passed as `--attempt-id`, the build analog of `RESTART_ID`). A queued
build otherwise reuses the **existing** §6.3 set rather than inventing
parallel names: the whole TOPOLOGY block (`QUEUE`, `WALLTIME`, `NODES`,
`TASKS`, `CPUS_PER_TASK`, `GPUS_PER_TASK`, `ALLOCATION`, `MAIL`, `MAIL_TYPE`,
`JOB_NAME`, `STDOUT_FILE`, `STDERR_FILE`) and the identity/machine block
(`MACHINE`, `HOSTNAME`, `USER`, `ALIAS`, `CACTUP`, `SOURCEDIR`,
`CONFIGURATION`, `SCRIPTFILE`, `JOB_ID`, `EXECHOST`, `MAX_CPUS_PER_NODE`,
`ENV_SETUP`, …) now reach a `buildsubmitscript` exactly as they reach a
`submitscript` — a submitted build is a scheduler job like any other, so it
gets the same job-shape and identity variables, just no `CHAINED_JOB_ID`
(builds never chain — §7.9) and none of the simulation-only names: no
`SIMULATION_NAME`/`SIMULATION_ID`/`SIMULATION_DIR`/`RUNDIR`/`RESTART_ID`/
`PARFILE`/`SIM_HOME`, and none of the testsuite-only group below either. This
mirrors the no-leak rule §11.9 states for test runs: each phase's variable set
is exactly what that phase's scripts can legitimately consume.

**Testsuite-only** (present only for `cactup test run`/`test submit` scripts and
test runs — never leaked into normal sim/config substitution;
§11.9): `TEST_HOME`, `TEST_DIR`, `TEST_NAME`, `RESULTS_ID`,
`TESTSUITE_RESULTS_DIR`, `TESTSUITE_SELECT`. (`CONFIGURATION` and `TASKS` are the
existing §6.3 variables a test runscript uses for `make @CONFIGURATION@-testsuite`
and `CCTK_TESTSUITE_RUN_PROCESSORS`.)

Dropped (no longer produced): the simfactory proc-layout names above, the bare
`SUBMITSCRIPT` machine key (variants replace it), and every remote/archive
variable.

---

## 7. Config subsystem

Local to the active installation. Replaces `sim-build` (`simfactory-docs.txt`
§16).

### 7.1 Commands

```
cactup build <name> [-f] [--thornlist P] [--variant V] [--universe U | --no-universe] [flags…]
                                    # foreground or queued, auto-selected — §7.9
cactup config show [<name>]
cactup config use <name>
cactup config delete <name>
cactup config delta [<name>]       # divergence from the last build (§3.3)
```

- `build`: builds (or rebuilds with `-f`) config `<name>` in the active
  installation, in the foreground or on the queue depending on the machine's
  MDB entry (§7.9); `build run`/`build submit` force either, and
  `build list`/`show`/`log`/`stop`/`prune` manage the resulting attempts. There
  is no `config build` — `build` is the whole command family, not a `config`
  subcommand. `--thornlist` defaults to the thornlist the config was last
  built from (§7.5), falling back to
  `<Cactus root>/thornlists/installation-default.th` for a fresh config.
  `--variant` selects the optionlist variant (required iff the machine has >1
  optionlist variant — §4.4).
  `--universe <U>` runs the build inside a declared universe (e.g. an Apptainer
  image), `--no-universe` forces the host context; both override the
  optionlist/machine defaults (§4.8).
  Build flags (`--debug`, `--optimize`, `--unsafe`, `--profile`, `--reconfig`,
  `--clean`, `--make-jobs`, …) carry over from simfactory's `GetConfigValue`
  precedence (`simfactory-docs.txt` §6.3, §16); their effective precedence is
  CLI > stored config metadata > default.
- `show` (no arg): list configs with `[built <date>]` / `[incomplete]` status
  (port of `list-configurations`).
- `show <name>`: print stored metadata (variant, thornlist, flags, build/config
  IDs, optionlist).
- `use <name>`: set the installation's active config.
- `delete <name>`: remove the config build and its metadata, and GC any now-orphaned
  `CACHE/exe/<build-id>` (§8.1). If `<name>` is the active config, the installation
  drops to the **null-config state** (below). If any simulations were built from
  `<name>`, cactup **warns** and lists them, then refuses unless `-f` is given
  (those sims keep working — they hold their own frozen `.cactup/exe` — but their
  `simulation.toml` `config-id` will point at a config that no longer has metadata;
  `-f` accepts that).

**Null-config state.** An installation has an **active config** only once one
has been built and selected. Before the first successful `cactup build`, or after
the last config is deleted, the installation is in the **null-config state**: its
`installation.toml` records no active config. Any command that needs a config
(`sim create`, and the implicit-create path of `sim submit`/`sim run` when
`--config` is omitted) fails fast in this state with guidance to build or select
one. `build` and `config show` remain available (that is how you leave the
state); the first build automatically becomes active — including a build that
went through the queue (§7.9), reconciled once the compute-node attempt
succeeds. The null-config state is
**only** reachable with zero configs — cactup never leaves an installation that
still has configs without an active one: deleting the active config while others
remain re-points the active config to the **most-recently-built** remaining config
(deterministic, no prompt) and reports the switch. Only when the last config is
deleted does the installation fall to null-config.

### 7.2 On-disk build layout (unchanged from simfactory)

Cactus's own build system is untouched: configs live under
`<Cactus root>/configs/<name>/` with `bindings/ build/ config-data/ lib/
scratch/`, `config-data/cctk_Config.h` (presence ⇒ complete), and the
`make <config>` / `make <config>-config` flow exactly as `simfactory-docs.txt`
§16. cactup drives `make` the same way (`echo yes | make <config>-config
options=<OptionList>`, then `make <config>`, `make <config>-utils`; a `VERSION:`
line change in the optionlist forces a full rebuild). Each `make` runs in a shell
that has first sourced the effective **build** env-setup (`env-setup` +
`env-build-setup`, §4.2/§6.1), so the build sees the same compiler/MPI modules
the run will. When a **universe** is resolved for the build (§4.8), that whole
env-setup-plus-`make` shell snippet is what cactup wraps — i.e. the build runs
inside the universe (container, chroot, …), with env-setup applied inside it.

**Build output lives per attempt, not per config.** There is no
`configs/<name>/cactup-build.log`: every `make` invocation — foreground or
queued — runs under a **build attempt** directory,
`configs/<name>/.cactup-builds/%04d/` (§7.9), and its stdout/stderr land at
that attempt's `build.out`/`build.err`. `cactup build show`/`build log` (§7.9)
locate the right attempt automatically; there is nothing to `tail` by hand.

### 7.3 What changed in build

- **Submit/Run scripts are no longer baked into the config** at build time.
  simfactory copied a `SubmitScript`/`RunScript` into `configs/<name>/`; cactup
  resolves the correct *variant* at submit/run time based on the chosen queue
  (§4.4). The config only owns the **optionlist variant** (which genuinely
  affects the binary). This removes `remove-submitscript` and the
  `--no-submitscript` dance.
- The OptionList variant is chosen via `--variant` and recorded in metadata.

### 7.4 Config metadata (per-installation on-disk TOML — D6)

Stored next to the build, **not** in the global DB:

```
<Cactus root>/configs/<name>/cactup-config.toml
```

```toml
schema = 1
name = "sim-gpu"
variant = "gpu"                 # which optionlist this config is built from: EXACTLY ONE of
# optionlist = "/home/me/my.cfg"  # `variant` (an MDB variant) or `optionlist` (the file a
                                 # --optionlist build came from), never both and never
                                 # neither, and the one a bare rebuild uses (§7.8)
gpu = true                      # copied from the optionlist [cactup].gpu at build (D12)
compatible-queues = ["gpu"]     # copied from the optionlist [cactup].compatible-queues (D12)
thornlist = "thornlists/installation-default.th"
universe = "et-sif"             # resolved build universe; omitted when built bare/in host —
                                 # treated as "host" wherever a build universe is consulted
                                 # (script-variant `build-universes` filtering, §4.4; run coercion, §4.8)
coerce-run-universe = true      # snapshotted from optionlist [cactup]; default true (§4.8, §7.8)
config-id = "…"                 # replaces CONFIG-ID
build-id = "…"                  # replaces BUILD-ID
[flags]
debug = false
optimize = true
unsafe = false
profile = false
```

`gpu` and `compatible-queues` are snapshotted from the chosen optionlist
variant's `[cactup]` header at build time (§7.8) so that `sim submit` can
enforce queue compatibility (§4.4) without re-reading the MDB. The machine the
config was built on is recorded too (`machine = "<name>"`; a build is not
portable across machines) and **enforced**: `build` of an existing config,
`sim create`/`submit`/`run` and `test run`/`submit` refuse a config whose
recorded machine is not the resolved one (`config "sim" was built for machine
"mike" but this is machine "qbd"`), and `sim submit`/`run` likewise refuse a
simulation whose own `machine` (§9.3) is not. `--ignore-machine` — implied by
each command's `-f` — turns the refusal into a note and proceeds at the user's
own risk; for `build` that means a forced full rebuild whose metadata then
names the current machine. The resolved build **`universe`** (§4.8) is recorded
so `config show` reports it, the rebuild decision (below) can detect a universe
change, and — unless the optionlist opted out — `sim run`/`sim submit` can coerce
simulations of this config into the same universe (§4.8 precedence step 2); it is
omitted when the build ran in the host context. **`coerce-run-universe`** is
snapshotted from the optionlist `[cactup]` header (§7.8; default `true`) and
governs that coercion. Only the universe **name** is inherited for the run — the
run-time wrapper is re-resolved and re-expanded from the current MDB (§4.8).

**Rebuild-decision snapshot.** At build time cactup also copies the chosen
**source optionlist TOML** verbatim to `configs/<name>/cactup-optionlist.toml`,
and the chosen **source thornlist** verbatim to
`configs/<name>/cactup-thornlist.src.th` (§7.5).
The rebuild decision (§7.8 rule 5) diffs the freshly-selected source TOML against
this stored copy; *any* difference triggers a full realclean + reconfigure +
rebuild. This is the source-of-truth for "did the optionlist change?" — the
rendered native file is never diffed (it exists only for the Cactus build system
to consume, §7.8). The recorded **`universe`** is compared the same way (its value
can come from CLI/machine default, not just the optionlist TOML, so it is checked
separately): a resolved universe that differs from the stored one also forces a
full realclean + rebuild, since a host build and an in-container build are not
interchangeable (§4.8).

**Source-tree tracking.** Each build also records an optional **`sources`**
map: repo name → a compact *state string* for the tree that repo checked out,
covering its HEAD commit plus a summary of the tracked files that differ from
it (`<n>mod@<newest-mtime-nanos>`, mtime-based exactly like the `make` that
consumes those files, so a second edit is distinguishable from the first).
Untracked files are excluded deliberately: the test harness leaves output
inside the source tree, and that must not read as a source change. Only repos
the config's own processed thornlist names appear, so a config built from a
narrow list is not invalidated by a repo it does not compile.

This is a **live reading** of the tree at build time, not a replay of
`fetch-state.toml` (§3.2): editing a thorn in place and rebuilding is an
ordinary workflow, and so is a `git checkout` inside a repo, and neither moves
anything the fetch recorded. It is what lets the rebuild decision (§7.8 rule 5)
notice that sources moved under a config — without it a refetch that
fast-forwards 81 repos leaves every config reading up-to-date and silently
never gets compiled.

Comparing the stored map against a fresh reading is **asymmetric, in both
directions deliberately**:

- A repo in the fresh reading that the **stored map** does not know about is
  *not* a change. The first build after source tracking landed, and any config
  whose thornlist just gained a thorn, would otherwise report every repo as
  new — and a thornlist that gained or dropped a thorn is already caught by the
  processed-thornlist diff, so nothing is lost.
- A repo the **stored map knows about** that the live tree can no longer
  produce a state string for **is** a change, ranking with a moved commit
  (and earning a realclean when it is the flesh). Two ways to get there: the
  repo directory is gone, or it is no longer a git repo — the hand-built
  variant swapped in for a checkout again. There is no state string to compare
  precisely *because* the source stopped being identifiable, which is the
  strongest possible reason to rebuild, not a reason to skip the repo. A
  reading in which not one repo is readable stays "no information" as below,
  since recording an empty map as a config's baseline would make every later
  comparison find nothing to compare and read as unchanged forever.

**Per-thorn build-state tracking.** Two more optional maps are recorded each
build, both reading absence as "no information", never as "unchanged" — same
convention as `sources` above, and absent for a config built before each map
landed:

- **`thorn-providers`** — thorn name → providing directory, from the
  processed thornlist. Cactus keys `configs/<name>/build/<Thorn>/` and
  `libthorn_<Thorn>.a` by thorn **name** only, never by arrangement, so
  swapping which arrangement provides a name (disabling
  `EinsteinAnalysis/Foo`, enabling `SpacetimeX/Foo`) would otherwise silently
  reuse build state compiled from the other source tree.
- **`thorn-shapes`** — thorn name → a fingerprint hash. Covers the providing
  directory; the symlink target it resolves to and the backing repo's
  normalized `origin` URL (so re-pointing a repo at a fork invalidates that
  repo's thorns, while a mere URL-spelling change does not); the sorted list
  of file paths in the thorn (top-level plus everything under `src/`); and
  the contents of its `*.ccl` files and `make.code.defn` /
  `make.configuration.defn` / `make.code.deps`. It deliberately excludes the
  contents of ordinary source files: `make`'s own `.d` dependency tracking
  already gets body-code edits right, and hashing bodies would discard a
  thorn's whole build directory on every such edit — the opposite of what
  this tracking is for.

A thorn whose recorded provider or shape has moved since the last build has
its `build/<Thorn>/` and `libthorn_<Thorn>.a` deleted before the reconfigure
(§7.8) — these stored maps are what `cactup build` diffs to find it.

The installation's **active config** is recorded per-installation in
`<installation home>/.cactup/installation.toml` (§8.1), not the global DB —
consistent with D6. (The global DB tracks *which installation* is active; the
installation tracks *which config* is active, and its sim-home — §8.1.)

### 7.5 Thornlist & machine thorn toggles (D8)

Port of `GetThornListContents` (`simfactory-docs.txt` §16). The thornlist file
is processed at build time: each thorn named in the machine's
`disabled-thorns` gets a `#DISABLED ` prefix; `enabled-thorns` removes such a
prefix. The machine arrays come from `meta.toml` (§4.2). This lets a cluster
that can't build a given thorn opt it out without editing the shared thornlist.

**The override is announced.** When the source thornlist *actively enables* a
thorn (an uncommented thorn line — a line the list already carries as
`#DISABLED` is not a conflict) that a `disabled-thorns` array then switches
back off, `build prepare` prints a loud yellow warning naming each such thorn,
the `disabled-thorns` entry that matched it, and which layer that entry came
from: the machine (§7.5) or the selected optionlist variant (§7.8). Without it
the drop is completely silent — the user asked for the thorn, the processed
list quietly omits it, and the absence only surfaces much later as a missing
thorn at runtime. The warning is informational; it never fails the build.

**Two artifacts per config.** The processed text is written to
`configs/<name>/cactup-thornlist.th` and handed to Cactus as `THORNLIST=`, which
Cactus copies to its own `configs/<name>/ThornList` — the file its make rules
actually consume. Both are *derived*: they are rewritten on every build, so the
file to edit is always the **source** thornlist. Alongside the processed copy,
the source is snapshotted verbatim to `configs/<name>/cactup-thornlist.src.th`.

**Source resolution**, in order:

1. `--thornlist PATH` — explicit; unreadable is a hard error.
2. the path recorded in the config's metadata (`thornlist`), when still readable.
3. that config's `cactup-thornlist.src.th` snapshot, when the recorded path has
   moved or been deleted — with a warning, and the original path stays recorded.
4. `<Cactus root>/thornlists/installation-default.th`, for a fresh config only.

Step 2 means `--thornlist` does not have to be repeated on every rebuild: a
config built from a custom thornlist never silently reverts to the stock
Einstein Toolkit list. It prefers the live file over the snapshot deliberately —
editing the thornlist in place is the normal way to add a thorn, and that edit
must be picked up. Step 3 makes the snapshot the safety net rather than a
second source of truth. If both are gone, the build refuses to guess and says
to pass `--thornlist`.

**Interaction with `installation refetch` (§3.2).** A refetch updates *source
trees*, which the rebuild decision (§7.8 rule 5) never inspects — it diffs only
optionlist text, universe, and processed-thornlist text. So a refetch alone
leaves every config `UpToDate` and a plain `cactup build` runs no `make`.
Refetch therefore warns per existing config, naming `cactup build <name> -f` as
the follow-up (after `--release`, `-f` outright — a release bump likely stales
`config-data/`, and §7.4 reserves realclean for optionlist/universe changes).
Configs recorded against a custom `--thornlist` path additionally keep building
their own list — refetch never touches that file; the warning says so. The
refetch post-pass records per-repo fetched HEADs in
`<installation home>/.cactup/fetch-state.toml`; wiring those into the rebuild
decision is a planned follow-up (TODO in `_impl_fetch.md`), which will
retire the warning.

### 7.6 Build precedence & flags

Carried over verbatim from `simfactory-docs.txt` §6.3 / §16, substituting
"stored config metadata TOML" for "configs/<name>/properties.ini" and "machine
`meta.toml`" for "machine ini". `MAKEJOBS` = `--make-jobs` > machine `make-jobs` >
1; parallelism flows only through `@MAKEJOBS@` in the machine `make` command.
When a machine omits `[build].make`, the default is `make -j@MAKEJOBS@` (not a
bare `make`), so `make-jobs` is honored as the default `-j` without every
machine having to hand-write the token. `--make-jobs max` (`-j max`) sets
`@MAKEJOBS@` to a shell `$(nproc 2>/dev/null || echo 1)`, evaluated by the build
shell inside the resolved universe — so it uses every thread available in that
build context (an srun/singularity allocation, a container's cpuset, or the
local host), not the login node's; the `|| echo 1` keeps a missing `nproc` from
degenerating into a bare `make -j` (unbounded parallelism). It only takes effect
where `@MAKEJOBS@` is referenced (the default make command and any machine that
templates it).

### 7.7 Virtual / prebuilt executables

**ASSUMPTION:** keep `--virtual`/`--virtual-executable` (copy a prebuilt
`cactus_<config>` into place, skip configure/make) — it is cheap and used for
benchmarking. Flag if you'd rather drop it.

### 7.8 Optionlist TOML format & render step (D9)

simfactory optionlists are native Cactus `NAME = value` files (`#`-comments,
first line a `VERSION` date), consumed directly by
`make <config>-config options=<file>`. cactup authors optionlists as **TOML**
and **renders** them to that native format before invoking make.

**TOML schema** (`mdb/<m>/optionlists/<variant>.toml`):

```toml
[cactup]                         # cactup-only metadata; NOT emitted to the native file
gpu = true                       # binary capability (D12)
compatible-queues = ["gpu"]      # which queues this build may be submitted to (D12)
default = false                  # optional, default false: marks the implicit choice when the
                                 # machine lists several optionlist variants (§4.4)
universe = "et-sif"              # optional: build this variant inside this universe (§4.8)
coerce-run-universe = true       # optional, default true: sims of this config run in `universe` too (§4.8);
                                 # set false to opt out (run resolves normally, no build-universe inheritance)

[options]                        # rendered to native NAME = value lines
VERSION = "2024-06-01"           # first emitted line; a change forces full rebuild
CPP = "cpp"
CC  = "gcc"
CFLAGS = "-O2 -g @SOME_TEMPLATED_VALUE@"
# … one key per native option …
```

**Render rules (deterministic; the rendered native file is fed to Cactus only —
it is never diffed for the rebuild decision):**

1. Only the `[options]` table is rendered. `[cactup]` is stripped (`gpu`,
   `compatible-queues`, and `coerce-run-universe` snapshotted verbatim into the
   config metadata, §7.4; `[cactup].universe` feeds universe resolution —
   precedence in §4.8 — and the *resolved* value is what lands in the metadata).
2. Emit `VERSION` first, then the remaining keys in **TOML document order**
   (`toml` preserves order). Each key → `KEY = value`.
3. **Scalar → native value mapping, applied to every `[options]` value:**
   - **string** → its inner text, unquoted (the common case; e.g. `"yes"` →
     `yes`). Whitespace/`#` inside a quoted string is preserved verbatim.
   - **boolean** → `true` renders `yes`, `false` renders `no` (Cactus's
     convention — this is why the mel5 port keeps yes/no as strings *or* may use
     bools interchangeably).
   - **integer** → its plain decimal text.
   - **float** → **rejected at load** with an error (Cactus options are never
     floats; a float almost always means a mis-typed version/flag, and float
     formatting is not reliably round-trippable). Authors needing a dotted value
     quote it as a string.
4. `@NAME@` templating (§6) is applied to values **after** render, before make,
   so build-context variables (`@MAKEJOBS@`, `@USER@`, build flags) resolve.
5. The DEBUG/OPTIMISE/UNSAFE/PROFILE build-flag injection
   (`simfactory-docs.txt` §16: `AddReplacement` + substitution) is applied to the
   rendered native text exactly as simfactory did. **Spelling note:** cactup's
   CLI flag (`--optimize`) and config-metadata key (`optimize`) use American
   spelling, but the Cactus optionlist key they drive is `OPTIMISE` (likewise
   `VECTORISE`, `*_OPTIMISE_FLAGS`). Those are external Cactus build-system
   identifiers and are emitted **verbatim** — cactup maps `optimize` → `OPTIMISE`
   at render. Never Americanize keys inside `[options]`.

**User-supplied optionlists (`cactup build --optionlist PATH`).** A build may be
made from a file outside the MDB entirely. The porting loop (edit a `.cfg`,
rebuild, read the error, repeat) and the one-off experiment both want an
optionlist that does not yet deserve to be a machine variant, and requiring one
to be installed into `mdb/<m>/optionlists/` first turns a two-minute iteration
into an MDB edit. `--optionlist` displaces variant selection completely and is
therefore mutually exclusive with `--variant`. Three spellings are accepted,
auto-detected, and never intermixed within one file:

1. **The MDB shape above** — a `[cactup]` table plus an `[options]` table. The
   `[cactup]` keys are honored exactly as if the file sat in the MDB: `gpu` and
   `compatible-queues` feed the D12 queue cross-check (§4.4), `universe` feeds
   §4.8 resolution, and `enabled-thorns`/`disabled-thorns` apply on top of the
   machine's. (`default` is meaningless here — the file was named explicitly —
   and is ignored.)
2. **The `[options]` table alone**, with or without its header line. There is no
   `[cactup]` table, so the header defaults apply: no `gpu` claim, and an empty
   `compatible-queues`, i.e. no queue restriction at all.
3. **A native Cactus `.cfg`** — `NAME = value` lines with `#` comments, the
   format simfactory consumed directly and the format every existing optionlist
   in the wild is already written in. Header defaults as in (2). Every value is
   taken as text and rendered back verbatim, so `DEBUG = no` stays `no` rather
   than round-tripping through a TOML boolean; duplicate keys — which real
   `.cfg` files carry and TOML forbids — resolve last-wins at the key's first
   position.

**Detection.** Forms 1 and 2 are TOML and form 3 is not, and the discriminator
is quoting: a TOML string value is quoted, a `.cfg`'s is bare. Each
`NAME = value` line is classified by its right-hand side — quoted, a bracketed
array, or a bare `true`/`false` (TOML's boolean literals; Cactus spells these
`yes`/`no`, so they can only mean TOML) ⇒ TOML; a bare integer ⇒ neutral, since
it means and renders the same in either family and so discriminates nothing;
anything else, an empty value included ⇒ `.cfg`. A file carrying both a TOML
value and a `.cfg` value is **rejected**, naming both offending lines:
intermixing is an authoring mistake, and guessing which half was meant would
silently build the wrong binary. A `[cactup]` table decides form 1; an
`[options]` header, or any quoted value, decides form 2; any other table header
is an error, as is a file that declares no options at all.

The config metadata (§7.4) records **which optionlist, of one kind or the
other** — a `variant = "cuda"` naming an MDB variant, or an `optionlist =
"/abs/path"` naming a user file, never both and never neither. The two are
alternatives, not a fallback chain, because the flags that set them are mutually
exclusive; the metadata is modeled as a sum so a config that is somehow both
cannot be written down. The recorded path is canonicalized, so `config show`
names the file the build came from wherever a later rebuild runs. The rebuild
trigger's first input — the source snapshot — is the user's file text verbatim,
exactly as it is for an MDB variant, so editing that file forces the same full
rebuild an MDB optionlist edit does.

**Stickiness.** A later bare `cactup build` rebuilds from whichever the config
records — the flag does not have to be repeated, exactly as `--thornlist` does
not (§7.5). This is true of `--variant` too: **all three** of the flags that say
what a config is made of are remembered, so there is no rule to learn about
which ones are. The resolution order is:

1. `--optionlist PATH` — explicit; a hard error if unreadable.
2. `--variant NAME` — explicit; how a config is deliberately moved onto, or
   between, the machine's own variants.
3. whichever of the two the config already records, since it records exactly
   one:
   - a **variant**, when the machine still lists it. When the machine has since
     renamed or dropped it, this is a **hard error** naming the missing variant
     and listing what the machine now offers — there is nothing to fall back on,
     because a snapshot records the text one build used, not a standing
     definition of a variant.
   - the **`--optionlist` path**, falling back to that config's verbatim
     snapshot when the path has since moved or been deleted, so the config stays
     rebuildable and the file going away cannot quietly change what gets built.
     Preferring the live file is deliberate: editing an optionlist in place and
     rebuilding is the whole point of naming one.
4. the machine's own variant selection (§4.4).

Steps 1 and 2 are alternatives, and so are the two halves of step 3. Supplying
either flag **displaces** what the config was on record as being — `--variant`
on a config built from a file moves it onto the MDB, `--optionlist` on a config
built from a variant moves it off. Whichever flag was passed most recently is
what sticks, in both directions; neither ever falls back to the other.

This replaces an earlier guard that refused a bare rebuild whenever the variant
a config recorded differed from what the machine would now resolve to. The guard
was there to stop a bare rebuild silently building a different flavor; step 3
stops that by construction instead, by rebuilding what the config actually
records, which is what the user meant both times.

**Rebuild trigger.** The decision to rebuild diffs five inputs against what the
config was last built with:

1. the freshly-selected **source optionlist TOML** against the copy stored at
   build time (`configs/<name>/cactup-optionlist.toml`, §7.4);
2. the resolved build **universe** against the recorded one (§7.4);
3. the **processed thornlist** against the stored
   `configs/<name>/cactup-thornlist.th` (§7.5);
4. each thorn's recorded **provider** and **shape** (§7.4) against the stored
   `thorn-providers`/`thorn-shapes` maps — independent of the thornlist-text
   diff above, since a thornlist can be byte-for-byte unchanged while what a
   name resolves to underneath it, or what that thorn contains, is not;
5. the live **source-tree state** against the stored `sources` map (§7.4) — a
   refetch, a manual `git checkout` inside a repo, a hand-edited thorn, or a
   repo that stopped being identifiable at all. None of the text diffs above
   can see any of it: every one of them leaves the thornlist byte-identical.

An optionlist or universe difference — a changed flag, a new key, a bumped
`VERSION` — triggers a full `make <config>-realclean` + reconfigure + rebuild.
(So a `VERSION` bump forces a rebuild only *because* it is a diff; there is no
separate VERSION-only path.) The rendered native file plays no part in the
optionlist comparison and its comments — which TOML drops on parse — are
irrelevant, since nothing diffs it. This collapses simfactory's finer "VERSION →
realclean vs. other change → reconfigure-only" distinction into "any change →
full rebuild": simpler and always safe, at the cost of a from-scratch rebuild on
every optionlist edit.

A thornlist difference triggers a **reconfigure + `make`, without the
realclean**. Unlike an optionlist edit it does not invalidate already-compiled
objects — it changes *which* thorns are in the build, not how the code compiles —
and Cactus regenerates the bindings itself from the `configs/<name>/ThornList`
the reconfigure step copies into place. Adding a thorn is routine, so charging a
from-scratch rebuild for it would be a poor trade; `-f` still forces one.
Diffing the *processed* text (not the source) makes this one comparison cover a
source-thornlist edit, a switch to a different thornlist file, and a change to
the machine's or variant's `enabled-thorns`/`disabled-thorns` — none of which the
optionlist diff can see.

A thorn whose provider or shape differs also triggers a reconfigure + `make`
without the realclean, but scoped to that thorn alone: `build/<Thorn>/` and
`libthorn_<Thorn>.a` are deleted before make runs, not the whole config. A
provider change means the name now resolves to a different arrangement
entirely; a shape change means a file was added or removed, or one of the
thorn's `.ccl`/`make.code.defn`/`make.configuration.defn`/`make.code.deps`
was edited — either way, make's own dependency tracking cannot be trusted to
notice on its own (a stale `.d` can still name a bindings header Cactus's
configure step has since deleted, and `ar` updates `libthorn_*.a` in place,
so a removed source's orphaned `.o` keeps linking in). Ordinary edits to a
thorn's existing source-file bodies do **not** appear in either map and so
never trigger this: `make` recompiles what such an edit affects on its own,
the same trust extended to an edited flesh above.

A **source-tree** difference is graded by what moved. A repo on a different
commit, or one that can no longer be read at all, triggers a reconfigure +
`make`; the same for the **flesh** triggers a full realclean + rebuild, since
the flesh is the make system and everything `config-data/cctk_Config.h` is
generated from, so every existing object is suspect. A repo whose *worktree*
alone differs — sources edited in place — never escalates past a reconfigure,
the flesh included: `make`'s own dependency tracking decides what such an edit
costs, and charging a from-scratch rebuild to anyone iterating on flesh code
would be hostile.

Only when all five inputs match does a complete config short-circuit as up to
date. The baseline for all five is recorded on **every** build, including under
`-f`: a config that always rebuilds with `-f` could otherwise never acquire one
to diff a later plain rebuild against.

---

### 7.9 Queued builds

Some clusters forbid `make` on a login node — the flesh has to be compiled on a
compute node, exactly like a simulation. Before this section, cactup's answer
was to abuse a **universe** (§4.8) for it: wrap the whole build-env-setup-plus-
`make` snippet in a shell fragment that `sbatch`'d itself, `tail -f`'d its own
log, and scraped `sacct`/`scontrol` for a truthful exit status because the
site's `sbatch` wrapper returned 0 even when the job died. That is the wrong
tool for the job, and the shape of the wrongness is worth stating precisely: **a
universe describes *where* a command executes** — a container, a chroot, a
module-loaded shell — **while a scheduler describes *how* work reaches a
machine** — queued, given a job id, polled for status, eventually run somewhere
the caller doesn't control the timing of. Conflating them is what forced the old
wrapper to hand-roll job-id parsing, log tailing, and exit-status classification
in `sh`, all of which `Scheduler` (§10) and `src/tail/` already do correctly for
every other job cactup submits. Once building is a first-class job like any
other, none of that reimplementation is needed: the compute-node process *is*
cactup, so it runs `make`, checks completeness itself, and records the outcome
directly — nothing has to trust a wrapper's relayed exit code or scrape a
second command to find out what really happened.

**Command surface.** `cactup build [<name>]` auto-selects between running in
the foreground and submitting to the queue; `cactup build run` and `cactup
build submit` force either explicitly. `build list`, `build show`, `build log`,
`build stop`, and `build prune` monitor and manage the resulting **build
attempts** the same way their `sim` equivalents (§8.6, §8.7) monitor a
simulation — full command shapes are in §3. `<name>` defaults to the active
config everywhere, matching `config show`/`config delta` (§3.3).

**Blocking submission (`--block`).** `cactup build submit --block` (and a bare
`cactup build` that auto-selects submit) waits for the queued build to finish
instead of returning the moment the job is queued. There are two routes. When
the machine declares the optional `[scheduler].blocking-submit` key (§4.2,
§10) — the exact analog of `submit`, but expressed so the command returns
only once the job has *finished* (`sbatch --wait @SCRIPTFILE@` where `submit`
is plain `sbatch @SCRIPTFILE@`; `qsub -W block=true @SCRIPTFILE@` on PBS Pro;
on the generic/workstation machines whose `submit` backgrounds the script and
echoes `$!`, the same thing plus `wait $pid`) — `--block` runs that instead.
Its output is scanned with the same `submit-pattern` line by line as it
arrives, rather than once at the end, so the job id is recorded while the job
is still running: a blocking submit can sit there for hours, and a Ctrl-C or a
crash must not leave a real queued job with nothing on disk naming it. How
early the id actually appears is the submit command's own business — one that
prints it and only then blocks holds it in its stdio buffer until it exits
unless it flushes (piped stdout is block-buffered, and `sbatch --wait` does
not flush), in which case the id simply arrives at the end; both orders work,
only the recoverability window differs. The command's exit status is
deliberately **not** the build's verdict: `sbatch --wait` relays the job's own
exit code, but what the build actually did is whatever the cactup running on
the compute node recorded in the attempt's `build.toml`, read back once the
wait returns — a non-zero exit matters only when no job id ever appeared, in
which case it is the *submission* that failed, not the build. Nothing about
the attempt is stored after the blocking command returns, either: by then the
compute node's own cactup has already written the outcome into the very
`build.toml` a store here would overwrite with a stale copy.

A machine with no `blocking-submit` key gets the ordinary `submit`, followed
by polling the attempt until it reports an outcome — reusing the exact loop
`--follow` already builds on `tail::LogTail` and `tail::PollBackoff` for, just
in a silent mode that skips the output streaming. Ctrl-C detaches without
touching the queued job, exactly as `--follow` does. Both routes derive the
verdict identically, from the attempt's own recorded outcome and never a
relayed exit code, so the only difference a user sees between them is that the
native route has no polling.

`--block` is an error when the submit path is not taken at all: on `cactup
build run`; and on a bare `cactup build` that resolves to the foreground
because `[build].default-action = "run"`, because the machine cannot submit
builds (no `[variants.buildsubmitscript]` variant and/or no
`[scheduler].submit`), or because `--virtual-executable` was given (which is
never queued regardless of what the machine would otherwise pick). The error
names *which* of those decided it, since the fix differs in each case — drop
the flag, drop `--virtual-executable`, say `build submit` explicitly, or fix
the machine. (`cactup build submit` on a machine that cannot submit builds
already errored before `--block` existed, and still does, for the same
reason.)

`--block` and `--follow` are mutually exclusive, rejected by clap rather than
composed: on a machine declaring `blocking-submit` the submit command holds
the terminal for the whole build, so there is nothing left to stream
alongside it, and a flag pair whose combinability depends on the MDB entry is
worse than one that simply never combines. `--follow` already waits for the
build; `--block` is what you use when you want the wait without the output.

**Auto-selection.** Submitting a build is *possible* on a machine iff it
declares at least one `[variants.buildsubmitscript]` entry **and**
`[scheduler].submit` (§4.2) — both are required, since a buildsubmitscript with
nothing to hand it to is as useless as a submit command with nothing to run.
`[build].default-action` then decides what an unqualified `cactup build` does:

| `[build].default-action` | submitting possible | result |
|---|---|---|
| unset | yes | **submit** |
| unset | no | **run** (the foreground fallback — nothing declared, nothing forced) |
| `"run"` | either | **run** (forces the foreground path even where submitting would work) |
| `"submit"` | yes | **submit** |
| `"submit"` | no | hard error, naming which of the two prerequisites is missing |

`build run` always works (it is exactly what `cactup build` did before this
section existed); `build submit` on a machine where submitting is impossible is
a hard error for the same reason, rather than a silent fallback to the
foreground — a user who explicitly asked to submit should not be surprised by
a login-node compile.

**The prepare/execute split, and what is frozen at submit time.** A build's
work splits into two phases with different privileges (D11):

- **`prepare`** — everything that needs the MDB, the installation's git repos,
  or the knobs: resolving the optionlist variant and universe, resolving the
  thornlist and its machine/variant thorn toggles, making the rebuild decision
  (§7.8), and staging the rendered optionlist, processed thornlist, and
  assembled `make` step list. This runs **only on the login node**, whether the
  build that follows is a foreground `build run` or a queued `build submit`.
  Prepare allocates the attempt directory and stages every artifact *into it*
  — never into the live `configs/<name>/` — so an abandoned or still-queued
  submit can never poison the config's rebuild-decision baseline or clobber a
  concurrent build's staged files.
- **`execute`** — takes the per-config build lock, runs the frozen build
  script, completeness-checks the result, and — only on success — installs the
  thornlist/optionlist snapshot into the live config directory and stamps
  `ConfigMeta`. This is the phase a submitted job's compute node actually runs,
  via the re-invocation in §7.9.1; for a foreground build it runs immediately
  after `prepare`, on the same node, with no queue wait between them.

Because `execute` may run on a different node, hours after `prepare`, on a
machine `prepare` never touches, everything it needs is **frozen into the
attempt's `build.toml`** rather than re-derived: the resolved `make` invocation
and effective build-phase `env-setup`, the frozen `UniverseSpec` (not just a
universe *name* — the executing node must be able to re-wrap the build command
without reading the MDB), the full variable set (§6.3) including things only a
login node can produce (the allocation knob, `@ENV(NAME)@` resolutions, `USER`,
`CACTUP` from the running binary's own path), and a fully-formed `ConfigMeta` to
store on success. `install_root` is frozen **separately** from `cactus_root`
because it cannot be derived from it — the thornlist's own recorded root need
not match the Cactus tree's `!DEFINE ROOT`. Nothing about the *decision* to
build is re-litigated on the compute node: prepare's rebuild decision, and the
optionlist/thornlist resolution behind it, are MDB-derived and correctly
frozen. What execute *does* re-check is the source tree, covered next.

**Re-probing staleness at execute time.** A build queued for hours can outlive
the source tree it was prepared against — a refetch, a manual `git checkout`,
or a hand-edit can land during the wait. If `execute` simply trusted the
sources/providers/shapes snapshot `prepare` recorded, that snapshot would
become a **lie**: the next plain `cactup build` would diff stored-against-live,
find them equal (because the frozen record already matches whatever the queued
build compiled *against*, not what it actually compiled), and print "up to
date" forever — a silently wrong binary, permanently. So `execute` re-probes
sources, providers, and shapes itself, right before compiling, and:

1. Records what it actually finds — not what `prepare` found — as this
   attempt's baseline.
2. Runs the `remove_stale` per-thorn cleanup (§7.4) against *that* baseline,
   not prepare's.
3. Upgrades an `Incremental` decision to `Full` if the re-probe shows the flesh
   itself moved since prepare — but never re-runs the optionlist/thornlist half
   of the decision, which is genuinely MDB-derived and does not need re-asking.
4. Refuses outright if the config directory or the attempt directory has
   vanished (a `config delete` landed while this was queued) — a queued build
   must never resurrect a deleted config.
5. Proceeds, and says so, if some *other* build of the same config succeeded
   in the meantime — the user asked for this build, and a compute-node job
   silently no-op'ing because someone else won a race would be more surprising
   than just doing the work again.

**The attempt's own record is the authority on the outcome — never the
scheduler's exit status.** This is the structural payoff of running cactup
itself on the compute node rather than wrapping `make` in a job: there is no
"did the wrapper actually run `make`, and did `make` actually finish" gap to
paper over with `sacct` scraping. The compute-node `execute` call runs the
build, applies the same completeness check a foreground build does
(§7.4/§7.8), and writes the verdict into `build.toml`'s `[outcome]` table
itself. `build show`/`build log`/`build list` (and the login-node reconcile
step below) read *that* record, never the scheduler's own accounting of
whether the job "succeeded" — a scheduler's success/failure classification
answers "did the job exit", not "did the build finish", and those are exactly
the two things the old universe-based wrapper conflated.

**Every build gets an attempt — foreground or queued, one per invocation.**
`configs/<name>/.cactup-builds/%04d/` holds `build.toml` (the frozen metadata
above, including the `[vars]`/`[knobs]` tables that let `execute` and a build
submit script resolve `@NAME@`/`@KNOB(…)@` without the DB — §5.1, §9.3), the
frozen `build-script` (and a `submit-script` too, for a queued
build), `build.out`/`build.err`, and a `running.lock`/`heartbeat` pair with the
same liveness semantics as a simulation restart's (§9.3). There is
deliberately **no `-active` symlink**: unlike a simulation's `output-NNNN`
directories, a build attempt's whole directory is already cactup's own — never
a user-visible workspace — a config has at most one build in flight at a time
(enforced at submit, next), and there is no simfactory `output-NNNN-active`
contract to preserve here the way §9.2 requires for restarts. The
**highest-numbered attempt is always the subject**: the live one if a build is
in flight, the most recent result otherwise. A pointer would only be a third
source of truth free to go stale, so there isn't one; `build prune --keep N`
(never automatic — surprising deletion is worse than disk use) simply removes
the oldest attempt directories.

**Builds never chain.** `sim submit`'s auto-chaining (§8.8) exists because a
simulation's requested walltime can legitimately exceed one job's ceiling and
still make sense as a sequence of checkpoint-and-resume jobs. A build has
nothing to resume from — a half-finished `make` is not a checkpoint — so a
requested `[build].walltime` (or `-w`) above the queue's ceiling is simply an
error, not a split. Two queued builds of the same config are also never
wanted: submitting refuses (unless `-f`) whenever an attempt is already live,
using the same liveness protocol §8.3 uses to decide whether a restart is
really dead — the per-config lock (below) held, or the attempt's
`running.lock`/heartbeat still fresh, or its scheduler status still live.

**Locking across submit → queue → execute.** `LinkLock::acquire` (§2.3) fails
fast and never blocks, so it cannot span a queue wait — a build's lock
discipline has to work around that rather than through it:

- `prepare` takes the per-config build lock (`.cactup-build.lock`, §2.3 item 4)
  only for its staging writes and attempt-id allocation, then **releases** it
  before returning — whether or not what follows is a queue wait.
- `execute` takes the same lock for the duration of the `make` invocation,
  held with a heartbeat, regardless of which node runs it. If a compute-node
  job happens to start while a foreground build still holds the lock, it fails
  loudly and is recorded as a failed attempt — a correct outcome, not
  corruption, since the alternative would be two `make` invocations racing the
  same `configs/<name>/`.
- The real guard against that ever happening in practice is the submit-time
  liveness refusal above, not the lock: the lock is the last line of defense,
  the refusal is the one that actually prevents the collision.

**Login-node-only steps stay login-node-only (D11).** Two things a successful
build does — pointing a null active config at its first successful build
(§7.1), and best-effort `CACHE/exe` garbage collection (§8.1) — need the
per-installation lock or the registry, so they cannot run inside `execute` on
a compute node. They run as a login-node **reconcile** step instead, triggered
by `build list`/`build show`/the next `cactup build` noticing a newly-finished
attempt, keyed on the attempt's outcome having recorded a completed build. The
active-config pointer is set only there — never at submit time — so a build
that is later found to have failed never leaves the installation pointing at a
config with no `cactup-config.toml`.

#### 7.9.1 Compute-node re-invocation

A generated buildsubmitscript re-invokes cactup to do the actual build, the
same pattern §8.3.1 uses for a submitted simulation:

```
@CACTUP@ build run @CONFIGURATION@ \
    --installation=@ALIAS@ --config-dir=@CONFIG_DIR@ --machine=@MACHINE@ \
    --attempt-id=@ATTEMPT_ID@
```

`--config-dir` (the config's absolute directory) + `--attempt-id` fully locate
the attempt (`@CONFIG_DIR@/.cactup-builds/<ATTEMPT_ID>`) with no registry
lookup, exactly as `--sim-dir`/`--restart-id` locate a restart. `--installation`
and `--machine` supply the alias and machine name without touching the global
DB. `cactup build run --config-dir --attempt-id` reads everything else —
variant, universe, the resolved `make` invocation, the full frozen variable
set — from that attempt's on-disk `build.toml` (§7.9), satisfying the same D11
rule §8.3.1 states for a compute-node `sim run`: the path a generated script
takes never touches the global DB, the installation registry, the MDB, or
knobs. `--config-dir` and `--attempt-id` are mutually required — like
`--sim-dir`/`--restart-id`, this pair is all-or-nothing, since the
compute-node branch is the only path that reads either.

Scheduler stdout/stderr for the *submission* job itself go to `@STDOUT_FILE@`/
`@STDERR_FILE@` as usual; the `make` invocation's own output — what a user
watches with `build log` — is teed to the attempt's `build.out`/`build.err`
directly by `execute`, not left to the scheduler's redirection.

---

## 8. Simulation subsystem

Local to the active installation. Replaces `sim-manage` + `simrestart`
(`simfactory-docs.txt` §14).

### 8.1 Where simulations live (D5)

```
<sim-home>/<config>/<SimName>/...
```

**Sim-home resolution (fixed at install time).** Each installation has one
**sim-home**, computed when the release is installed and recorded in
`<installation home>/.cactup/installation.toml`:

- sim-home = `<machine simulation-home>/<alias>`, where `simulation-home` is the
  optional machine `meta.toml` key (§4.2), `@USER@`-substituted and made absolute.
- If the machine omits `simulation-home`, sim-home falls back to
  `~/.cactup/simulations/<alias>`.

This is the port of simfactory's `GetBaseDir` (`simfactory-docs.txt` §13.1),
except (a) the root comes from the renamed `simulation-home` key with a home-dir
fallback, and (b) it is resolved once at install rather than per-command, so
`sim` subcommands never re-derive it. There is **no `--basedir` flag**.

**`root-dir` resolution (same discipline as sim-home).** `installation.toml`
also records `root-dir` — the thornlist's `!DEFINE ROOT` directory name
(§3.2) — resolved **once, at install time**, and never re-derived. A missing
key (an installation from before root tracking landed) means the historical
default, `Cactus`. `Installation::cactus_root()` resolves
`<installation home>/<root-dir>` from this one recorded value, and every
path that needs the source tree — build, sim, test, config, refetch,
delta — goes through it rather than assuming a literal `Cactus`.

**Per-simulation directory & the registry.** A simulation's directory defaults to
`<sim-home>/<config>/<SimName>` (grouping simulations by the config that produced
them). It may be overridden **only at create time** with `sim create --sim-dir
<path>` (§8.2); it cannot move afterward. Because a sim may live at a custom
path, cactup keeps a per-installation **registry** at
`<installation home>/.cactup/simulations.toml` mapping `<SimName>` → its absolute
directory (plus config and creation time). All later `sim` subcommands (`submit`,
`run`, `stop`, `clean`, `delete`, `show`, `log`, `output-dir`) locate a
simulation by name through this registry — this is why no path flag is needed
after create. `sim show` lists the active installation's registry; `sim show
--all` unions every installation's registry (§8.1 is the only place cross-install
enumeration happens). The registry is a *location index*, not per-sim state (which
still lives in the sim's own `.cactup/`, D4); a `sim` command that finds a
registry entry whose directory is gone reports it as missing and offers to prune
the stale entry (the same reconcile discipline as the stale-restart reaper, §8.3).

`CACHE/` and `TRASH/` siblings live at `<sim-home>/` level (one cache / trash per
installation). The executable hard-link cache (`CopyFileWithCaching`) stores
**one physical copy per `build-id`** under `<sim-home>/CACHE/exe/<build-id>`,
**keyed by `build-id`, not a content hash** — cactup does not hash the
hundreds-of-MB binary on every `sim create`; it trusts the `build-id` recorded in
the config metadata (§7.4) to identify the binary. A simulation's frozen binary
lives at its own `.cactup/exe`, a **hard link** into the cache entry (a plain copy
only when the link would cross filesystems — the Cactus tree
`~/.cactup/cacti/<alias>` and the sim-home, often `/work` or `/scratch`, are
frequently on different filesystems; simfactory does the same). Restarts do not
each re-link the binary; they exec the simulation-level `.cactup/exe` (the value
of `@EXECUTABLE@`, §6.3). This buys two things simfactory relied on: (1) N
simulations from one build share one physical binary instead of N full copies,
and (2) the linked copy is frozen at create time, so **rebuilding the config never
disturbs an in-flight run**.

**One build per config; orphan cleanup.** A config *is* a build: at most
one `build-id` for a config exists at a time. When `cactup build` produces a new
`build-id` for an existing config, the previous `CACHE/exe/<old-build-id>` entry
is an orphan the moment nothing links it. cactup **garbage-collects** the cache:
a `CACHE/exe/<build-id>` entry is removed once its on-disk link count shows no
simulation still references it (a hard-linked file whose only remaining link is
the cache entry itself). GC runs opportunistically when a config is rebuilt and,
authoritatively, on **`sim delete`** (§8.7): deleting the last simulation that
referenced a `build-id` drops the link count so the cache entry can be reaped.
Because trashing a simulation (`sim delete` → `TRASH/`, §8.7) *keeps* the sim's
`.cactup/exe` link, the cache entry survives until the trash is emptied — GC
therefore also considers `TRASH/` links live and only reaps a `build-id` when no
live sim *and* no trashed sim references it.

### 8.2 `sim create`

```
cactup sim create [-f] [--ignore-machine] <sim> <parfile> [--config C] [--sim-dir P]
```

Port of `create()` (`simfactory-docs.txt` §14.1):
1. Resolve config = `--config` or the installation's active config; locate
   `<Cactus root>/exe/cactus_<config>` (fatal if missing). The config must
   have been built for the resolved machine (§7.4); `--ignore-machine`/`-f`
   overrides.
2. **Validate `<parfile>`'s name (collision guard).** The parfile must end
   in `.par` (literal `@NAME@` substitution) or `.py` (Python variant —
   §6.1/§6.2). cactup strips that one extension to derive the working-directory
   name (so intra-name dots are fine — `q1.5.par` → working dir `q1.5/`,
   `q1.5.py` → `q1.5/`). Create is rejected only if the extension-stripped
   basename equals a **reserved name**: `.cactup`, `exe`, `cfg`, or `par` (the
   metadata dir and its master-copy entries, §9.3). This is the *only* naming
   restriction.
3. Determine the simulation directory: `--sim-dir <path>` if given, else
   `<sim-home>/<config>/<SimName>` (§8.1). Register it in the per-installation
   `simulations.toml` registry (§8.1).
4. Create the simulation skeleton (§9). `-f` deletes a pre-existing simulation of
   the same name first (simfactory fataled instead; cactup's `-f` overwrites per
   `cactup-simfactory-design.txt`).
5. Write the cactup simulation metadata (§9.3), and hard-link the executable from
   `<sim-home>/CACHE/exe/<build-id>` into the sim's `.cactup/exe` (populating the
   cache entry for this `build-id` first if it isn't cached yet, and
   opportunistically GC-ing orphaned `build-id` entries — port of
   `CopyFileWithCaching`, §8.1).
6. Copy the parfile into `.cactup/par`, and the config's build provenance into
   `.cactup/cfg`: the optionlist pair (`cactup-optionlist.cfg` rendered +
   `cactup-optionlist.toml` source) and the thornlist pair
   (`cactup-thornlist.th` processed + `cactup-thornlist.src.th` source, §7.5).
   The processed thornlist is what records *which thorns the frozen binary
   contains*, so a simulation stays self-describing after its config is
   rebuilt or deleted. `simulation.toml`'s `optionlist`/`thornlist` keys name
   the fed-to-Cactus copy of each pair; both are empty strings when the config
   was built by a cactup old enough not to have written the artifact.

No restart is created yet (matches simfactory).

### 8.3 `sim submit`

```
cactup sim submit [-f] [--overwrite] [--force-queue] [--ignore-machine] [--universe U | --no-universe] <sim> <TOPOLOGY…>
cactup sim submit [-f] [--overwrite] [--force-queue] [--ignore-machine] [--universe U | --no-universe] <sim> <parfile> [--config C] <TOPOLOGY…>   # implicit create
```

Port of `submit()` (`simfactory-docs.txt` §14.2):
- If `<sim>` doesn't exist and a parfile is given, create it first (the
  `create-submit` fusion). If `<sim>` **does** exist and a parfile is *also*
  given, that is an **error** unless `--overwrite`/`-f` is passed — cactup
  will not silently ignore a parfile that contradicts an existing sim.
- Validate the built config's `compatible-queues` against the chosen queue
  (§4.4 / D12); error on mismatch unless `--force-queue`/`-f`. Before that,
  the config and the simulation must both have been built for the resolved
  machine (§7.4); error on mismatch unless `--ignore-machine`/`-f`.
- **Reap a stale active restart (port of simfactory `initRestart`).** Before
  allocating a new restart, if an `output-NNNN-active` symlink exists cactup
  decides whether its run is truly dead using the **liveness protocol**, not
  scheduler status alone: a restart is reaped (auto-`clean`ed — deactivate +
  finish, §8.6) **only if all of** (a) its live job status is `U` (gone from the
  queue), (b) its per-restart liveness marker (`.cactup/running.lock`, §9.3) is
  not held, and (c) its heartbeat file (`.cactup/heartbeat`, touched periodically
  by a live `sim run`, §9.3) is older than a staleness threshold. This prevents
  reaping a compute job that is briefly invisible to `get-status` during a
  scheduler hiccup (a transient `U`). A restart that is still running/queued —
  or whose marker/heartbeat says it is alive — instead triggers chaining (below),
  never reaping. Without reaping, the lingering `-active` symlink would make
  `makeActive` refuse (§9.2).
- Allocate the next restart id, create `output-NNNN/` + its `.cactup/` dir.
- Compute the topology variable set (§8.5), select the submit/run script
  variants for the chosen queue (§4.4), substitute, and write the substituted
  SubmitScript into the restart metadata.
- **Resolve universes (§4.8).** Resolve the **run** universe (following the §4.8
  precedence: `--universe` → the config's coerced build universe unless the
  optionlist opted out → runscript variant / `default-universe` → none) and record
  it in `restart.toml` so the compute-node `sim run` applies it (§8.3.1); resolve the
  **submit** universe and, if any, wrap the `submit` command below in it. The
  run-universe resolution happens here, on the login node, because the compute
  node deliberately does not touch the MDB/knobs/CLI (D11).
- `makeActive()` — create the `output-NNNN-active` symlink (§9.2) **only for the
  restart that will run first**; chained pre-submissions (below) are created
  un-activated.
- Run the machine `submit` command (wrapped in the submit universe if one was
  resolved, §4.8); parse the job id with `submit-pattern`; store it in the restart
  metadata (`job-id`; `-1` ⇒ failed/unknown).
- **Auto-chaining** (the simplified model — §8.8): if requested walltime > the
  effective walltime ceiling (§4.2), cactup transparently pre-submits
  `ceil(walltime / ceiling)` chained restarts, each scheduler-dependent on the
  prior job id. There is no user-facing chaining command; the dependency flag is
  emitted by the submit-script variant (a `.py` variant, since this is
  conditional — §6). Each segment continues from the prior one only insofar as
  its **parfile** recovers the newest checkpoint — cactup does not arrange that
  and takes no position on it (§8.8).

#### 8.3.1 Compute-node re-invocation

The generated SubmitScript re-invokes cactup to do the actual run, as
simfactory's template re-invoked `sim run` (`simfactory-docs.txt` §12.3). But in
cactup a bare `sim run <name>` is **not** a unique locator — with no `--basedir`
flag it would resolve `<name>` against the login node's *active installation* and
its registry, which may be unset or different on the compute node. The template
therefore passes the **simulation directory explicitly** and does **not** rely on
global DB state:

```
@CACTUP@ sim run @SIMULATION_NAME@ \
    --installation=@ALIAS@ --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ \
    --restart-id=@RESTART_ID@
```

`--sim-dir` (the absolute simulation directory) + `--restart-id` fully identify
the restart (`@SIMULATION_DIR@/output-<RESTART_ID>`) with no registry lookup;
`--installation` and `--machine` supply the alias and machine name without
touching the global DB. `cactup sim run --restart-id` reads everything else
(config, executable path, topology, **and the run universe**)
from that restart's on-disk `.cactup/` metadata (§9.3) — satisfying the D11 rule
that the compute-node path does not touch the global DB or the registry. The run
universe was resolved and frozen into `restart.toml` at submit time (§8.3, §4.8),
so the compute node simply wraps the runscript in it without re-consulting the
MDB. Scheduler stdout/stderr filenames
come from the template via `@STDOUT_FILE@` / `@STDERR_FILE@`, not from cactup
code (preserving simfactory's design point).

#### 8.3.2 Active-symlink handoff across a chain

The exactly-one-active invariant (§9.2) is maintained across a pre-submitted
chain as follows. At submit time, only the first restart of the chain is made
active; restarts `K>first` are created with full metadata (including
`chained-job-id`) but **no** `-active` symlink. When
a chained job actually starts on its compute node, its `cactup sim run
--restart-id=K` performs the handoff atomically:

1. If `output-(K-1)-active` exists, unlink it.
2. Create `output-K` under a unique temporary symlink name, then **atomically
   `rename()`** it onto `output-K-active`.

**Why this ordering, and the correctness argument.** Because the active
symlink's *name* encodes the restart id (`output-NNNN-active`, a preserved
contract — §9.2), the predecessor and successor are two differently-named links,
so there is no single syscall that swaps one for the other. The two steps are
therefore performed **under the per-simulation lock** (§2.3), so no other cactup
process observes the intermediate state. Ordering is unlink-old **then**
create-new: the only observable transient (to a non-locking external reader) is a
brief *zero*-active window, which the reader (§9.2) treats benignly as
"inactive" — never a *two*-active window, which simfactory's reader treats as
fatal. Step 2 uses `symlink`-to-temp + `rename()` so a reader never catches a
half-created link. Only one job in a dependency chain runs at a time, so
successors never race each other here. A crash — between the two steps, before a
job starts, or mid-run — leaves at most one stale symlink, which the next
`submit`'s reaper clears.

### 8.4 `sim run`

```
cactup sim run [-f] [--overwrite] [--force-queue] [--ignore-machine] [--universe U | --no-universe] <sim> <TOPOLOGY…> [--debug]
cactup sim run [-f] [--overwrite] [--force-queue] [--ignore-machine] [--universe U | --no-universe] <sim> <parfile> [--config C] <TOPOLOGY…>
```

Port of `run()` / `userRun` / `submitRun` (`simfactory-docs.txt` §14.3): runs
interactively, bypassing the queue. With `--restart-id` it runs that restart —
this is the **compute-node path** the submit script takes (§8.3.1), and in that
mode it accepts `--installation`/`--sim-dir`/`--machine` to locate the
simulation without the global DB or the registry, and performs the chain handoff
(§8.3.2). Throughout its run it holds the
per-restart liveness marker and periodically touches the heartbeat file (§9.3)
so the reaper (§8.3) never mistakes it for dead. Without `--restart-id`, it
builds a fresh restart, makes it active, forks, and tees child stdout/stderr to
`<SimName>.out` / `<SimName>.err` while echoing to the terminal. `--debug`
launches under the debugger (`@RUNDEBUG@`/`@DEBUGGER@`).

**Run universe (§4.8).** cactup executes the substituted runscript wrapped in the
resolved **run** universe, if any. In the compute-node path the universe is read
from `restart.toml` (frozen at submit time, §8.3.1); in the interactive path
(`--restart-id` absent) it is resolved on the spot per the §4.8 precedence:
`--universe` → the config's coerced build universe (unless the optionlist opted
out) → runscript variant / `default-universe` → none. `--no-universe` forces the
host context. The whole runscript — mpirun/`srun` line included — runs inside the
universe (the per-rank-container boundary of §4.8 applies).

Computed parfiles use the **`.py` variant** (§6.2), replacing simfactory's
executable `.rpar`: the `.py` parfile's master copy (§8.2) is invoked per the
§6.1 calling convention (the full §6.3 variable set as JSON on stdin, bound as
globals), and its stdout is written into the restart as the ready-to-run
`<basename>.par` and used **as-is** — no post-substitution, since the author
interpolates the variables directly in Python. A plain `.par` is `@NAME@`-
substituted as before.

### 8.5 TOPOLOGY → process layout

cactup's topology flags (`cactup-simfactory-design.txt` §3) replace simfactory's
proc-distribution math. Flags (those passed through to submit scripts marked *):

| Flag | Meaning | Default |
|------|---------|---------|
| `-a/--allocation` * | account | knob |
| `-q/--queue` * | scheduler queue | knob → machine default queue |
| `-m/--mail` * | notify address | knob |
| `-M/--mail-type` * | notify type | knob (`all`) |
| `-n/--nodes` * | node count | 1 |
| `-T/--tasks` | total tasks (MPI ranks) | inferred: `nodes * tpn` |
| `-t/--tpn` * | tasks per node | full node (see below) |
| `-c/--cpus` * | CPUs (threads) per task | 1 |
| `-g/--gpu` | use GPUs | inferred from the queue's `gpu` flag |
| `-G/--gpus-per-task` * | GPUs per task (GPU runs only) | machine `default-gpus-per-task`, else 1 |
| `-J/--job-name` * | job name | `<SimName>` |
| `-w/--wall-time` * | total walltime, canonical format below | machine/queue default |
| `-o/--out` * | stdout filename | template default |
| `-e/--err` * | stderr filename | template default |

**`-J`, not `-j` (breaking change from earlier drafts of this spec).** This whole
TOPOLOGY flag set is flattened into `cactup build`/`build submit` unchanged
(§7.9), so it can no longer claim `-j` — `cactup build` already spends `-j` on
`--make-jobs` (§7.6), matching `make -j`. `--job-name`'s short moves to `-J`
(SLURM's own spelling) everywhere this flag set is used — `sim submit`/`sim
run`, `test run`/`test submit`, and `build`/`build submit` alike — rather than
carving out a build-only exception, so the flag means the same thing on every
command line a user types.

**Canonical walltime format.** Every walltime cactup accepts or emits —
`--wall-time`, `[queues.<q>].max-walltime`, machine `max-walltime` — uses the
single grammar **`(DD-)?HH:MM:SS`**. The `DD-` day prefix is optional and elided
when zero (`00-1:00:00` ≡ `1:00:00`); when `DD-` is absent, `HH` may exceed 23
(so `72:00:00` = 72 hours is valid). Each field is parsed to an integer count and
reduced to a total number of **seconds**, which is cactup's one internal
representation — all comparisons (queue ceiling checks) and the chaining division
(§8.8) operate on seconds, independent of how the value was spelled. The
`@WALLTIME@` variable and its `WALLTIME_*` components (§6.3) are derived from the
per-job seconds value. `--wall-time` is the **total** wall the user wants for the
whole simulation; cactup splits it into per-job segments during chaining (§8.8).

Derivation (produces the canonical §6.3 names directly — no legacy aliases).
This is the availability→request bridge (§1.1): the `max-`/`default-` hardware
facts fill any topology the user left unset.
- `CPUS_PER_TASK` = `--cpus` if given, else the queue-effective
  `default-cpus-per-task` (simfactory's `num-threads`), else 1. This is the
  request-side default — e.g. Deep Bayou sets it to 24 so a no-`--cpus` job
  fills its 48-CPU nodes as 2 tasks × 24 CPUs.
- `TASKS_PER_NODE` = `--tpn` if given, else
  `floor(MAX_CPUS_PER_NODE / CPUS_PER_TASK)`, min 1 (fill the node — divide the
  node's available CPUs among ranks) — using the queue-effective
  `max-cpus-per-node` (§4.2) — and then, on a GPU run whose queue declares
  `max-gpus-per-node`, additionally bounded by the node's GPU budget:
  `min(that, max(1, floor(MAX_GPUS_PER_NODE / GPUS_PER_TASK)))`. The bound
  applies **only to the fully derived value**: an explicit `--tpn` is
  authoritative, and an explicit `--tasks` is a requested rank count — on a
  single-node machine with no batch system every one of those ranks lands on
  the node, so quietly shrinking `TASKS_PER_NODE` under it would bless a layout
  the node cannot run. Both leave the CPU-rule value in place and are checked
  against the ceiling below instead. With GPUs indivisible and one per rank, a
  node holding fewer devices than the CPU split implies cannot run that many
  ranks — deriving them anyway only produces a layout the ceiling refuses.
  Bounding it here is what lets a GPU machine declare a *polite*
  `default-cpus-per-task` (omnia: a rank wanting 8 of its 72 cores for the host
  side of a single-MI210 job) without the CPU rule inferring 9 ranks for one
  device. This costs the fill-the-node guarantee on such machines — CPUs may sit
  idle, which is the machine's stated intent — and does not make GPUs *build* a
  layout: the count comes from CPUs, GPUs only cap it.
- `TASKS` = `--tasks` if given, else `NODES * TASKS_PER_NODE`. An explicit
  `--tasks` replaces the fill-the-node total, so a *derived* `TASKS_PER_NODE` is
  then capped to `ceil(TASKS / NODES)` to keep the layout self-consistent
  (`--tasks=1` is 1 rank on 1 node, not `TASKS = 1, TASKS_PER_NODE = 2`); an
  explicit `--tpn` is authoritative and never capped. This is a no-op in the
  default case, where `TASKS == NODES * TASKS_PER_NODE` already.
- **Script-variant default tasks (§4.2).** When *no* process-layout flag
  (`-n`/`-T`/`-t`) was given, the selected script variant's optional `tasks = N`
  setting replaces the fill-the-node `TASKS` (capping `TASKS_PER_NODE` to keep
  the layout self-consistent); for a submit the submitscript entry is consulted
  first, then the runscript entry. Testsuite runs additionally fall back to
  `TASKS = 2` when no variant sets it (§11.6).
- `GPU` = `1` if `--gpu` or the chosen queue's `gpu = true`, else `0` — checked
  **one-directionally** against the built binary's `gpu` flag (§4.4 / D12):
  refused (absent `--force-queue`/`-f`) only when the binary's `gpu` flag is
  set and this computed `GPU` is `0`; a non-GPU binary with `GPU = 1` is
  allowed (advisory note only when a non-GPU queue exists on the machine).
- `GPUS_PER_TASK` = `0` whenever `GPU` is `0` — a run with no GPUs asks for
  none, and a script can branch on this variable alone. On a GPU run:
  `--gpus-per-task` if given, else the queue-effective `default-gpus-per-task`,
  else **`1`**. Passing `--gpus-per-task` when the run resolves to `GPU = 0` is
  refused rather than ignored.

  **One GPU per rank is the default everywhere**, deliberately *unlike* the CPU
  chain's fill-the-node rule. A GPU is not divisible the way a core is: one
  device per rank is the overwhelmingly common shape, and it is the only default
  that stays correct when the layout changes for unrelated reasons. Dividing the
  node's GPUs among its ranks instead would silently hand extra devices to a job
  that merely shrank its rank count, and would fight any machine whose scheduler
  *reserves* GPUs on a different axis than it *binds* them (qbd: `--gres` is
  per-node and CPU-derived, `--gpus-per-task` is per-rank). A partition that
  genuinely wants otherwise says so with `default-gpus-per-task`.

  Consequently GPUs never *build* a `TASKS_PER_NODE`: CPUs alone drive the
  process layout, and GPUs only ever bound a derived rank count downward (see
  the `TASKS_PER_NODE` bullet) or refuse an explicitly requested one (below).
- **GPUs are not oversubscribable.** `max-gpus-per-node` is therefore a
  **ceiling, never a target**: it does not set `GPUS_PER_TASK`, it bounds it.
  A layout needing more GPUs per node than the queue has — `GPUS_PER_TASK ×
  TASKS_PER_NODE > MAX_GPUS_PER_NODE`, whether from an explicit
  `--gpus-per-task` or from too many ranks on a node — is a hard error, raised
  before anything is submitted. This is the one place the GPU chain is stricter
  than the CPU chain: CPU oversubscription merely time-slices, while a job
  asking for GPUs a partition does not have either never schedules or lands with
  ranks fighting over one device. Machines whose GPU count is undeclared cannot
  be checked and are therefore never refused on this ground.

  qbd is the worked example, and shows what a machine has to declare for a
  no-flag job to fill its node — which the GPU bound on a derived
  `TASKS_PER_NODE` makes a way to *fill* a node, no longer the only way to get a
  schedulable layout out of one. Its `gpu2` and `gpu4` partitions share a 64-CPU
  node but hold 2 and 4 GPUs. With one GPU per rank fixed, the rank count is
  what has to move — so each partition sets `default-cpus-per-task` to its own
  `64 / GPUs`: 32 on `gpu2` (2 ranks) and 16 on `gpu4` (4 ranks). Both land on
  one rank per GPU with the cores split evenly and nothing idle. Had `gpu4`
  simply inherited `gpu2`'s 32, it would run 2 ranks on a 4-GPU node and waste
  half of it — a reminder that `max-gpus-per-node` bounds a layout but never
  builds one.
- A script that needs "total cores" or "cores requested" computes them from
  `TASKS`, `CPUS_PER_TASK`, `NODES`, and `MAX_CPUS_PER_NODE` — cactup no
  longer pre-derives `PROCS`/`PROCS_REQUESTED`/`PPN_USED`.

**ASSUMPTION:** SMT (`THREADS_PER_CPU`) defaults to the machine (or queue)
`threads-per-cpu` (1) and is not a topology flag in v1; expose later if needed.

### 8.6 `stop` / `clean`

Ports of `simfactory-docs.txt` §14.8 / §14.5:
- `cactup sim stop <sim>`: if `TERMINATE` exists and not `-f`, write `1` into it
  (graceful termination trigger created by the running Cactus job), then finish.
  Otherwise run the machine `stop` command (forced) and finish.
  - **DEVIATION (graceful stop is verified, not assumed).** `TERMINATE:=1` is a
    request: Cactus acts on it at its next termination check, and only if the
    parfile actually enables the file trigger. So the graceful path polls the
    run — queue status, or the `running.lock` liveness marker for a foreground
    run — for up to 30 s. Only a run that really left is finished; while it is
    still there, cactup says so and leaves the restart **active**, because
    deactivating it hides the live job from `stop -f` (whose `active_id` guard
    would then report "nothing to stop") and from the live-job guard in
    `delete` (§8.7). Corollary, enforced in `sim/start.rs`: cactup must never
    create `TERMINATE` itself — its existence is the *evidence* that the run is
    watching it, which a file we minted would destroy.
- `cactup sim clean <sim>`: deactivate the active restart (remove the
  `-active` symlink), tighten `TERMINATE` perms, and run the Formaline tarball
  **hard-link dedup** across prior restarts. Dedup semantics are preserved verbatim from simfactory
  (`simfactory-docs.txt` §14.5): for each `*.tar.gz` ≥ 1000 bytes, scan prior
  `output-%04d` dirs (descending) for a file **of the same name whose contents
  are byte-identical** (`filecmp`-style full comparison — *not* name-only), and if
  found replace this copy with a hard link to it via a `.tmp` rename. Because the
  match requires identical content, dedup can never alias two different tarballs.
  All of this is preserved because it shapes on-disk output (§9).

  **`clean` never deletes checkpoints** (a deliberate divergence from
  simfactory's `cleanup`, which unlinked `*.chkpt.tmp.it_*.*`). Checkpoints may
  be any format — HDF5 files, ADIOS2/BP5 *directories*, whatever a future driver
  writes — in any location the parfile chose, very likely outside the restart dir
  entirely (§8.8). cactup therefore cannot reliably tell a half-written
  checkpoint from a finished one, or from an unrelated file, and a wrong guess
  deletes real data. Reaping partial checkpoints belongs to whoever owns the
  checkpoint dir: the parfile author and the driver.

Job status is queried **live** (not stored): run machine `get-status`, match
against the machine `*-pattern` regexes → `R`/`Q`/`H`/`U`/`E`
(`simfactory-docs.txt` §14.7). cactup may *cache* the last observed status in the
restart metadata for display, but the symlink + live query remain the source of
truth (per D4's "state inferred, not stored" principle — `simfactory-docs.txt`
§24).

Display-state derivation for `cactup sim show`:
- **PRESUBMITTED** = a restart that has a `job-id` and a `chained-job-id` (it was
  pre-submitted as part of a chain, §8.3.2) but is **not yet active** (no
  `-active` symlink) and whose job is still `Q`/`H` behind its dependency. It is
  distinguished from a plain QUEUED restart precisely by being a non-active,
  dependency-gated member of a chain.
- RUNNING/QUEUED/HOLDING follow `R`/`Q`/`H` of the *active* restart; FINISHED =
  active restart whose job is `U` and that has reached termination; ERROR = `E`;
  INACTIVE = no active restart.

`--long` additionally prints the build provenance snapshotted into
`.cactup/cfg/` at create time (§8.2) — the paths of the simulation's optionlist
and thornlist — so "what was this binary compiled from?" is answerable from the
simulation alone, after the config has been rebuilt or deleted. Either reads
`(not recorded)` for a simulation created before cactup snapshotted that
artifact.

### 8.7 `sim delete`

Port of `purge`/`trash()` (`simfactory-docs.txt` §14.10): **fatal if any restart
has a queued/holding/running job, unless `-f`** — as the "bypass all nagging"
umbrella (§3), `-f` overrides the live-job guard (cactup first `stop`s the
running/queued jobs, §8.6, then deletes), the trash/purge default, *and* the
`--purge` prompt. Without `-f` on a live simulation, cactup refuses and tells
the user to `sim stop` first. Deletion otherwise does a `shutil.move`-equivalent
of the whole simulation dir into `<sim-home>/TRASH/<simulation-id>/`, removes the
simulation's entry from the per-installation registry (§8.1), and then **runs
executable-cache GC** — if this was the last live-or-trashed sim referencing its
`build-id`, the `CACHE/exe/<build-id>` entry is reaped (§8.1). Nothing is
deleted outright by default; `TRASH/` is not auto-emptied. **ASSUMPTION:** `cactup
sim delete --purge` (also implied by `-f`) permanently removes instead of trashing
(and its links no longer count as live for cache GC, so purging can reclaim the
`build-id` immediately); default is the safe move-to-trash. (If the sim was
created with a `--sim-dir` on a different filesystem than `<sim-home>`, the move
degrades to copy-then-delete.)

### 8.8 Checkpoint recovery, walltime & the simplified restart CLI (D4)

**Checkpoint recovery is out of cactup's scope (scope decision).** cactup does
**not** choose, locate, copy, link, or otherwise steer which checkpoint a run
recovers from. Recovery is driven entirely by the **parfile and the Cactus
driver**: the parfile author points Cactus at a checkpoint/recovery directory and
Cactus automatically loads the newest checkpoint it finds there. That directory
is typically **shared across restarts and lives outside any `output-%04d`** — a
parfile saying `IO::recover_dir = ../checkpoints_standing` resolves it against
the run's cwd (the restart's working dir, per the runscript's `cd @RUNDIR@-active`),
naming a single `<SimName>/checkpoints_standing` for the whole simulation. cactup
never reads that directory, never parses checkpoint filenames, and sets no Cactus
recovery parameter. Consequently there is **no recovery-source selection, no
`from-restart-id`, and no on-disk checkpoint shuffling** anywhere in cactup.

**What this replaces, and why.** Earlier drafts of this spec ported Carpet's
`PrepareCheckpointing` (`simfactory-docs.txt` §14.6): cactup scanned restarts
backward for `*chkpt.it_*` files, picked a "recovery source", prompted the user
when the history looked divergent, hard-linked that restart's checkpoints into
the new restart, and recorded `from-restart-id` / `checkpointing` in
`restart.toml`. **This genuinely worked in simfactory**, and the reason cactup
drops it is *not* that it was always broken — the honest reasons are narrower:

1. **cactup cannot know where the checkpoints are.** The location is
   `IO::checkpoint_dir`, set **inside the parfile** in Cactus's own config
   language: resolved against the run's cwd, subject to Cactus's `$parfile`
   substitution, and — for a `.py` parfile (§6.1) — not even existing as text
   until the script is *executed at run time*. There is nothing cactup can
   reliably parse. Every scan cactup could write is a guess about someone else's
   configuration language.
2. **A `*chkpt.it_*` glob is a guess that already lost.** It matches Carpet's
   `standing.chkpt.it_100.file_0.h5` but not CarpetX/openPMD's
   `checkpoint.chkpt.it00001888.bp5`. Widening the pattern only moves the guess;
   the next driver names them something else again. (Note the file **format** was
   never the obstacle: a BP5 checkpoint is a *directory*, but recursive
   hard-linking — `cp -al` semantics — handles that fine. Format is a red
   herring; **location** is the blocker.)
3. **When the parfile is written sensibly, there is nothing to do.** A shared
   checkpoint dir outside the restarts plus `IO::recover = "autoprobe"` — e.g.
   `IO::checkpoint_dir = "../checkpoints_$parfile"`, which resolves one level
   above the restart dir to a single `<SimName>/checkpoints_<par>/` — gives
   continuity across restarts for free: Cactus finds the newest checkpoint
   itself, with no copying, no per-restart duplication, and no scan. This is
   strictly better than linking N generations forward, and it is what the
   reference parfile does.

The `--resume-from` and `--no-recover` flags go with it: neither could reach
`IO::recover`, so neither did what its name promised.

**Consequences accepted — including one real loss.** The convention simfactory
served — a parfile that leaves checkpoints **inside** the restart (no `../`, so
they land in `output-%04d/…`) — needs *somebody* to carry them forward, and
cactup no longer will. Such a parfile **silently cold-starts** on resubmit where
simfactory would have continued. The fix is one line in the parfile: point
`checkpoint_dir`/`recover_dir` at a shared directory outside the restarts (item 3
above). This is a deliberate trade: cactup declines to support the inferior
pattern rather than guess at parfile semantics to prop it up. Beyond that, cactup
cannot distinguish a cold start from a continuation and does not try;
`restart.toml` records no recovery lineage and `sim show` displays none; a chain
whose parfile fails to checkpoint before the wall loses that segment's tail
(already the parfile author's responsibility — see the walltime paragraph below).

If cactup ever needs to participate in recovery, the shape is **templating, not
file-moving**: expose a `@CHECKPOINT_DIR@`/`@RECOVER_DIR@` variable that the
parfile consumes, so cactup *owns* the location instead of guessing it — and the
parfile stays the single source of truth. That, not a smarter scan, is the door
left open.

**Walltime is a scheduler reservation only — cactup does not manage Cactus
termination.** cactup's sole walltime responsibility is *reserving* wall with the
scheduler (the `@WALLTIME@` value it puts in the scheduler directive, and, when
chaining, the per-segment wall). It does **not** target, inject, inspect, or
otherwise consider any Cactus runtime termination parameter — `TerminationTrigger`
is out of cactup's model entirely. Ensuring a job **checkpoints before its
scheduler wall** (so a chained successor has something to recover from) is the
**parfile author's responsibility**: their parfile configures whatever
termination/checkpoint-on-terminate behavior they want. A parfile that runs to
the wall with no checkpoint will simply lose that segment's tail — cactup does
not, and by design cannot, prevent that.

**Two walltimes, one buffer (a computed convenience, still exposed).** cactup
gives the parfile author two values to work with (§6.3): the **hard wall**
`@WALLTIME@` (what the scheduler will kill at) and a **checkpoint hint**
`@CHECKPOINT_WALLTIME@` = hard wall − buffer (a suggested moment to checkpoint
and wind down with margin to spare). A typical parfile ties the hard wall to a
safety termination and the checkpoint hint to its walltime-based
checkpoint/termination trigger, so it checkpoints comfortably before the kill.
Both are just numbers cactup hands to the substitution engine; **cactup still
sets no Cactus parameter itself**, and an author may ignore either variable. The
**buffer** defaults to `max(reserved-walltime / 24, 10 minutes)` — so a 24-hour
reservation yields ~1 h of margin and a 2-hour reservation the 10-minute floor —
and is overridable per invocation with `--checkpt-buffer <walltime>` (canonical
format, §8.5). The chosen buffer is recorded in `restart.toml` (§9.3). Note this
changes nothing about what cactup reserves: the scheduler always gets the full
hard wall; the buffer only shifts the *hint* variable.

The key cactup divergence from simfactory is the **CLI**: simfactory exposed a
thicket of manual knobs (`--recover`, `--restart-id`, `--from-restart-id`,
explicit pre-submit chaining). cactup hides all of that behind sensible
defaults. The user thinks in terms of "submit this simulation" / "submit it
again to extend it", not restart bookkeeping.

**Default behavior (no restart flags):**

1. **Next restart on resubmit.** `cactup sim submit <sim>` (or `sim run`) on a
   simulation that already has restarts allocates the next `output-%04d` and runs
   it; the first submit of a fresh simulation starts from `output-0000`. Whether
   that run continues from a checkpoint or starts from scratch is decided by the
   parfile + Cactus (above), never by cactup — so there is no "is this a fresh run
   or a continuation?" decision for the user to make on the cactup CLI, because
   cactup is not the one making it.
2. **Automatic walltime chaining.** If the requested total `--wall-time` (in
   seconds, §8.5) exceeds the effective per-job walltime ceiling (§4.2), cactup
   transparently pre-submits `ceil(total-walltime / ceiling)` chained restarts,
   each scheduler-dependent on the previous job and each *reserving* the ceiling
   as its scheduler wall (§8.3). The user asks for "100 hours" on a 24-hour-max
   queue; cactup figures out it needs 5 chained jobs. No manual chaining command
   exists. Continuity across segments is the **parfile's** doing, not cactup's:
   each segment picks up the newest checkpoint in the shared recovery dir because
   its parfile says so (above). Whether a segment actually checkpoints before its
   wall so the next has something to continue from is likewise the parfile
   author's responsibility — cactup only reserves the wall.

   **No-op tail jobs after early completion (accepted).** The chain is a fixed
   set of dependency-gated jobs sized from `--wall-time`. If the simulation reaches
   its termination condition partway through (say segment 2 of 5), the remaining
   pre-submitted segments still launch when their scheduler dependency clears; each
   starts Cactus, which recovers the final checkpoint, sees the run already
   terminated, and exits quickly. cactup does **not** cancel the tail — doing so would require it to
   inspect Cactus termination state, which it deliberately does not model
   (see the walltime paragraph above). These tail jobs are harmless (no data
   change) but do consume a queue slot and startup each. **Sizing the chain
   sensibly by passing a reasonable `--wall-time` is the simulation runner's
   responsibility**; over-requesting simply yields a few no-op tail jobs.

**Minimal manual knobs (escape hatches only):**

| Flag | On | Effect |
|------|-----|--------|
| `--restart-id N` | run (with `--sim-dir`) | **Locator, not a recovery knob.** Load and run exactly this `output-%04d`. This is the submit-script's own re-invocation on the compute node (§8.3.1) and nothing else: it is meaningless without `--sim-dir` and the two are required together. |
| `--checkpt-buffer W` | submit, run | Override the checkpoint buffer (default `max(reserved-walltime/24, 10 min)`) that sets the `@CHECKPOINT_WALLTIME@` hint = hard wall − buffer (§8.8 above). Affects only the exposed hint variables; cactup still reserves the full hard wall and injects nothing into Cactus. |

That's the entire manual surface — two flags, neither of which touches recovery.
simfactory's `--recover`, `--from-restart-id`, and cactup's own short-lived
`--resume-from` all have **no** equivalent: recovery is not cactup's decision to
make (above), so there is no knob to expose. There is no user-facing chaining
flag either; internally cactup still tracks `chained-job-id` in `restart.toml`
(§9.3) to wire up scheduler dependencies — computed, not asked for.

---

## 9. ON-DISK LAYOUT — the binding compatibility contract

This is the load-bearing interface (`simfactory-docs.txt` §13, §24).

**What the contract actually promises (precise statement).** The thing
external analysis tools depend on is the **layout of Cactus output data** — the
numbered `output-%04d` restart dirs, the per-restart Cactus working directory
(the parfile-basename dir where HDF5/ASCII output, checkpoints, and Formaline
tarballs land), and the `output-NNNN-active` symlink. cactup preserves **that**
exactly (§9.1, §9.2): it never renames, moves, or removes any file or directory
that simfactory placed at or below `<SimName>`, and the Cactus working dir is left
entirely untouched. cactup's own bookkeeping is purely **additive** — it adds
`.cactup/` subdirectories (at sim level and inside each `output-%04d`, §9.3) that
simfactory did not create. So the honest invariant is *"no existing output file
or directory is renamed/removed; cactup only adds `.cactup/` dirs alongside"* —
**not** a literal byte-for-byte-identical tree. Tools that read specific output
files, or the working dir, or the active symlink, are unaffected; a tool that
blindly enumerates a simulation's children will additionally see `.cactup/`.
Scoped divergences beyond the additive `.cactup/` dirs: (1) the path *to*
`<SimName>` differs from simfactory's flat `basedir/<SimName>` (§8.1); (2) the
metadata directory and its TOML format (§9.3), which no external tool reads (D4).

### 9.1 Preserved structure

```
<sim-home>/                                    = <machine simulation-home>/<alias>, or ~/.cactup/simulations/<alias> (§8.1)
  CACHE/exe/<build-id>                         executable cache, one per build-id (§8.1)  [PRESERVED]
  TRASH/<simulation-id>/                       trashed simulations              [PRESERVED]
  <config>/<SimName>/                          (default; overridable at create with --sim-dir)
    log.txt                                    simulation log                   [PRESERVED format]
    output-0000/  output-0001/ … output-NNNN/  numbered restarts ("output-%04d")[PRESERVED]
    output-NNNN-active                         RELATIVE symlink → output-NNNN   [PRESERVED]
    output-%04d/
      <parfile-basename>.par                   ready-to-run parfile (substituted from
                                               `.par`, or emitted by a `.py` parfile — §6.2) [PRESERVED]
      <parfile-name>/                          Cactus working/output dir        [PRESERVED]
                                               (checkpoints, Formaline tarballs,
                                                Cactus output land here)
      <SimName>.out, <SimName>.err             foreground-run stdout/stderr     [PRESERVED]
      TERMINATE                                graceful-termination trigger     [PRESERVED]
      .cactup/                                 restart metadata (ADDED — §9.3)  [cactup-owned]
```

- `output-%04d` naming, ids `0..9999`, discovery regex, and the
  `>9999 ⇒ "maximum number of restarts reached"` rule: preserved
  (`simfactory-docs.txt` §13.5).
- The **active restart symlink** is the single source of truth for "which
  restart is active": a **relative** symlink named `<SimulationDir>/output-NNNN-active`
  whose target is `output-NNNN`. Exactly-one-active invariant preserved
  (`simfactory-docs.txt` §13.4). cactup also reads this symlink (not a stored
  field) to determine the active restart, for compatibility.
- The Cactus working directory is the parfile basename with extension stripped;
  Cactus writes its output, checkpoints, and Formaline tarballs there — untouched
  by cactup.
- `simulation-id` string format preserved
  (`simulation-<name>-<machine>-<hostname>-<user>-<timestamp>-<pid>`,
  `simfactory-docs.txt` §13.8) since it names the `TRASH/` subdir and appears in
  metadata.
- `@SHORT_SIMULATION_NAME@` (the scheduler job name): keep the truncation rules
  (printable-only, whitespace→`_`, ensure leading letter, truncate to 15 chars).
  **ASSUMPTION:** drop simfactory's joke `--hide`/`--hide-boring`/`--hide-dangerous`
  obfuscation word lists; default short name = `<SimName>-<RestartID>` truncated.

### 9.2 The active symlink mechanism — explicit

Preserved exactly (`simfactory-docs.txt` §13.4): `makeActive()` creates
`<SimulationDir>/output-NNNN-active` → `output-NNNN` (relative). Refuses if one
already exists. `clean`/`finish` removes it with `unlink`. Reading: scan for
`output-([0-9]+)-active`; zero ⇒ none, more than one ⇒ fatal.

### 9.3 cactup metadata (the ONLY divergence — D4)

simfactory put per-sim/per-restart bookkeeping in `SIMFACTORY/properties.ini`
(`[properties]` INI). cactup replaces this with its own directory and **TOML**
format. External tools do not read this metadata, so changing it is safe.

```
<SimName>/.cactup/                 simulation-level metadata           [cactup-owned]
  simulation.toml                  (replaces SIMFACTORY/properties.ini at sim level)
  exe                              hard link → CACHE/exe/<build-id> (the frozen
                                   binary for this sim's config; a plain copy only
                                   if CACHE is on a different filesystem — §8.1)
  cfg/  par/                       master copies of the optionlist + thornlist, and
                                   of the parfile (§8.2)
output-%04d/.cactup/               restart-level metadata              [cactup-owned]
  restart.toml                     (replaces restart properties.ini; absorbs the
                                    old timestamp/simulation mark files)
  submit-script  run-script        substituted scripts for this restart
                                   (cactup-owned `.cactup/` files → kebab-case per §4.2;
                                    the CamelCase `SubmitScript`/`RunScript` elsewhere in
                                    this doc name the MDB *variant* artifacts, not these)
  running.lock                     per-restart liveness marker held by a live run
  heartbeat                        mtime touched periodically by a live run
```

**Metadata dir name & simulation detection (D10 — greenfield).** cactup uses
`.cactup/`, never `SIMFACTORY/`. A directory is recognized as a **cactup
simulation iff it contains `.cactup/simulation.toml`** (this replaces
simfactory's "has `SIMFACTORY/properties.ini`" test), and cactup only ever looks
at directories its per-installation registry (§8.1) points to. cactup does
**not** read, migrate, list, or otherwise touch pre-existing simfactory
simulations — they are simply invisible to cactup. (Consequence to accept: a
user mid-migrating from simfactory manages old sims with old simfactory and new
sims with cactup; there is no interop. If that proves painful, a one-shot
`cactup sim import` could be added later, but it is explicitly out of scope.)

**Schema versioning (backward-compatible reads).** Every cactup TOML
file (`simulation.toml`, `restart.toml`, `cactup-config.toml`,
`installation.toml`, `simulations.toml`) — and the global `database.json` — carries
a top-level integer **`schema`** as its first key. The policy matches §2.1: **a
newer cactup binary MUST read any older `schema` it ever shipped** (reads are
backward-compatible by contract), so upgrading cactup never orphans an existing
installation, config, or in-flight simulation/chain — this is what lets the
compute-node `sim run --restart-id` (§8.3.1) safely read metadata written by a
possibly-older login-node binary across an upgrade. cactup refuses only a `schema`
**newer** than it understands, with guidance to upgrade. The `schema` integer is
bumped **only** on a breaking on-disk change; during active prototyping it stays
fixed so iterating on the code needs no data migration. In-place *up-migration* of
old files is still out of scope for v1 (the contract is that new binaries *read*
old schemas, not that they rewrite them); a migration story is designed if and
when a breaking bump happens.

`simulation.toml` (sim level) carries `schema`, the keys simfactory wrote at
create (`simfactory-docs.txt` §15) — `machine`, `simulation-id`, `sourcedir`,
`configuration`, `config-id`, `build-id`, `executable`, `optionlist`, `parfile`
— plus cactup's `alias`. (No `testsuite`/`select-tests` keys on `simulation.toml`:
a simulation is never a testsuite — testsuite state lives in the `cactup test`
subsystem's own `test.toml` under test-home, §11.8 — D3.) `restart.toml` carries
`schema` plus the submit/run keys
(`nodes`, `tasks`, `tpn`, `cpus`, `queue`, `allocation`, `walltime`,
`checkpt-buffer`, `job-id`, `chained-job-id`,
the last observed `status`, the resolved **run** `universe` (name + expanded
wrapper, or absent for the host context — §4.8, so the compute-node run applies it
without re-reading the MDB), the frozen `[vars]` table (the §6.3 variable set)
and `[knobs]` table (the effective knob snapshot, §5.1 — both so the compute
node rebuilds its whole substitution context from disk, D11), and the
creation/marking timestamps that simfactory kept in the separate
`timestamp`/`simulation` mark files — folded in here). `build.toml` (§7.9) and
`test.toml` (§11.8) carry the same `[vars]`/`[knobs]` pair for the same reason.
Job id lives in `restart.toml` (`job-id`); there is no `job.ini` (D10 — no legacy
fallback to read).

TOML naturally stores lists, fixing the simfactory "list values weren't stored as
blocks" bug (`simfactory-docs.txt` §15).

**Liveness files.** `running.lock` is a per-restart lock (link-based, §2.3)
acquired by a live `sim run` for the duration of the run; `heartbeat` is a file
whose mtime that run touches periodically. The stale-restart reaper (§8.3)
consults both — plus live job status — before reaping, so a compute job that is
briefly invisible to the scheduler is not mistaken for dead. These are cactup
bookkeeping, not part of the output contract. simfactory's reaper throttles (skip
sims < 60 s old, re-clean every 30 s; `simfactory-docs.txt` §14.4) are **kept**,
since the reaper now runs on every `submit`.

---

## 10. Job submission & scheduler abstraction

Preserved from simfactory (`simfactory-docs.txt` §8 scheduler keys, §14.7). The
machine `meta.toml` carries `submit`, `get-status`, `stop`, `submit-pattern`,
`status-pattern`, `queued-pattern`, `running-pattern`, `holding-pattern`,
`exec-host`/`exec-host-pattern`, `stdout`/`stderr`/`stdout-follow`. cactup:

- Submits via `submit` (with `@SCRIPTFILE@` = the substituted SubmitScript),
  parses the job id via `submit-pattern` (group 1).
- For `cactup build submit --block` (§7.9), submits via the optional
  `blocking-submit` instead, when the machine declares one — the same command
  shape as `submit` except that it does not return until the job is over. The
  same `submit-pattern` still parses the job id, but incrementally, matched
  against the output line by line as it arrives rather than once at the end,
  so the id is captured while the job may still be running for hours. The
  command's exit status is not the build's verdict — only whether a job id
  ever appeared is; the verdict itself comes from the attempt's own record,
  read back after the wait. A machine with no `blocking-submit` still supports
  `--block`: it submits normally via `submit` and polls the attempt for an
  outcome instead of blocking natively.
- Queries status via `get-status` (with `@JOB_ID@`) and classifies via the
  pattern regexes into `R`/`Q`/`H`/`U`/`E`.
- Stops via `stop` (with `@JOB_ID@`).
- The generic "machine" describes a workstation with no batch system (`submit` =
  local `nohup`/exec, `get-status` = `ps`, `stop` = `pkill`) — preserved.

`cactup sim show` maps statuses to ACTIVE / RUNNING / QUEUED / PRESUBMITTED /
FINISHED / ERROR / INACTIVE as simfactory's `list-simulations` did.

---

## 11. Test suites (D3)

Cactus ships each thorn with a `test/` directory of reference data, and the
flesh exposes a `make <config>-testsuite` target (`simfactory-docs.txt` §13.7)
that runs those tests and diffs the output against the reference. simfactory
drove this by **overloading** its normal simulation machinery — `sim create
--testsuite` with an empty-parfile sentinel, a `testsuite`/`select-tests`
property on the simulation, a `copyTestsuiteData` step that built a **monster
simulation directory** (`output-NNNN/exe/`, a `copytree` of the whole built
config, an `rsync` of arrangement test data, and a `ln -s . output-0000-active`
self-link special case). cactup **keeps the capability but not the
entanglement**, per the two requirements that motivate this section:

1. **A first-class `cactup test …` command tree**, parallel to but separate from
   `sim`. You run tests with `cactup test run`/`test submit` against any built
   config — there is **no separate test-config kind**.
2. **A separate, configurable output root.** Test output lands under a dedicated
   **test-home** (`tests/`), *not* inside `simulations/` and *not* inside a
   simulation directory. Its location is a machine key with a home-dir fallback,
   exactly like `simulation-home` (§8.1) and `install-home` (§4.2).

The rest of this section defines the model, the `test = true` marking that lets
one machine ship both normal and test run/submitscripts, the CLI, the on-disk
layout, and what is deliberately dropped from simfactory's version.

### 11.1 Model & concepts

A testsuite is just another way to run an already-built config, so there is **no
test-config kind** and **no separate active pointer** — only two concepts:

- **the config under test** — any ordinary config on disk
  (`<Cactus root>/configs/<name>/`, §7). `make <config>-testsuite` runs that
  config's live built binary against the thorns' `test/` reference data, so a
  test run needs nothing a normal `cactup build` doesn't already produce. `test
  run`/`test submit` target `--config C`, defaulting to the installation's
  **active config** (§7.4) — the same config a bare `sim run` would use. There is
  no `test build`, no `test use`, and no `active-test-config`.

  A config built with a DEBUG optionlist (to surface assertion/bounds errors the
  testsuite is meant to catch) is just a config built with `cactup build
  --variant <debug>` — see §4.4 optionlist selection; the old test-optionlist
  partition is gone.
- **test run** (a.k.a. **test-sim**) — one execution of a config's testsuite
  against a chosen topology and test selection. It is the analog of a
  simulation but **much simpler**: one-shot, no restarts,
  no walltime chaining (§11.6). Test runs live under **test-home** (§11.5) and
  are addressed by name via `cactup test …` (`test delete <name>` in the
  required surface).

### 11.2 Marking test scripts (`test = true`) and resolution

A machine typically needs **different** runscripts and submitscripts for
testsuites than for production (a test runscript drives `make <config>-testsuite`
and exports `CCTK_TESTSUITE_RUN_*`, §11.6, rather than `mpirun`-ing a parfile).
cactup lets one machine ship both, keyed by a single `test = true` marker on the
script variant, and resolves the right one by the rules the requirements specify.
(Optionlists are **not** part of this: they are chosen purely by §4.4 — a config
under test uses whatever optionlist its `cactup build` selected.)

**Where the marker lives.** Run/submitscripts have no header of their own, so
the marker goes on the `meta.toml` variant entry, using the inline-table form
(§4.2):

```toml
[variants.runscript]
"cpu"      = { queues = ["checkpt", "single"], default = true }              # normal default
"test-cpu" = { queues = ["checkpt", "single"], test = true, default = true } # test-partition default (see resolution below)
```
(`test = true` and `default = true` compose with each other and with
`universe = "…"` / `build-universes = […]` in the same inline table.)

**Partitioning.** For each script kind (runscript / submitscript) cactup splits
the machine's variants into a **normal** set (no marker) and a **test** set
(`test = true`). Normal commands (`sim run`, `sim submit`) consider **only the
normal set** — a test variant is *never* used for a normal run. Test commands
(`test run`, `test submit`) prefer the **test set** but **fall back to the normal
set when the test set is empty**. This is the asymmetry the requirements call
out: *"if there is no testsuite script but there is a regular one, use that for
tests, but not vice versa."*

**Resolution within the chosen set** (selected at `test run`/`test submit` time,
per the chosen queue **and** the config's build universe): the chosen set (test,
or normal on fallback) is first filtered to variants compatible with the
config's build universe — `"host"` when the config records none — using the
same `build-universes` compatibility list and filtering rule that governs `sim
run`/`sim submit` (§4.4, §4.8); then cactup picks, among the survivors, the
variant whose `queues` include the chosen queue, or (absent a mapping) the
test-partition default (the `test = true`, `default = true` variant, or the
sole test variant). If the test set is empty, resolve against the normal set
exactly as a sim does (§4.4: filter by universe, then queue → variant, else
the `default = true` variant). `--variant V` on `test run`/`test submit` is
the escape hatch that forces the entry named `V` in each map (it must be a
test-marked variant when the test set is non-empty, and compatible with the
build universe, or `V`'s resolution errors naming the universe); the
runscript and submitscript maps are resolved independently, and `V` names the
entry in each.

**Validation at MDB load** extends §4.2's "every queue is served" check
**per-partition** — and, within each partition, per the same **per-universe-context**
rules §4.2 defines for `build-universes` lists: if a machine defines *any* test
runscript (or submitscript) variant, then every queue in `[queues.*]` must be
served, for universe context `"host"`, by some test variant of that kind or by
the test-partition default; a machine that defines **no** test variants of a
kind is fine (tests borrow the normal partition, already validated). As with
the normal partition, coverage for a declared non-host universe is *not*
checked at load time — a gap there instead surfaces as the same
selection-time error (§4.2) the first time a `test run`/`test submit` actually
needs it.

### 11.3 CLI surface

Added to §3 (all operate on the **active installation**; the run/submit topology
flags are the §8.5 set):

```
cactup test run    [--config C] [--variant V] [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <TOPOLOGY…> [<test>…]
cactup test submit [--config C] [--variant V] [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <TOPOLOGY…> [<test>…]

cactup test list       [--long] [--all]           # list test runs (--all: across installations)
cactup test show       <name>                      # show one test run in detail
cactup test stop       <name> [-f]                # stop a queue-submitted test run
cactup test delete     <name> [-f] [--purge]      # move a test run to test-home TRASH/ (--purge to remove)
```
(A config is built with `cactup build` — there is no `test build`.)

Notes:
- `[<test>…]` is the optional test selection (`test run` / `test submit`). Omitted
  ⇒ run **all** tests (the successor to simfactory's `--select-tests all`
  default). A selector is a test name, a thorn (`arrangement/Thorn`), or an
  arrangement; cactup passes the resolved selection to the flesh harness (§11.6).
- `--config C` defaults to the **active config** (§7.4); both `test run` and
  `test submit` fail fast in the null-config state with guidance to
  `cactup build`. The config must be a complete build.
- Unlike `sim submit`/`sim run`, there is **no parfile argument and no implicit
  create** — a test run's "parfile" is the thorn test data, chosen by `[<test>…]`.
  (This is where simfactory's empty-parfile `""` sentinel goes away entirely.)
- `-f`/`--overwrite`/`--force-queue`/`--universe`/`--no-universe` carry the same
  meanings as on `sim run`/`sim submit` (§3, §4.4, §4.8).

### 11.4 Building the config under test

There is no `test build`. A testsuite runs the binary that `cactup build`
already produces (`make <config>-testsuite` invokes it), so any complete config
is runnable as-is. Nothing about a build is test-specific: the `configs/<name>/`
layout (§7.2), the make flow, the optionlist render (§7.8), and the metadata
(§7.4) are exactly as §7 describes.

If you want the testsuite to run against a DEBUG binary (assertions / bounds
checking, to surface errors the tests exist to catch), build the config with a
DEBUG optionlist variant: `cactup build <name> --variant <debug>` (§4.4).
mel5, for example, ships `default` (OPTIMISE, the implicit pick) and `debug`
(DEBUG + OPTIMISE); the latter is just a normal variant reached with
`--variant debug`.

Deleting a config that test runs reference: `config delete` (§7.1) warns and
refuses without `-f` when registered test runs point at it — unlike simulations
they hold no frozen exe, so re-running their testsuite needs the config rebuilt —
and GCs the test-home executable cache alongside sim-home's when a build-id
becomes orphaned (§8.1).

### 11.5 Where test runs live (test-home) — the second requirement

**New machine key `test-home`** in `[paths]` (§4.2), alongside `simulation-home`,
`install-home`, `scratch-home`. It behaves **exactly** like `simulation-home`:

- optional, `@USER@`-substituted, made absolute;
- omitted ⇒ falls back to **`~/.cactup/tests`**;
- real clusters point it at fast/large storage (e.g.
  `test-home = "/work/@USER@/tests"`).

Like sim-home, the **effective test-home is resolved once at install time** as
`<machine test-home>/<alias>` (or `~/.cactup/tests/<alias>`) and recorded in
`installation.toml` (§8.1) so `test` subcommands never re-derive it. It is
exposed to scripts as `@TEST_HOME@` (§11.9).

**On-disk layout** (deliberately *not* the simulation layout of §9):

```
<test-home>/                                   = <machine test-home>/<alias>, or ~/.cactup/tests/<alias>
  CACHE/exe/<build-id>                         (shared-semantics executable cache — see below)
  TRASH/<test-run-id>/                         trashed test runs (test delete, §11.7)
  <config>/<TestName>/                         one dir per test run (grouped by the config under test)
    log.txt                                    test-run log (same [LOG:…] format as §12)
    results-%04d/                              numbered result sets (newest is "active")
      <flesh testsuite output>                 pass/fail reports + diffs the harness writes
      summary.log                              flesh testsuite summary (as produced by the target)
    results-NNNN-active                        RELATIVE symlink → results-NNNN
    test.out / test.err                        foreground `test run` stdout/stderr
    submit-script / run-script                 substituted scripts for the latest run
    .cactup/                                   test-run metadata (§11.8)   [cactup-owned]
```

Rationale for the divergences from §9:
- **`results-%04d`, not `output-%04d`.** These are testsuite result sets, not
  Cactus simulation restarts, and no external analysis tool should mistake a
  `tests/` tree for a `simulations/` tree. Keeping a **numbered, newest-active**
  scheme (mirroring the familiar restart convention, and reusing the §9.2 active
  symlink mechanics — one relative `results-NNNN-active` link, exactly-one-active)
  lets you re-run a config's tests and keep prior result sets for comparison,
  without pulling in restart/checkpoint machinery. Each `test run`/`test submit`
  allocates the next `results-%04d` and makes it active; `-f`/`--overwrite`
  overwrites the active one instead of allocating a new set.
- **No `output-NNNN/exe/` copytree, no arrangement `rsync`.** This is the "monster
  directory" cactup drops (§11.10). `make <config>-testsuite` is driven from the
  Cactus source tree (where `configs/<config>/` and the thorns' `test/` data
  already live); cactup only **redirects the harness's result output** into the
  active `results-%04d` under test-home (§11.6), leaving the source tree clean.
- **Executable cache.** A test run does **not** freeze a private copy of the
  binary the way a simulation does (§8.1): a testsuite runs `make
  <config>-testsuite`, which uses the config's live `<Cactus root>/configs/<config>/exe`,
  and a test run is short and one-shot, so the "survive a rebuild in flight"
  guarantee that motivates a frozen `.cactup/exe` (§8.1) does not apply. The
  test-home `CACHE/`/`TRASH/` exist for symmetry and for the cache-GC accounting
  (§11.4 counts test runs as live `build-id` references so `test delete`/`config
  delete` don't reap a binary a queued test still needs), but a test run's
  `.cactup/` holds no `exe` link. (**ASSUMPTION:** using the live built binary is
  the simpler, correct-for-one-shot choice; flag if you'd rather freeze it for
  strict reproducibility under concurrent rebuilds.)

### 11.6 `cactup test run` / `cactup test submit`

The run/submit split mirrors §8.3/§8.4 (submit → queue; run → foreground /
compute-node re-invocation), but the body is the **simplified, one-shot** path:

1. Resolve config = `--config` or the active config (fatal in null-config).
   Locate its built binary in `<Cactus root>/configs/<config>/` (fatal if the
   config is incomplete — no `config-data/cctk_Config.h`, §7.2).
2. Enforce the built config's `compatible-queues` against the chosen queue (§4.4 /
   D12) exactly as a sim does; `--force-queue`/`-f` overrides. The config
   must have been built for the resolved machine (§7.4);
   `--ignore-machine`/`-f` overrides.
3. Resolve the **test** submit/run script variants for the chosen queue (§11.2)
   and the **run universe** (§4.8, same precedence as §8.3), recording the run
   universe in the test-run metadata so a queued run applies it on the compute
   node without touching the MDB (D11).
4. Allocate the next `results-%04d` (or overwrite the active one with
   `--overwrite`/`-f`), make it active (§9.2 mechanics), and compute the topology
   variable set (§8.5).
5. **Drive the testsuite.** The (test) runscript is responsible for exporting
   `CCTK_TESTSUITE_RUN_COMMAND` and `CCTK_TESTSUITE_RUN_PROCESSORS`
   (`simfactory-docs.txt` §6.4) and invoking `make @CONFIGURATION@-testsuite
   PROMPT=no`, with the effective **run** env-setup applied (auto-prepended for
   `.sh`, author-emitted via `@ENV_SETUP@` for `.py` — §6.1). Because thorn
   tests are written for 1–2 MPI ranks, a testsuite run does NOT fill the node:
   with no `-n`/`-T`/`-t` flag, `TASKS` is the selected script variant's
   `tasks` setting (§4.2, mode-relevant script first), else **2**. cactup
   exposes the values the script needs: `@TASKS@` (→
   `CCTK_TESTSUITE_RUN_PROCESSORS`), the
   run-command pieces (`@EXECUTABLE@` and the machine's launcher, exactly as a
   normal runscript builds its `mpirun`/`srun` line — this is why tests get their
   own runscript), `@TESTSUITE_SELECT@` (the selection, default `all`), and
   `@TESTSUITE_RESULTS_DIR@` (the active `results-%04d`, §11.9).
6. **Redirect results into test-home, keep the source tree clean.** cactup points
   the flesh testsuite target's output at `@TESTSUITE_RESULTS_DIR@`. (**ASSUMPTION
   / implementation detail:** the exact hook depends on the flesh version — prefer
   the target's own output-directory option when present; otherwise cactup makes
   `configs/<config>/TEST/<machine>` a symlink into the results dir before
   running, so the harness writes straight into test-home and only a symlink is
   left in the tree. Either way cactup **never** does simfactory's `copytree` of
   the built config — §11.10.)
7. Parse the harness's pass/fail summary, record it in `test.toml`
   (`[results] passed/failed`, §11.8), print a one-line summary, and **exit
   non-zero if any test failed** (so `test run` is CI-usable). `cactup test
   show <name>` reprints the last run's results.

**No chaining, no restart bookkeeping.** A testsuite is a single
job: §8.8's auto-chaining and the stale-restart reaper **do not apply** (nor does
checkpoint recovery, but that is nobody's business in cactup now — §8.8).
`--wall-time` is a single scheduler
reservation for the one job (no splitting). This is the largest simplification
versus the simulation path and the reason tests get their own, thinner code path
instead of a `--testsuite` flag bolted onto `sim`.

**Compute-node re-invocation (`test submit`).** As with `sim submit` (§8.3.1),
the generated test submitscript re-invokes cactup on the compute node, passing the
run target explicitly so it needs neither the global DB nor the registry:

```
@CACTUP@ test run @TEST_NAME@ \
    --installation=@ALIAS@ --test-dir=@TEST_DIR@ --machine=@MACHINE@ \
    --results-id=@RESULTS_ID@
```

`--test-dir` (the absolute test-run directory) + `--results-id` fully identify the
result set; `cactup test run` in this mode reads the config, topology, selection,
and run universe from `.cactup/test.toml` (§11.8) and drives the testsuite as
above. It holds the per-run `running.lock` and touches `heartbeat` (§11.8) for the
same liveness reasons as a sim run, but there is no reaper/chain handoff to
perform. `--no-universe`/`--universe` and the §4.8 run-universe wrapping apply to
the `make …-testsuite` shell exactly as they wrap a normal runscript.

### 11.7 Managing test runs

- **`cactup test list [--long] [--all]`** — lists the active installation's
  test runs from the `tests.toml` registry (§11.8), or unions every
  installation's with `--all` (the §8.1 cross-install discipline). Per-run display
  state is coarser than a sim's (no restart chain): `RUNNING`/`QUEUED`/`HOLDING`
  from the active result set's live job status, `DONE (passed/failed)` once the
  job is `U` and a summary was recorded, `ERROR` on `E`.
- **`cactup test show <name>`** — one test run in detail.
- **`cactup test stop <name> [-f]`** — the §8.6 `stop` semantics for the
  active result set's job (there is no `clean`: no checkpoints/Formaline tarballs
  to dedup, no chain to deactivate beyond removing the active symlink).
- **`cactup test delete <name> [-f]`** (the required surface) — the §8.7
  `sim delete` semantics against test-home: refuse a live run without `-f`
  (`-f` stops it first), `shutil.move` the run dir into
  `<test-home>/TRASH/<test-run-id>/`, remove its `tests.toml` entry, and run
  executable-cache GC (a test run counts as a live `build-id` reference until
  trashed/purged; `--purge`/`-f` removes outright and drops the reference
  immediately). `test-run-id` reuses the §9.1 `simulation-id` string format
  (`test-<name>-<machine>-<hostname>-<user>-<timestamp>-<pid>`) so it names the
  `TRASH/` subdir unambiguously.
- To delete the config a test run used, `cactup config delete <name>` (§7.1);
  it warns when registered test runs still reference it.

### 11.8 Metadata files

Following D6/§9.3 (per-thing on-disk TOML, `schema`-versioned, backward-compatible
reads), the test subsystem adds:

- **`installation.toml` gains one key** (§8.1): `test-home` (the resolved
  per-alias test output root, resolved at install like `sim-home`). There is no
  separate active-config pointer — test runs use `active-config` (§7.4).
- **Per-installation test-run registry** `<installation home>/.cactup/tests.toml`
  — the test analog of `simulations.toml` (§8.1): `<TestName>` → `{ dir,
  config, created }`. The `test` management subcommands locate a run by name through it;
  mutations go under the per-installation lock (§2.3, item 5), which now also
  guards `tests.toml`.
- **Per-test-run metadata** `<TestName>/.cactup/test.toml`:
  ```toml
  schema = 1
  name = "et-tests"
  config = "et-cpu"                 # the config under test
  config-id = "…"
  build-id  = "…"
  machine = "mel5"
  alias = "et-dev"
  test-run-id = "test-et-tests-mel5-…-…"
  select = "all"                    # or the resolved selector list
  # topology + scheduler, as a restart records them (§9.3), for the single job:
  queue = "local"
  nodes = 1
  tasks = 4
  tpn = 4
  cpus = 1
  walltime = "1:00:00"
  allocation = "…"
  job-id = "…"
  status = "U"                      # last observed live status (cached; §8.6)
  universe = { name = "et-sif", wrapper = "…" }   # resolved run universe, or absent
  [vars]                            # frozen §6.3 variable set (D11)
  TASKS = 4
  [knobs]                           # frozen knob snapshot for @KNOB(…)@ (§5.1, D11)
  allocation = "hpc_xxx"
  [results]                         # written when a run completes (§11.6)
  passed = 42
  failed = 3
  results-id = 0                    # the results-%04d this summary belongs to
  [timestamps]
  created = "…"
  submitted = "…"
  finished = "…"
  ```
  Plus the liveness files under the same `.cactup/`: `running.lock` and
  `heartbeat` (§9.3 semantics; consulted by `test stop`/`show`, not by any
  reaper). There is **no** `exe` link (§11.5) and no `restart.toml` (a test run
  has result sets, not restarts).

### 11.9 Variables added

The §6.3 canonical variable set gains a small **test-only** group, available to
test runscripts/submitscripts (and their `.py` variants) — never leaked into
normal sim/config substitution:

- `TEST_HOME` — the per-alias test output root (§11.5), the test analog of
  `SIM_HOME`.
- `TEST_DIR` — absolute path to this test run's directory (passed to the
  compute-node re-invocation as `--test-dir`, §11.6), the analog of
  `SIMULATION_DIR`.
- `TEST_NAME` — the test-run name (analog of `SIMULATION_NAME`).
- `RESULTS_ID` — the active `results-%04d` id (analog of `RESTART_ID`).
- `TESTSUITE_RESULTS_DIR` — absolute path to the active `results-%04d`, where the
  harness must write (§11.6, step 6).
- `TESTSUITE_SELECT` — the test selection (`all` or the resolved selector list,
  §11.3), the successor to simfactory's `select-tests`.

`CONFIGURATION` (§6.3) is the config name for `make @CONFIGURATION@-testsuite`;
`TASKS` (§6.3) feeds `CCTK_TESTSUITE_RUN_PROCESSORS`; `EXECUTABLE`, `SOURCEDIR`,
`ENV_SETUP`, and the topology/scheduler variables are the existing §6.3 ones. No
new *build-time* variables are needed — the config is built by ordinary
`cactup build` (§7.8).

### 11.10 Deliberately not ported from simfactory's testsuite

- **The monster simulation directory.** No `output-NNNN/exe/` dir, no `copytree`
  of the built config into the run dir, no `rsync` of arrangement test data
  (`copyTestsuiteData`, `simfactory-docs.txt` §13.7). cactup runs the testsuite
  from the source tree and redirects only its *results* into test-home (§11.5,
  §11.6).
- **The `output-0000-active → .` self-link special case.** cactup uses the normal
  §9.2 active-symlink mechanics against `results-%04d`; there is no self-link hack.
- **The empty-parfile `""` sentinel** and the `--testsuite`/`--select-tests` flags
  bolted onto `sim create`/`sim submit`. Test selection is the positional
  `[<test>…]` on `test run`/`test submit` (default all), and there is no parfile.
- **Overloading simulation state.** No `testsuite`/`select-tests` keys on
  `simulation.toml` (§9.3 already notes their absence); testsuite state lives only
  in `test.toml` (§11.8), and testsuite output lives only under test-home — the
  two requirements this section exists to satisfy.

---

## 12. Logging & errors

Port of `simfactory-docs.txt` §22, adapted to Rust (`anyhow`, existing style):
- `log.txt` per simulation, append mode, preserved line format
  (`[LOG:<ts>] <a>::<b>`) so any tooling that reads it keeps working.
- User-facing errors via the existing `anyhow` + `colored` conventions in
  `src/main.rs`. cactup does not adopt simfactory's single global `log/` file.
- Exit codes: `0` success, non-zero on error (cactup may use distinct codes per
  failure class — an improvement over simfactory's uniform `1`).

---

## 13. Data-format summary

| Artifact | simfactory | cactup |
|----------|-----------|--------|
| Global state | n/a (per-tree) | `~/.cactup/database.json` (JSON) — installs, active install, knobs |
| Machine def | `mdb/machines/<m>.ini` | `mdb/<m>/meta.toml` (TOML) |
| Machine discovery | `aliaspattern` regex | `mdb/<m>/hostname.regexp` (Rust regex), then `mdb/<m>/discover.py` |
| Config DB / defs | `etc/defs.ini` + `etc/defs.local.ini` | **removed**; → knobs + machine meta |
| OptionList | `mdb/optionlists/<m>.cfg` | `mdb/<m>/optionlists/<variant>.toml` (rendered to native `.cfg` before make — §7.8) |
| SubmitScript | `mdb/submitscripts/<m>.sub` | `mdb/<m>/submitscripts/<variant>.{sh,py}` |
| RunScript | `mdb/runscripts/<m>.run` | `mdb/<m>/runscripts/<variant>.{sh,py}` |
| BuildSubmitScript | n/a (`cactup build` always ran on the login node) | `mdb/<m>/buildsubmitscripts/<variant>.{sh,py}` — optional; only on machines that hand builds to the scheduler (§7.9) |
| Parfile | user-supplied `.par` / executable `.rpar` | user-supplied `.par` (literal `@NAME@`) / `.py` variant (JSON-on-stdin, emits `.par` to stdout — §6.1/§6.2) |
| Config metadata | `configs/<name>/properties.ini` | `configs/<name>/cactup-config.toml` |
| Build attempt | n/a (build output went to `cactup-build.log` in the config dir) | `configs/<name>/.cactup-builds/%04d/build.toml` + frozen `build-script` (and `submit-script` when queued) + `build.out`/`build.err` — one per build, foreground or queued (§7.9) |
| Sim metadata | `SIMFACTORY/properties.ini` | `.cactup/simulation.toml`, `.cactup/restart.toml` (schema-versioned) |
| Sim detection | dir has `SIMFACTORY/properties.ini` | dir has `.cactup/simulation.toml` (greenfield — D10) |
| Sim output dirs | `output-%04d/…` | **identical (preserved)** |
| Active restart | `output-NNNN-active` symlink | **identical (preserved)** |
| Substitution | `@NAME@` + `@(expr)@` + `@ENV()@` | **`@NAME@` + `@ENV(NAME)@`** (unset/empty env = hard error); `.py` for logic (JSON-on-stdin convention, §6.1) |
| cactup binary var | `@SIMFACTORY@` | `@CACTUP@` |
| Machine detection | `aliaspattern` regex on hostname | `hostname.regexp` first, `discover.py` second (all scripts in one `python3`); result cached in DB as one `detected` record stamped with hostname + login session, re-verified when either changes (§4.3) |
| Per-installation state | n/a | `<installation home>/.cactup/installation.toml` (active config, sim-home, test-home, root-dir) + `simulations.toml` (name→dir registry) + `fetch-state.toml` (per-repo URL/branch/HEAD from the last fetch — §3.2) + `<installation home>/installation-source.th` (pristine as-fetched thornlist, the hand-edit guard baseline — §3.2) |
| Sim root key | machine `basedir` | machine `simulation-home` (optional; falls back to `~/.cactup/simulations`) — §8.1 |
| Test-suite command | `sim create --testsuite` (overloads `sim`) | `cactup test run`/`submit` against any built config (own command tree — §11) |
| Test output root | inside a simulation dir (`output-NNNN/exe/…`) | machine `test-home` (optional; falls back to `~/.cactup/tests`) — §11.5 |
| Config under test | n/a (a `testsuite` property on a sim) | any built config; `--config C` defaults to the active config (no separate test-config kind) — §11.1 |
| Test run metadata | sim `properties.ini` + `output-NNNN/exe/` copytree | `<test-home>/…/<name>/.cactup/test.toml` + `tests.toml` registry (§11.8); no copytree (§11.10) |
| Test-script marking | separate faked machine defs | `test = true` on the meta.toml run/submitscript variant entry (§11.2) |
| Install root key | machine `sourcebasedir` (source base; also sync/disambiguation) | machine `install-home` (optional default install prefix; falls back to `~/.cactup/cacti`; `--install-prefix` overrides) — §4.2 |
| Locking | none (per-tree) | `link()`-based (NFS-safe) global-DB lock + per-sim lock + per-config build lock + per-installation lock + per-installation fetch lock (heartbeat) (D11, §2.3) |
| Execution universe | faked via separate machine defs (e.g. `db-sing-*`) | `[universes.*]` command-wrapper in `meta.toml`; wired for `cactup build`, `sim run`, and `sim submit` (§4.8) |
| Queue-submitted build | faked via a `[universes.*]` wrapper around `make` (the qbd hack this design retires — §7.9) | first-class: `[build].default-action`/`queue`/`walltime`/… + `[variants.buildsubmitscript]` (§4.2); `build submit` goes through the same `Scheduler` abstraction as `sim submit` (§10) |

---

## 14. Open items / assumptions to confirm

Each is marked **ASSUMPTION** inline above; collected here:

1. MDB source: git clone into `~/.cactup/mdb` (prod) vs hard-coded dev path; a
   `--mdb-path` override (§2.2, §4). **Resolved by D14 (§17):** dist builds
   sync the `mdb` branch into `~/.cactup/mdb/{repo,<sha>,gen-<N>}`; dev builds
   read the repo's `mdb/`.
2. Python runtime: shell out to `python3` for `discover.py` (one process for
   all of them) and `.py` script variants rather than embedding CPython (§4.3).
3. `interactive` command dropped for v1 (§3.1).
4. `--virtual` prebuilt-executable build kept (§7.7).
5. `sim delete` moves to `TRASH/` by default; permanent removal behind an
   explicit flag (§8.7).
6. Short job-name obfuscation word lists dropped; truncation rules kept (§9.1).
7. SMT not exposed as a topology flag in v1 (§8.5).
8. `user`/`email` auto-derived (like simfactory `setup`) and overridable as
   knobs (§5).
9. `.py` variant calling convention is JSON-on-stdin with a fixed preamble
   binding globals (§6.1) — flag if you'd prefer env vars or a generated
   assignment preamble instead.
10. Checkpoint buffer defaults to `max(reserved-walltime/24, 10 min)`, overridable
    with `--checkpt-buffer`; it only sets the *exposed* `@CHECKPOINT_WALLTIME@`
    hint variables (hard wall − buffer). cactup reserves the full hard wall and
    does not inject or consider any Cactus termination parameter (§8.8).
11. Test-suite support is a first-class `cactup test …` tree with its own
    configurable **test-home** output root; it runs against any built config
    (default the active config), with no separate test-config kind (D3, §11).
    Softer choices inside §11 to confirm:
    (a) a testsuite runs the config's normal binary; a DEBUG run is just `config
        build --variant <debug>` (§4.4, §11.4);
    (b) a test run uses the config's **live** built binary rather than freezing a
        private copy, since it is one-shot (§11.5);
    (c) results are redirected into test-home via the flesh testsuite target's
        output-directory option when available, else a `configs/<config>/TEST/…`
        symlink — the exact hook depends on the target Cactus flesh version
        (§11.6). None of these change the two hard requirements (separate command
        tree, separate configurable output root).
12. Schema/version handling: newer binaries **must read older `schema`**
    (backward-compatible reads), refusing only a `schema` newer than understood;
    the `schema` integer is bumped only on a breaking change and in-place
    up-migration is deferred (§2.1, §9.3).
13. Universes (§4.8): generic command-wrapper (`wrapper-argv` prefix form default,
    `wrapper`/`@COMMAND@` template power-form), wired for all three seams —
    `cactup build`, `sim run`, and `sim submit` — via one `[universes.*]` registry
    and a uniform resolution rule. The **run** universe is the load-bearing one
    (containerized simulations); the **submit** universe is included for
    generality but is rarely useful (it wraps `sbatch`, not the job — see §4.8).
    A config built in a universe **coerces** its simulations into that same
    universe at run time by default (§4.8 precedence step 2), overridable per-CLI
    or opted out via the optionlist `[cactup].coerce-run-universe = false`.
    Flag if you'd prefer a different representation or want the temp-script-file
    (`@COMMAND_FILE@`) power-form instead of the `@COMMAND@` template.

---

## 15. Mapping checklist (every simfactory subsystem accounted for)

| simfactory subsystem (`simfactory-docs.txt` §) | cactup disposition |
|---|---|
| 2–3 bootstrap/dispatch | Rust binary + clap (§3) |
| 4 commands | mapped (§3.1) |
| 5–6 option system/precedence | clap + knob precedence (§5.1) |
| 7 INI parser | TOML (serde) |
| 8 machine DB keys | `meta.toml` (§4.2), remote/archive keys dropped |
| 9 machine detection | `hostname.regexp` / `discover.py` (§4.3) |
| 10 defs layering | removed; → knobs/meta (§5.2) |
| 11 `@VAR@` engine | literal `@NAME@` + `.py` escape hatch (§6) |
| 12 optionlists/run/submit | variants (§4.4, §6.2); test-marked variants (§11.2) |
| 13 on-disk layout | **preserved** (§9) |
| 13.7 testsuite layout / `copyTestsuiteData` | `cactup test` subsystem: test-home output root + `test.toml`; monster `output-NNNN/exe/` copytree **not** ported (§11) |
| 14 lifecycle | §8 |
| 15 properties.ini | `.cactup/*.toml` (§9.3) |
| 16 build | §7 |
| 17 sync | **dropped** (D1) |
| 18 remote/SSH | **dropped** (D1) |
| 19 archive | **dropped** (D2) |
| 20 setup decision trees | `cactup machine create` + `generic` fallback + autodetect (§4.6, §4.7); knobs (§5) |
| 21 distribute/bench tools | out of scope (external test harnesses) |
| 22 logging/errors | §12 |
| 23 bugs/quirks | fixed where noted (§6.1, §9.3) or N/A |
| 24 recommendations | honored (§9 contract, inferred state) |
```

---

## 16. Wisdom

`cactup wisdom` prints one random message from a corpus compiled into the
binary — a mix of **feature tips** (a concise description of a useful or
obscure cactup feature with a copy-pasteable call to action) and **zen
entries** (mature, practically applicable words of wisdom; direct quotes
carry visible `— Name` attribution as part of the printed text).

**Corpus** — `resources/wisdom.txt`, embedded via `include_str!`
(the file never ships to users; edits are compile-time only):

- Entries are separated by lines containing only `%`. Leading, trailing,
  and doubled separators are harmless (empty entries are dropped).
- Lines whose first non-whitespace character is `#` are comments — used to
  record the source of anything pulled from the internet (attribution in
  the file is required for such entries). Never printed.
- An entry whose first line is exactly `!zen` is a zen entry; the marker is
  stripped from display. Anything unmarked is a feature tip.
- The corpus is parsed and validated at **build time**: `build.rs` (sharing
  the parser in `src/wisdom_parse.rs` via `include!`) checks it (non-empty,
  both kinds present, no tabs, ≤ 100 columns, every zen entry attributed)
  and generates static `TIPS`/`ZENS` slices into `OUT_DIR`, so a malformed
  edit fails `cargo build` itself and the binary does zero parsing at run
  time.

**Random post-command wisdom** — after any *successful* command, cactup may
print one entry to **stderr**, each line dimmed, preceded by a blank line,
so it reads as decoration rather than output. Controls:

- `wisdom-frequency` knob (§5): `off` (never), `rare` (1/15), `normal`
  (1/8, the default), `chatty` (1/4), `always` (1/1).
- `wisdom-kind` knob (§5): `relevant` = feature tips only; `all` (default)
  = tips mixed with zen entries. When `all`, a zen entry is chosen 25% of
  the time (a dev-time constant in `commands/wisdom.rs`, not a knob).
- Suppressed when stderr is not a terminal (pipes, scripts, job logs), on
  the compute-node path (`sim run --sim-dir …` — D11 hermeticity: no DB,
  no decoration in scheduler logs), after the log-follow views (`sim log` /
  `test log` with `-f`/`-o`/`-e`, which end via Ctrl-C), after `cactup
  wisdom` itself, and after any failed command. Decoration must never fail
  a command: any problem in the hook (unreadable DB, empty corpus) is a
  silent no-op.

---

## 17. Distribution, self-update & MDB generations (D14)

### 17.1 What CI publishes

One workflow (`.github/workflows/ci.yml`; jobs `stamp`, `test`, `mdb-compat`,
`build`, `docs`, `site`, `publish-mdb`, `deploy`) runs on every push and PR;
only `master` publishes. The GitHub Pages root (`update-url`, default
`https://max-morris.github.io/Cactup`) holds:

```
index.html, users/…, authors/…, …    # the docs site (cactupdocs) at the root
cactup-init.sh                       # the installer
latest.json                          # the version manifest below
<target>/cactup                      # stable alias, for manual downloads
<target>/cactup-<build>              # immutable, what latest.json points at
<target>/cactup.sha256               # "<sha256>  cactup-<build>"; the installer
                                     # reads it first, then fetches that file
```

for `<target>` ∈ {`x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl`}.

```json
{ "build": "a1b2c3d", "date": "2026-09-24T12:00:00-05:00", "mdb_generation": 1,
  "targets": { "x86_64-unknown-linux-musl":
               { "path": "x86_64-unknown-linux-musl/cactup-a1b2c3d",
                 "sha256": "…", "size": 11534336 }, … } }
```

Readers are lenient (unknown fields ignored); `build` must match
`^[0-9a-f]{7,40}$` and `path` must be relative without `..`.

**Build id = the last commit that touched a build input** (`src build.rs
Cargo.toml Cargo.lock resources mdb/GENERATION mdb/generic`), abbreviated to
7 hex; `date` is that commit's committer date (`%cI`). A docs- or MDB-only
push therefore has the same id, and CI republishes the live binaries (fetched
from the current site and verified against `latest.json`) instead of
rebuilding. `--version` prints `0.1.0 (a1b2c3d 2026-09-24, mdb generation 1)`,
or `0.1.0 (dev build, mdb generation 1)` for an unstamped build.

**Ordering and caching.** `publish-mdb` (push `git subtree split --prefix=mdb`
to `refs/heads/mdb`) runs before `deploy`, so a binary of a new generation is
never live before an MDB commit of that generation exists. The reverse window
(an old binary sees the new generation before the new binary is live, up to
the ~10 min Pages CDN TTL) yields only the generation warning, whose text
says a release may still be propagating. The CDN caches each file
independently, so `latest.json` names the immutable `cactup-<build>` path, and
a 404 or checksum mismatch means "retry later", never an error. Runs are
serialized per ref (no cancellation on `master`), so two pushes cannot deploy
out of order.

### 17.2 Versioned binaries and self-update

Dist builds install as `~/.cactup/bin/cactup-<build>` with `bin/cactup` a
symlink to the current one. `@CACTUP@` is the versioned path (§6.3), so a job
keeps its exact build. An update: `LinkLock` on `bin/.update.lock`; download
into a temp file in `bin/`; check size and SHA-256 **before** executing
anything; run `<tmp> --version` (≤ 10 s, must print the build id); persist as
`cactup-<build>`; atomically replace `bin/cactup` with a symlink to it (safe
while the old binary runs); stamp the previous target `cactup-<old>.retired`.
Nothing is deleted automatically: a restart chain re-submits itself under
the build it started with, and every submit script it writes names that
file, so an automatic prune would break long chains. `cactup update --prune`
removes builds that are neither the link target nor the running executable
and whose `.retired` stamp is over 30 days old (fileserver clock, §2.3),
listing each one; a job whose build was pruned must be submitted again.

### 17.3 The update check and `cactup update`

Before dispatch, an interactive (stderr is a tty) dist build not on a
compute-node path (D11), not running `cactup knob` (the user is configuring
it) or `cactup update`, and not already re-executed (`CACTUP_UPDATED`) fetches
`latest.json` at most once per 24 h (`~/.cactup/update-check`, stamped on
success and on failure, but not when an automatic install finds the release
still propagating — missing, or not matching `latest.json` — so the next
command retries; connect 5 s, total 10 s; failures silent). Builds are
ordered by date (RFC 3339, compared as instants): a strictly later published
date is newer; an equal or unparseable date with the same build (one id a
prefix of the other) is up to date; anything else is the server being older,
never installed. Then per
`autoupdate`: `notify` prints one line (``cactup <b> is available (you have
<a>); run `cactup update` ``); `auto` installs (§17.2) when the running binary
is in `bin/` and named `cactup` or `cactup-<hex>`, then `exec`s the new
binary with the same argv and `CACTUP_UPDATED=1`; `off` does nothing.
`cactup update` does the same unconditionally (binary first, re-exec as the
new binary, then a forced MDB sync unless `--mdb-path`); `--check` only
reports. Dev builds refuse `cactup update`.

### 17.4 MDB sync

Dist builds resolve the system MDB through `mdb::sync` (dev builds read the
repo's `mdb/`; `--mdb-path` bypasses both). Layout under `~/.cactup/mdb/`:
`repo/` (bare, fetch-only clone of the `mdb` branch into `refs/mdb/head`),
`<sha>/` (exported trees), `gen-<N>` (relative symlink → `<sha>`, swapped
atomically), `.synced-<N>` (throttle stamp: `tip_generation`, `tip`,
`commit`), `.lock` (`LinkLock`). A sync is skipped when `gen-<N>` exists and
the stamp is younger than 6 h; otherwise, under the lock, after a 5 s TCP
preflight (skipped behind a proxy: an `*_proxy` variable, or git config's
`http.proxy`, `https.proxy` or `http.<url>.proxy`), cactup fetches the branch
(anonymous remote at `mdb-url`) on a watched helper thread (gix's http
transport has a 20 s connect timeout and reqwest's default 30 s idle timeout
on the headers and each body read; git://, ssh and file transports have no
timeout of their own; and gix checks the interrupt flag only between
phases). The watcher abandons the fetch as a network failure once no
progress has been reported for 30 s (the bound that matters for the
non-http transports), or 5 min have passed in all, so a server that accepts
the connection and never answers cannot hang every sync; on an interrupt it gives the fetch 700 ms to unwind
(dropping gix's lock and pack temp files), then fails "interrupted". It reads
`GENERATION` at the tip, and walks first-parent history back to the newest
commit whose `GENERATION` equals its own N (a missing file = generation 0,
stop). That commit is exported to `<sha>/` (verified to carry `GENERATION ==
N`) and `gen-<N>` is swapped to it. The stamp is written on success **and** on
network failure, so an offline host pays the timeout at most once per
interval; a failure with an existing link keeps it (one yellow line), a
failure with none is a hard error pointing at `cactup update` from a
networked host or `--mdb-path`. Trees no link references are pruned 24 h
after they were retired. A tip generation newer than N produces the loud
warning ("the machine database has moved to generation M; this cactup
(generation N) keeps using the last generation-N revision. Run `cactup
update` (if that reports up to date, a release is still propagating; retry
later)"). Compute-node paths (D11) never open the MDB, so never sync.

### 17.5 MDB generations

`mdb/GENERATION` (an integer, compiled into the binary as `MDB_GENERATION`)
and `mdb/GENERATIONS.md` (one `## Generation N` entry per generation: what
changed and how to migrate an overlay) version the published MDB. Unlike all
other on-disk state, the published MDB has deployed readers, so it carries a
compatibility promise, and the generation is its only mechanism.

**Bump rule (two directions).** Bump when (a) `mdb/` at the new commit would
fail to load or behave differently in the oldest binary of the current
generation, or (b) an overlay written for the current generation would fail
or behave differently in the new binary. Adding optional `meta.toml` keys
counts (the schema is closed). The `mdb-compat` CI job loads every machine in
both directions against the last commit that raised `mdb/GENERATION` to the
current number (a bump reverted later does not move it); the
rest (template variables, `.py` protocol, ignored optionlist header keys)
needs review. A unit test requires a `GENERATIONS.md` entry for every
generation up to N.

**Overlays** (`~/.cactup/machines/<name>/meta.toml`) carry
`[cactup] mdb-generation = N` (written by `machine create`). On load of a
user-layer machine: missing → one warning per process ("assuming generation
N"); older → `OverlayGenerationError` naming `GENERATIONS.md`; newer →
"requires a newer cactup; run `cactup update`". `machine list` shows a
refused overlay annotated instead of failing. A `--mdb-path` directory whose
`GENERATION` differs from the binary's is a hard error (a missing file is
accepted, for fixtures).

---

## 18. Build cache (D15)

One cache per cactup instance, shared by every installation and
configuration in it. The goal is that a new installation, or a configuration
rebuilt from scratch after an optionlist edit, compiles only what no build
of this instance has compiled before.

**Status.** Built in stages; the plan and the state of each stage are in
`design/build-cache/`. What exists now is the interposition (§18.2–§18.4)
and the keys (§18.5): with `build-cache = record`, cactup stands in front of
every object compile, works out the key it would be cached under, and logs
it; `cactup cache report` (§18.6) reads the logs. With `build-cache =
serve` it also serves objects from the store (§18.7) and publishes the ones
it compiles (§18.8); `audit` checks every hit by compiling anyway. `cactup
cache stats`, `gc` and `verify` look after the store (§18.9). Fortran is
cached for gfortran (§18.10); CUDA is still to come.

### 18.1 Rules

These hold for every stage, and code in `src/objcache/` is reviewed against
them.

1. **Never a stale object.** A hit must be byte for byte what the compile
   would produce here and now. A false miss is acceptable; a false hit is
   not. Anything the cache does not fully understand is not cached.
2. **Fail open.** The cache must never fail a build, and never change what
   gets compiled, with one exception: a serving cache adds the path map's
   flags to a compile it runs (§18.8), which changes the paths an object
   records and nothing else. The cache sits strictly below `make`: `make` and cactup
   (§7.8) still decide *whether* to compile and *with what*; the cache only
   answers *what the compile would produce*. Every failure inside the
   wrapper or the probe ends in the compile running as `make` asked.
3. **Nothing outside Cactus's object compiles.** No compiler variable and
   no recipe's environment may change, and no build other than Cactus's
   object compiles may behave differently: not an ExternalLibraries build,
   not a configure run, not dependency generation; `config-data` stays as
   configured. That rules out a `make CC=…` override and an optionlist
   rewrite; §18.3 is what is left. (The makes *above* the object sub-makes
   do read the fragment, and it does nothing there: see §18.3.)
4. **Quiet.** A wrapped compile's stdout and stderr are the compiler's own;
   the wrapper adds nothing. (A serving cache passes them on as they come,
   and a hit writes the stored ones, §18.8.) (With `SILENT=no` Cactus echoes its recipes,
   and the echoed compile line then shows the wrapper in front of the
   compiler — that is make's output, and the truth.) The build output says
   in one line that the cache stayed out of a build, or how many compiles
   went through it.
5. **Signals and exit statuses pass through.** The wrapper stands between
   `make` and a compiler: a stop signal must reach the compiler, an ignored
   one must stay ignored, and the wrapper must end the way the compiler
   ended.
6. **Hermetic on the compute node (D11).** The wrapper and the probe read
   the attempt's frozen settings, the configuration directory named in
   them (and `/proc/self/status`), and the store whose root is named in
   them (§18.7). Never the global DB, the registry, the MDB, or knobs:
   `prepare` resolves the store's root from its knob and freezes it.
7. **No eviction without being asked.** Old entries are what make reverting
   a thorn cheap.

### 18.2 What `prepare` freezes

With `build-cache` off, a build attempt and its build script are exactly
what they are without this section. Otherwise `prepare` writes
`<attempt>/cc/config.toml` (`objcache::BuildConf`): the mode, the versioned
cactup binary (`freeze::frozen_cactup`, as for `@CACTUP@`), the configuration
directory and the Cactus root, what keys objects to their platform
(§18.5): the machine, the build universe, and a SHA-256 of the build-phase
environment setup; the store's root (§18.7); and
whether keys use the path map (§18.8, knob `build-cache-relocate`). If that
cannot be written, the build goes on without the cache and says so.

The build script gains one step and changes one (`objcache::Staged`):

```sh
echo yes | make … <name>-config …          # unchanged: real compilers
make … <name>-clean                        # unchanged (only with --clean)
<probe step>                               # may only turn the cache off
<build step>                               # make <name>, reading inject.mk if allowed;
                                           #   then one line: how many compiles it recorded
make … <name>-utils                        # unchanged
```

### 18.3 The probe and the injected fragment

`cactup __cc-probe <attempt>/cc/config.toml` runs inside the build script,
after the configure and clean steps — so where the compiles will run
(compute node, container universe) and after
`config-data/make.config.rules` exists. It declines (exit 3, one line on
stderr, the build goes on uncached, nothing written) when it cannot do its
job there; a container that cannot see the binary at all makes the shell
report 126/127, which the script turns into the same kind of line.

Otherwise it writes `<attempt>/cc/inject.mk`, which `make` reads through the
`MAKEFILES` environment variable for the `make <name>` step alone, and
creates the configuration's `build/` directory if a `realclean` removed it
(the self-test runs there; make would create it moments later). A `build/`
that is a link is named by where it leads, since that is the working
directory make reports. The probe also clears what an earlier run left in
`<attempt>/cc/` (self-test results, the event log). The
fragment redefines Cactus's compile recipes — `COMPILE_C`, `COMPILE_CXX`,
`COMPILE_CU`, `COMPILE_F77`, `COMPILE_F`, `COMPILE_F90` — each copied from
the configuration's own `make.config.rules` with its one compiler reference
replaced:

```make
<attempt>/cc/inject.mk: ;
ifdef CCTK_TARGET
ifneq ($(findstring |<config>/build/,|$(CURDIR)/),)
MAKEFILES := $(filter-out <attempt>/cc/inject.mk,$(MAKEFILES))
ifeq ($(shell <grep -Eq for the stand-down pattern in /dev/null and whichever of $(SRCDIR)/make.code.defn and make.code.deps exist>; echo $$?),1)
define cactup_cc_run
'<cactup>' __cc '<config.toml>' '$(subst ','\'',$1)' '$(subst ','\'',$(SHELL))'
endef
override define COMPILE_C
current_wd=`$(GET_WD)` ; cd $(SCRATCH_BUILD) ; $(call cactup_cc_run,$(CC)) $(CPPFLAGS) $(CFLAGS) …
endef
…
endif
endif
endif
MAKEFILE_LIST := $(filter-out $(lastword $(MAKEFILE_LIST)),$(MAKEFILE_LIST))
```

Why this shape (rule 3):

- **Recipes, not compiler variables.** `$(CC)` is still expanded where it
  always was, per target, so a thorn that sets its own compiler — globally,
  per target, by pattern — gets it, and no recipe's environment changes: an
  ExternalLibraries `build.sh`, which hangs off the objects that need its
  library and reads `$CC`, gets what it always got. Setting `CC` per object
  target instead (a pattern-specific `private` variable) fails on both
  counts: before GNU make 4.4 the target-specific value of an exported
  variable is exported into the recipes of the target's prerequisites,
  `private` or not; and a more specific pattern silently beats a thorn's
  less specific one.
- **Only in Cactus's object sub-makes.** `MAKEFILES` is inherited by every
  make below `make <name>`. The fragment acts only where `CCTK_TARGET` is
  set (`make.thornlib` passes it to the `make.subdir` sub-make) *and* the
  working directory is under this configuration's `build/`. A third-party
  build started below one inherits `CCTK_TARGET` through `MAKEFLAGS` but
  runs in its own tree; and the object sub-make takes the fragment out of
  the `MAKEFILES` it hands on (a user's own entries stay).
- **Inert elsewhere.** Every other make that reads it — Cactus's makes
  above the object sub-makes, and anything they start — gets an empty rule
  for the fragment itself and nothing more. (make tries to remake each
  makefile it reads; without that rule a forwarding makefile's
  match-anything rule would be run for the fragment.) And in every make the
  fragment takes its own name back out of `MAKEFILE_LIST`, so a makefile
  that locates itself with `$(firstword $(MAKEFILE_LIST))` still finds
  itself.
- **A thorn's own recipe wins — as far as the fragment can see it.**
  `override` would silently beat a thorn that redefines `COMPILE_C` in its
  `make.code.deps` (the one place a plain redefinition takes effect, since
  it is read after the rules). The fragment stands down for a source
  directory whose `make.code.defn` or `make.code.deps` mentions the compile
  recipes at all (`COMPILE_` anywhere), or could define one where the
  fragment cannot see it: an `include` directive (`include`, `-include`,
  `sinclude` at the start of a line), a `define` directive (also behind
  `override`, `export`, `private` or `unexport`), a `load` directive (a
  make plugin can define anything), or `$(eval` or `$(guile` (with braces
  too). It matches directives, not words: `INCLUDE_DIRS` or a comment
  saying "include" does not count. make joins a line ending in `\` to the
  next with a space before it reads a directive or a function, so a `\`
  counts as the space after the keyword (`include\` then the file name on
  the next line is an `include`). A line that only continues the one
  before and begins with `include` is matched too (that costs the thorn
  the cache, never a wrong recipe). `grep -E` does the reading, once per
  object sub-make, with its messages discarded (the shell picks the files
  that exist; make's `$(wildcard …)` crashes GNU make 4.2.1 built against a
  current C library), and the fragment wraps only on its "no match" (exit
  1): an unreadable file or no `grep` at all leaves the thorn to plain
  make, and the self-test's wrapped run, which needs the "no match", turns
  the cache off for a build where `grep` cannot run. None of the 348
  thorns of the Einstein Toolkit has any of these (counted 2026-10-02), so
  this costs nothing there. What is left: a thorn that redefines a compile
  recipe under a computed name (`$(RECIPE_NAME) = …`, with `COMPILE_`
  nowhere in its text) is not seen. That needs a makefile written to hide
  the name; it is listed in the contract with the Cactus build work.
- **`config-data` stays pristine**: a `make <name>` run by hand later builds
  with the real compilers, with no cactup involved.

The probe wraps a recipe only if `make.config.rules` defines it exactly
once, plainly (a line `define NAME` with nothing after the name, then
`endef`; no other assignment to it) and outside any conditional — otherwise
the body it reads might not be the one make ends up with — and declines
altogether if the rules file includes other makefiles, which it does not
follow. The body must have exactly one reference to its compiler variable,
as a word of its own in command position: at the start of a recipe line
(not one continuing the line above), or right after `;`, `&&` or `||`, and
outside quotes. (A line ending in `\` goes on in the next: the compiler
may stand at the start of a continuation line if a separator ends the line
before it.) A recipe that runs the compiler behind something else, or only
mentions it, is not a shape cactup stands in front of. The probe
refuses (declines) paths with characters that mean something to make or the
shell — anything but letters, digits and `/ . _ - + @ ~` — rather than
escape them for every context they appear in.

The script then runs the goal `all` of two throwaway makefiles under the
fragment, with the build's own `make` command (`<attempt>/cc/selftest/`,
output in `<attempt>/cc/selftest.log`), three times in all. One run is the
way an object sub-make runs: each wrapped recipe must win over a later
plain definition and, run for real with the compiler `cactup:selftest`,
must reach a cactup that can read its configuration and
`/proc/self/status`. Two runs are the way other makes run — in `build/`
without `CCTK_TARGET`, and with it somewhere else, so that each half of the
fragment's guard is tried on its own — and each must find nothing of the
fragment's defined, itself first in `MAKEFILE_LIST`, and its match-anything
rule not run for the fragment (that rule leaves a file behind if it is).
Every check of a recipe is one `&&` chain ending in a marker file, and a
run writes its `.passed` file only when its checks have all run; the script
takes those three files, not make's exit status alone, as the pass. So a
make that exits 0 having run nothing, or one told to carry on past errors
(`-i`, `-k`), proves nothing. A `make` that fails any of it builds
uncached. A unit test feeds the self-test the fragment broken in each of
the ways it exists to catch. This holds on GNU make 4.2.1, 4.3 and
4.4.1 (tested; real Cactus trees on 4.3 and 4.4.1); for anything else the
self-test is the judge.

### 18.4 The wrapper

`cactup __cc <config.toml> <compiler> <shell> <args…>`. It is dispatched as
the first statement of `main`, before the interrupt handler, clap, the DB
and the update check: it runs once per compile with `make` waiting.

`<compiler>` is what the recipe's compiler variable expanded to, and
`<shell>` is make's `$(SHELL)`, each passed as one quoted argument, so that
the recipe's shell does not interpret the compiler text before the wrapper
has looked at it.

**The recipe's shell is the reference.** Without cactup, that shell decides
what the compiler text means and how to start it. Whatever the wrapper
cannot start itself goes to a shell of the same kind:
`<shell> -c '<compiler> "$@"' <shell> <args…>`.

- **A plain command** is what the cache works with: words the shell would
  pass on unchanged, the first naming a program the shell would take from
  `PATH` or by its path (`gcc`, `nvcc --compiler-bindir /usr/bin/g++`). The
  wrapper starts it directly — where the cache has identified the compiler
  (§18.5), the very file it identified, under the name the recipe gave. If
  that fails — a script without a `#!` line,
  a name only the shell's lookup finds — the shell runs it, and its message
  and exit status are the shell's own if it cannot either.
- **A name the shell resolves itself** goes to the shell even when a
  program of that name exists: a reserved word or builtin (`time gcc`,
  `command gcc`, `exec gcc`), a function exported to the shell under that
  name (bash's `export -f`), and a bare name when `PATH` has an entry
  beginning with `~` ahead of the directory the program is in (bash expands
  such an entry as it searches; other shells and the wrapper do not).
- **Anything else** (`LANG=C gcc`, quotes, any shell syntax) means what it
  means only to a shell, and goes to one without being looked at further.
- **A compiler already behind another wrapper** (ccache, sccache, distcc,
  …) is started as it is and left alone: cactup does not stack.

**The shell is asked what the compiler's name means.** Whatever the shell
sets up for itself at startup that changes how it looks a name up — a
function or alias, or a `hash -p` — cannot be seen from outside it. Cactus
runs its recipes with `SHELL = /bin/bash`, and bash reads the file
`BASH_ENV` names before every recipe line (module systems set it); bash and
zsh even run a function named by a path (`function /usr/bin/gcc { … }`)
for the command `/usr/bin/gcc`. So before a compile the wrapper runs
`<shell> -c 'printf "\ncactup-lookup:"; type "$1"' cactup <name>` in the
recipe's environment and working directory (messages in English, as for
the compiler), and starts the compiler itself only when the answer after
the marker is `<name> is <path>` (or bash's `<name> is hashed (<path>)`)
for the very file a `PATH` search finds, or the name names (both resolved
to their physical paths). Any other answer — a function, an alias, a
builtin, another path, nothing, a failure — leaves the compile to the
shell, with the answer in the log. (`type`, not `command -v`: for a
function named by a path, `command -v` prints the path.) The shell's
output goes to a file, and only the shell is waited for: a startup file
that leaves something running in the background with the shell's output
open does not hold up the compile, as it does not hold up the recipe. The
answer is remembered per build attempt (`<attempt>/cc/compilers/`), for
that shell and name, as long as these are what they were: the file a
`PATH` search finds, the shell's file (size, change time, inode), `PATH`,
`BASH_ENV`, `ENV`, `ZDOTDIR`, `HOME`, the files `BASH_ENV` and `ENV` name
and, for zsh, its `.zshenv` files, and the working directory when `PATH`
has a relative entry.

Differences from the recipe's own shell remain, and they are stated
limits. It is a *new* shell, run with `-c`: a compiler text that uses the
recipe's shell variables (`$$current_wd`) does not find them, and flags a
makefile gives its shell in `.SHELLFLAGS` (`-O expand_aliases`, `-l`) are
not given to it. And the answer is remembered: a startup file that answers
differently by the working directory or by a variable not listed above, a
file it reads in turn that is edited during the build attempt, or a
`BASH_ENV` value the shell expands (`$HOME/.env`, watched as the literal
name) can make a later compile of the attempt run differently in the
recipe than in the wrapper. A startup file that defines a function named
`type` or `printf` can make the answer say anything. No makefile or site
setup cactup knows of does any of that.

A started compiler's environment is the recipe's, with one adjustment: a
shell that sets `_` for each command it starts (bash) set it to cactup, and
the compiler is given its own path there instead, as that shell would have.
A `SIGPIPE` the recipe was started ignoring reaches the compiler at its
default (the Rust runtime and `std::process` both touch it); only a build
script run by hand from such a parent can show that.
- **Unreadable configuration, mode `off`, any internal error, a panic:** the
  process becomes the compile, by `exec`: same stdin, signal dispositions
  and jobserver descriptors, nothing of cactup in between.
- **`record`:** the compiler runs as a child with inherited stdio. `SIGHUP`,
  `SIGINT`, `SIGQUIT` and `SIGTERM` are passed on to it (`make` signals the
  recipe, not the recipe's children) — except those the wrapper was started
  ignoring, which stay ignored for the compiler too (a build under `nohup`
  survives the hangup). A terminal's own signal thus reaches the compiler
  twice, once from the terminal and once passed on. The wrapper then ends
  as the compiler ended: same exit code, or killed by the same
  `SIGHUP`/`SIGINT`/`SIGTERM`; any other fatal signal becomes the shell's
  `128 + signal`. One JSON line per compile goes to
  `<attempt>/cc/events.jsonl`, best-effort (`objcache::event::Event`): the
  compiler, the object's name below `build/`, the key and its parts or the
  reason there is none (§18.5), the exit status, and the time keying, the
  compile and the check afterward took. A compile left to the shell (also
  one the wrapper tried to start and could not), or to another wrapper,
  gets a line with the reason and nothing else — the wrapper becomes that
  compile and does not see how it ends. A thorn the fragment stood down for
  is not in the log at all.

Passing a signal on needs `kill(2)`, which std does not offer; the wrapper
uses `rustix` (D13).

### 18.5 Keys

A key is a digest that two compiles share only when they would produce the
same object (rule 1). `objcache::key` builds it from six parts, each a
SHA-256 over length-framed input (`objcache::hash`), kept apart in the log
so that two builds can be compared part by part. The six digests are
combined under a label (`key::KEY_LABEL`, now `key-6`), and **any change
that can make one key stand for another object changes the label**:
something the key now covers that it did not, anything cactup adds to or
changes in a compile it runs, a change in how a part is digested. Several
cactup builds share one store at once (a queued job runs the build it was
submitted with, for months), and the label is what keeps them from
serving each other objects made differently. A unit test pins the key of
fixed parts, so that the key does not change by accident:

- **Preprocessed text.** The output of the same compiler with the same
  arguments and `-E` in place of `-c -o <object>`. It shows what the include
  paths and macro definitions made of the source: which files were found,
  which branches were taken. No header is tracked or guessed at. With `-g3`
  macro definitions are kept (`-dD`), since the object's debug information
  then has them.
- **Files read.** The bytes of every file the preprocessor names in its
  line markers: the source and every header (everything but the compilers'
  own names for what is not a file, `<built-in>` and its like, matched
  exactly). The text alone forgets what
  the compiler does not. Spacing and comments move the columns that debug
  information, `__builtin_COLUMN` and `std::source_location` record; Clang's
  debug information carries a checksum of each file. So an edit to a
  comment, or to text inside `#if 0`, changes the key. The names are read
  as the compilers write them, as C strings (Clang writes a tab or a byte
  outside ASCII in octal); a marker whose name cannot be read leaves the
  compile without a key. So does a file the preprocessor *entered* (marker
  flag `1`) that cannot be read afterward, and the source itself. Only a
  name that no marker enters — one a `#line` in the source gave, as
  generated code names its origin — may be of no file, and is then keyed
  as absent: the bytes compiled are those of the file the directive stands
  in, which was entered.
- **Arguments.** Every argument but `-c`, `-o <object>`, the source file,
  and `-I`/`-D`/`-U` (whose whole effect is in the text and the files), in
  order, plus the language by the source's suffix, and with debug
  information the working directory (and `PWD`, which compilers prefer when
  it names the same place). `objcache::compile` reads GCC and Clang command
  lines from a list of what it knows. A flag that is not on it makes the
  whole command line "not cached"; so does one that names another input
  (plugins, profiles, precompiled headers, LTO, sanitizer lists), another
  output (split DWARF, coverage), another program (`-B`, `-Wa,`),
  whose meaning depends on where it stands (`-x`), or that reads a file the
  preprocessor's output does not name (`-imacros`). Optimization and debug
  levels are listed one by one, since `-g…` also begins flags that record
  the command line or embed the source. One second output is understood: a
  dependency file written while compiling, in the form `-MD` or `-MMD`,
  `-MP`, `-MF <file>` (required: without it the file's place follows from
  `-o`), `-MT`/`-MQ <target>` — the form a Cactus recipe gives when
  dependencies come from the compile itself. These flags change neither
  the object nor the text, so they are not in the key, and they are kept
  from the preprocessor runs that make the key, which would write the file
  too. (A serving cache has to have the file written on a hit; its own
  preprocessor run, given the flags, does that.) Two families are admitted by
  prefix, because their members are too many to list and none of them names
  a file: `-W…` (diagnostics) and `-m…` (machine options; `-mllvm` and
  Clang's `-module…` excepted).
- **Compiler.** `objcache::identity`: the bytes of the file that would run
  (found as `execvp` finds it), the name it is run by (`clang` and `clang++` are one file), for GCC the
  bytes of `cc1`, `cc1plus` and the assembler it names, and of every shared
  library each program loads (as the dynamic loader resolves them), plus
  what the driver says of itself (`--version` without the installation
  directory, and for GCC its built-in specs and target). Not its path and
  not its modification time. The identity is worked out once for a build
  attempt and remembered with what it was computed from: the files (by
  their physical paths and by the names they were found by, which a link
  turned elsewhere leads elsewhere from), and the places searched and
  passed over before each was found, which must go on holding nothing (or
  what they held) for the next compile to reuse it; and it is worked out
  again after the compile, before what it made is stored, and must be the
  same (tried: an assembler that appeared first on `PATH` mid-build, also
  while a compile ran, assembled objects stored under the real one's
  identity). For GCC's programs, the places it says it looks
  (`-print-search-dirs`; none for an assembler it was built to run,
  `--with-as`), then `PATH`'s for a program it leaves to `PATH` (the
  assembler); for libraries, the places the loader says it tried (glibc's
  `LD_DEBUG=libs`, said where cactup reads it whatever `LD_DEBUG_OUTPUT`
  says), each entry of its search paths that is not there or is no
  directory (it is not tried again for the next library), a library
  preloaded by its path that is not there, and its cache file. A relative
  place is looked at from each compile's own working directory; a library,
  or a program the driver runs, found by a relative name (or a library
  its program names with a `/` but not from the root) rules the compiler
  out. Along `PATH`, as `execvp` does, a file the system will not run (no
  permission, a filesystem mounted `noexec`; tried by starting it) is
  passed over. A program the driver runs must be one, as the driver must:
  a script runs something nothing here follows (tried, a wrapper around
  `as`). Once the places are looked at, the driver is asked again where its
  programs are and the libraries are listed again, and a different answer
  rules the compiler out (something appeared at a place between the search
  and the look). A flag that selects another of a GCC's sets of programs
  and libraries (`-print-multi-lib`: `-m32`, `-mx32`) keeps the compile out
  of the cache: the driver looks for its programs elsewhere then (tried).
  Each compile looks at all these places, a hit too: a few hundred lookups
  with a long `PATH`, about a millisecond on a local disk, and one round
  trip each on a network filesystem that caches no negative lookups
  (`lookupcache=positive`). Left: what appears during a compile and is
  gone again after it, a preload list the system keeps
  (`/etc/ld.so.preload`), the loader's tunables, the character-set
  converters a compile that converts loads (`GCONV_PATH`), and the
  libraries of a program whose loader is not glibc's (it lists none). What still leads the driver elsewhere from one
  directory to the next keeps the compile out of the cache: an entry of
  `COMPILER_PATH` or `GCC_EXEC_PREFIX` that is no absolute path (an empty
  one is the working directory), a library preloaded by a relative path,
  `LD_AUDIT`; and for Fortran, whose dependency run runs in a directory of
  its own (§18.10), also such an entry of `LIBRARY_PATH` (where the driver
  finds a `specs` file), a compiler named by a relative path, and, for one
  named without a `/` (which finds its own prefix along `PATH`), such an
  entry of `PATH`. The driver's own bytes must say it is GCC or
  Clang: a wrapper (`mpicc`, a Cray `cc`, a script) passes `--version` on
  to a compiler and is not one, and is not cached. Neither is a compiler
  that takes flags from a file of its own — a GCC with a `specs` file on
  disk, a Clang that says it reads a configuration file: what such a file
  adds never passes the reader of the command line, so nothing that reader
  declines would be declined. **One kind of specs file is accepted**: one
  that changes only how GCC links. Site-built GCCs have such a file to add
  an `-rpath` to their own libraries (seen on qbd: `*link_libgcc:` gains
  `%(link_libgcc_rpath)`, a new section with the `-rpath`), and a `-c`
  compile never links. The file is read against the driver's built-in
  specs (`-dumpspecs`, which ignores the file) and accepted only if it
  defines every built-in section (a GCC that finds a specs file does not
  set up its built-in sections first: one the file leaves out, such as the
  target's `cc1_cpu`, is gone, and an empty file leaves GCC compiling
  nothing), and every section it defines is a built-in one with the same
  text, a built-in one only the link command uses (`link_command`, `linker`, `link`, `lib`,
  `libgcc`, `link_libgcc`, `link_gcc_c_sequence`, `link_ssp`, `link_gomp`,
  `startfile`, `endfile`, `post_link`, `linker_plugin_file`,
  `lto_wrapper`, `lto_gcc`), or a new one that no section outside that
  list refers to (`%(name)`, `%[name]`) except other new ones; and if it
  has nothing else: no `%include`, `%include_noerr` or `%rename`, no
  compiler for a suffix (`.ext:`, `@language:`), no section twice, nothing
  unparsed, no empty section followed by a single blank line (GCC
  skips blank lines after a section's name, so it would read the next
  section's name as this one's text; `-dumpspecs` writes two), and no `#`
  or line-final `\` anywhere (GCC takes a comment, and a backslash with
  its line end, out of the text; `-dumpspecs` prints the text as it is). GCC's own compile steps, which `-dumpspecs` does not show,
  refer to built-in sections only, and to none on that list. The file's
  bytes join the compiler's identity. Which file a driver reads depends on the
  compile (Clang picks a configuration file by target, so `-m32` can bring
  one in), so **every compile is asked**: the preprocessor run is given
  `-v`, and the driver says on stderr whether it read a configuration file
  (Clang) or its built-in specs (GCC); a GCC whose specs file was accepted
  must say it read that file (`Reading specs from`, the same file by its
  physical path) and no other. Each must say a line it always says
  (`InstalledDir:`, `Using built-in specs.` or the accepted file's line): an answer without it is no
  answer, and no key (Clang names a configuration file between
  `InstalledDir:` and the command line of the compiler proper, so it has
  to have printed both). That run is given English messages
  (`LC_MESSAGES=C`, no `LANGUAGE`, and a non-empty `LC_ALL` taken apart
  into `LC_CTYPE`, `LC_COLLATE`, `LC_NUMERIC`, `LC_TIME` and
  `LC_MONETARY`, which thus stay what they were: a compiler may read its
  source by `LC_CTYPE`).
  The compiler as a whole is asked too, once, to spare the compiles the
  asking; and whatever a driver is asked for its identity (`--version`,
  its specs), it is asked in English the same way, so that the identity
  does not depend on the language of the session that asked. gfortran is a
  family of its own (§18.10): its back end is `f951`, and its relocation
  and locale trials compile Fortran. The answer — also "not one the cache
  works with" — is remembered per build attempt (`<attempt>/cc/compilers/`)
  and reused while every file it came from still has the same size, change
  time and inode. The wrapper starts the file that was identified, under
  the name the recipe gave.
- **Platform** (D15). `objcache::platform`: what `prepare` froze — the
  cactup machine, the build universe, the digest of the build-phase
  environment setup — and what the wrapper finds on the host that compiles:
  the architecture, each kind of processor (from `/proc/cpuinfo`: vendor,
  family, model, stepping, feature flags, cache size; and its caches as
  `/sys/devices/system/cpu` describes them; not speed or microcode), and
  `/etc/os-release`. The host half is there because the machine name can be
  wrong or too coarse (detection keeps the last machine when no machine
  claims a host; `generic` covers every unclaimed one; login and compute
  nodes may differ), and it is always in the key, so `-march=native` and
  compilers that tune for the build host unasked need no flag to say so. A
  change of machine keys differently and invalidates nothing; so does a
  change of hardware or of the operating system under one machine.
  Remembered per attempt, host and boot (`<attempt>/cc/hosts/`).
- **Environment.** `objcache::environment`: the variables compilers are
  known to read — locale, `SOURCE_DATE_EPOCH`, the loader's and the
  compiler's search paths, the loaded-modules lists, and the families MPI
  wrappers and vendor toolchains use. A variable that makes a compiler
  write another file (`DEPENDENCIES_OUTPUT`, …), or that rewrites its flags
  behind the command line (`CCC_OVERRIDE_OPTIONS`, `GCC_COMPARE_DEBUG`),
  makes the compile not cached. The whole environment cannot be keyed: Cactus's makefiles export
  well over a hundred variables into every recipe, many of them the
  configuration's own paths.

**Compiles with no one object.** Some compiles the cache declines because
nothing it could key would say what comes out:

- A source whose text makes the assembler read a file (`.incbin`,
  `.include`): the object holds bytes no part of the key covers.
- A precompiled header that would be used unasked. GCC takes
  `<header>.gch` beside a header in place of the header; its `-E` is asked
  to say so (`-fpch-preprocess`), and such a compile is not cached. Clang
  looks for one only beside a file given with `-include`, and its `-E`
  does not say; a Clang compile with `-include` is not cached.
- `-march=native` (or `-mtune=`, `-mcpu=`, with or without extensions
  after `native`) on a host whose processors are not all of one kind (performance and efficiency cores): the compiler
  targets the core it happens to run on. Seen on a hybrid Intel
  workstation: one compile, run twice, gave two objects (GCC resolved three
  different cache sizes across its sixteen cores).

**Paths.** Cactus compiles with absolute paths, and an object records them:
`__FILE__` in every `CCTK_WARN`, file names and the compile directory in
debug information. As it stands an object belongs to one configuration of
one installation. Where it is known to hold, the key is computed as if the
compile ran with the Cactus root mapped to `/cactup-root/` and the
configuration directory to `/cactup-root/configs/@config/`
(`key::PathMap`), which is how a serving cache runs it (§18.8). The names
are absolute (decision 8 in `design/build-cache/DECISIONS.md`): debug
information then names every file by an absolute path, which gdb's
`set substitute-path` turns into the real one (a rule for
`/cactup-root/configs/@config` first, then one for `/cactup-root`), where a
relative name would be taken as relative to the recorded compile
directory.

- The compiler is given `-ffile-prefix-map=<root>/=/cactup-root/` and then
  `-ffile-prefix-map=<config>/=/cactup-root/configs/@config/` for the preprocessor
  run, and maps `__FILE__` where the text uses it. The option and the two
  names (not the directories) are in the arguments part of the key, so a
  cactup that maps another way keys apart. Each directory is given
  with its trailing `/`, in the spelling cactup has for it and in its
  physical one.
- The key maps what the compiler does not map in `-E` output — the file
  names in line markers — and the working directory, by the compiler's own
  rule: a plain string prefix at the start of a name, the most specific
  directory first. `<root>-libs/include` is not under `<root>/`, for the
  compiler and for the key alike.
- Of the keyed arguments, only the values of `-isystem`, `-iquote`,
  `-idirafter` and `-include` are mapped: the compiler uses them to find
  files and keeps them nowhere but in the names of what it finds. Every
  other argument is keyed as written. GCC records its command line in debug
  information, unmapped, so a path of the tree in `-frandom-seed=<path>` is
  part of the object, and two installations do not share such a key.
- Each file's bytes are keyed under its mapped name. Cactus begins a build
  copy with `#line 1 "<absolute path of the original>"` when the option
  list asks for line directives; in a file's *first line* such a name is
  keyed as mapped (the compiler takes a name from that line and nothing
  else). Further down, what looks like a directive may be the inside of a
  comment or a string, and is keyed as the bytes it is.
- **Whether the map holds is tried, per compiler** (`identity::relocates`,
  once per build attempt): a Cactus compile in miniature — a build copy
  with a line directive, a header from the tree, one from the
  configuration, `__FILE__` in each, debug information on — is compiled in
  two trees at different paths with configurations of different names, with
  the flags above, and the two objects must be the same bytes. A compiler
  without the option, one that applies the first matching map and not the
  last (compilers have differed, between versions and between `__FILE__`
  and debug information), or one whose debug information carries the
  unmapped path some other way, fails the trial and keeps the
  installation's paths in its keys.
- Clang with `-fopenmp` keeps them too: its OpenMP source-location strings
  hold the unmapped path.

A key made without the map is sound and of use to later builds of the same
configuration only. The log says for each key whether it is free of the
installation's paths. **Nothing is added to the real compile while the
cache only records**: the objects of a recording build are byte for byte
those of a build without the cache.

**Record mode** does around each compile what a serving cache does around
one it has to run: key it, run it, and compute the text and file digests
again to see whether the key still describes what was compiled (a header
edited during the compile would otherwise leave an object of the new text
under the key of the old). Only the compile has any effect. The two
preprocessor runs and the reading of the files are the cost a build pays
for the cache on a miss, and the log has what each took. For gfortran the
check runs no compiler where it can: it reads the files again and looks
where the compile looks (§18.10, decision 14 and 16 in
`design/build-cache/DECISIONS.md`); the log says how many lookups that took
(`lookups`), or why the dependency run ran again (`checked_by_compiler`). A stop signal that
arrives during the check afterward ends the wrapper on the spot, by that
signal (`make` then discards the object, as it would have with the compiler
still running); the compile is not logged.

**What the key rests on, and where it could be wrong.** Stated so that
nobody has to find out:

- *What the compiler reads that the preprocessor does not name.* The key
  covers the files in `-E`'s line markers. A file read some other way that
  this section does not list is not in it. Known and handled: precompiled
  headers, `#embed` (its bytes are in the text). Known and excluded by the
  flag list: plugins, profiles, sanitizer lists, module maps, `-imacros`.
- *Assembler includes are found by their spelling.* `.incbin` or
  `.include` followed by a quote, in the preprocessed text. A source that
  assembles the directive from pieces (`".inc" "bin \"blob\""`) is not
  noticed, and the file it pulls in is not in the key. Nobody writes that
  by accident.
- *A file changed and changed back* between the key and the check after the
  compile, with the compile reading the changed bytes in between, has its
  keyed bytes again. The check also compares, for each file (not part of
  the key): every entry its name resolves through, directories and
  symlinks (followed as the kernel follows them), by device, inode and
  birth time (where the filesystem keeps one; it tells a new entry from an
  old one when ext4 hands the new one the old inode number at once, and
  does not move when files are created inside a directory), a symlink also
  by its change time and its target; and the file itself as the opened file is
  (device, inode, size, modification and change time; taken after the
  open, which on NFS revalidates what the client knows of it). User space
  cannot set a change time back, and a symlink swapped and swapped back is
  a new symlink, so a file rewritten or reached another way in between
  fails the check. Left: a filesystem whose change times are coarser than
  the edits (a change and a change back within one tick, to a file of the
  same size); and the very same directory renamed away and renamed back
  while the compile reads through it (decision 9 in
  `design/build-cache/DECISIONS.md`: a directory's change time, the only
  trace, also moves whenever anything is created in it, and watching it
  would stop honest compiles from being published); and, on a filesystem
  that keeps no birth times (NFS, typically), a directory removed and
  another made in its place that gets the same inode number. Audit mode
  would catch an object any of these produced.
- *The flag families.* A `-W…` or `-m…` flag that names a file or records
  something outside the key would be admitted. None is known besides the
  two excepted.
- *Flags from behind the command line.* The reader sees the command line.
  Known other sources are ruled out: a response file, a GCC `specs` file
  that does more than change the link, and a Clang configuration file (by
  what the driver says of each compile), the override variables. The
  link-only judgment rests on the list of link sections above being
  right for every GCC: a GCC whose compile steps refer to one of them
  would be keyed without what that section adds. Flags a distribution
  built into its compiler are part of the compiler, and of its identity;
  one that built in a flag the reader would decline is not noticed.
- *The compiler's identity.* Files a compiler reads by rules of its own
  that are named nowhere above — a plugin directory, lists in Clang's
  resource directory, whatever a later version adds — are not hashed. The
  remembered identity watches where programs and libraries are searched
  for (above); a Clang configuration file or a GCC `specs` file added
  while a build attempt runs is reported by each compile, and seen there.
- *The path map.* The trial shows the map holds for the trial's compile. A
  flag on the list that makes the compiler put an unmapped path *it worked
  out itself* into the object, where the trial does not look, would give
  two installations one key for two objects (differing in a recorded path,
  not in code). Known and excluded: sanitizers, Clang's OpenMP. (A path
  written in a flag is not such a case: it is keyed as written.) Audit mode (the serving milestone's gate) is
  what tries it on real compiles.
- *The environment.* A variable on no list that changes some compiler's
  output. The environment-setup digest in the platform part and the
  loaded-modules variables narrow that; it is not closed.
- *The host.* Two hosts alike in everything the platform part reads and
  different in something a compiler targets.

**What the key costs in hits.** Keying files by their bytes means that an
edit which changes no token still misses: every source that includes a
header misses when that header's bytes change. Cactus generates headers
that nearly every thorn source includes and that list the configuration's
thorns (`cctk_DefineThorn.h`, `CParameterStructNames.h`), so adding a thorn
to a configuration costs more than the new thorn's compiles (measured:
§18.6's report on a 25-thorn configuration against the same with three
thorns added dropped from 90% served, with the text alone, to 55%).
Narrowing that is a design question for the serving milestone, not a
soundness one.

### 18.6 `cactup cache report`

```
cactup cache report [<config>] [--attempt N]
                    [--against <config>] [--against-installation <alias>] [--against-attempt N] [--long]
```

Reads the event log of a recording build (the newest attempt of the config
that has one, or `--attempt`) and prints how many compiles were keyed, per
language and with the share of compile time; how many of the keys are free
of the installation's paths; why the other compiles were not keyed; how
many keys no longer held after the compile; and what keying and checking
again cost against the compiles themselves.

The log's format is not kept between versions of cactup (§2.4: no backward
compatibility). Lines the reader cannot parse are counted and the count is
printed; a log with nothing else is an error that says to build again.

With any of the `--against` flags it compares with another recording build
— another config, another installation, or an earlier attempt of the same
one (never the same attempt: naming it is an error) — and says what a cache filled by that build
would have served: a compile is served if the other build has a compile
with the same key that succeeded and whose key still held, under whatever
name. For the rest it finds the same object (by its name below `build/`) in
the other build and names the parts of the key that differ; `--long` lists
them one by one. Both builds are found before anything is printed.
Nothing is read from or written to a cache: both builds only recorded, and
"would be served" stands on §18.5's path mapping, which the per-compiler
trial supports and audit mode has yet to try on real compiles.

### 18.7 The store

One directory tree per instance, shared by every installation,
configuration and host of it, and written by builds running at the same
time on several hosts over NFS or Lustre, where `flock` cannot be trusted
(§2.3). So: no lock, no index, no file written by two processes. Every
write is a new file under a name of its own, and the one shared step is
`link(2)`, atomic on all three (`objcache::store`).

**Where.** `<root>/v2/<machine>/<first two hex digits of the key>/<key>`.
The root is the user's knob `build-cache-home` (an absolute path), else the
machine's `[paths] build-cache-home` (§4.2), else `.cactup-build-cache` in the
install home (the `install-home` knob, else the machine's, else
`~/.cactup/cacti`): beside the installations, whose builds already do their
I/O there. A machine's place that cannot be resolved here is passed over for
the next with a warning (`objcache::store_root`). It is resolved by `prepare`
and frozen into
`<attempt>/cc/config.toml` (§18.2): the wrapper on a compute node reads it
from there (§18.1 rule 6). `v2` is the entry format. A cactup with another
format writes beside it and never reads it — no backward compatibility
(§2.4); the old tree is left as it is (`cache stats` names it, and says to
remove it by hand once no older cactup builds with the store). `<machine>` is the cactup
machine the build was prepared for, which the key also covers: the
directory keeps machines apart for `cache stats` and `cache gc`, not for
soundness. A machine name that is not a plain file name keeps the build out
of the store.

**An entry** is one file, written whole once and never changed:

```
cactup build cache entry\n                       magic line
<header> <object> <stdout> <stderr> [<module>…]\n their lengths, decimal
<header, TOML>
<the object>
<what the compiler wrote to stdout>
<what the compiler wrote to stderr>
<each module file the compile wrote, §18.10>
<SHA-256 of every byte above, 64 hex digits>\n
```

The header has the format (`2`), the label the key was made under, the
key and its six parts (§18.5), the names of the module files in the order
they follow the messages (at most 1000; none but for Fortran); and,
for people only, the object's name below `build/`, the compiler as the
recipe named it, whether the key is relocatable, the cactup version and
the host that compiled it. An entry is *whole* when the magic line is
there, the lengths add up to the file's size, and the checksum is right;
it is *valid* when it is whole, its header parses (unknown fields are an
error), its key is the file's name and the digest of its parts, and it
names as many module files as it holds, each a plain module file name
(`<name>.mod` or `<name>.smod`), none twice. A
dependency file is not stored (§18.5: a hit has the key's own preprocessor
run write it).

**Several cactup builds share the store** (a queued job runs the build it
was submitted with), so the lengths are outside the header and the header
is read last: a whole entry whose header this cactup cannot read, or whose
key was made under another label, was written by another cactup, and is
left alone, a miss. Any change to what
an entry holds or how it is read changes the format, which puts the new
entries in a directory of their own; any change to what a key stands for
changes the key's label (§18.5).

**Publishing** an object (what a serving cache does after a compile that
succeeded and whose key still held, §18.5):

1. If `<key>` exists as a file, nothing is written: an entry is the object
   of its key, whoever compiled it first. (Something else there, such as
   a directory, fails the publish.)
2. The two directories above it are created if missing.
3. A temporary file is created beside it, `.tmp-<key>-<random>`, and the
   entry is written into it, the object read once from the compile's
   output and the checksum computed over exactly the bytes written. An
   object whose size or modification time changed while it was copied is
   not published. The file is synced to disk (`sync_all`) and then made
   read-only (`0444`), in that order, so that a server that checks
   permissions when the data reaches it has nothing left to refuse.
4. It is hard-linked to `<key>`. The entry is published if the link
   succeeds, or if it fails and the temporary file's link count is 2 (NFS
   can lose the reply to a link that happened; `lock::LinkLock` does the
   same). If `<key>` exists by now, another build published it first: the
   same object, so nothing is lost. Any other failure (no space, no
   permission, no hard links on that filesystem) leaves the object
   unpublished; the build goes on.
5. The temporary file is removed.

What an interruption at each step leaves: before step 4, a `.tmp-` file no
restore ever opens; between 4 and 5, the same beside a whole entry. `cache
gc` removes `.tmp-` files a day old (by the fileserver's clock). The data
is synced before the link, so a name that exists names a whole entry, also
after a crash. A stop signal during publishing ends the wrapper on the spot,
as during the check after the compile (§18.5), and leaves such a `.tmp-`
file.

**Restoring** an object:

1. `<key>` is looked at, then opened. Not there: a miss. Not a regular
   file: invalid (a FIFO there must not block the open).
2. The magic line and the lengths are read and checked against the file's
   size.
3. A temporary file is created beside the object the compile would write
   (`.<object name>.cactup-<random>`), created as a compiler creates its
   output (`0666`, less the umask, plus what a default ACL of the
   directory adds), and the object's bytes are copied into it while the
   whole entry is digested; the compiler's output is read into memory; each
   module file (§18.10) is copied into a temporary file in the directory it
   goes into (`.module.cactup-<random>`, created as the object's is); the
   checksum is compared. Then the header is read.
4. If the entry is valid, the module files go first: each temporary file
   is renamed onto its module file's name, unless the file there already
   has the same bytes, when it is left alone (with its modification time,
   as gfortran leaves it) and the temporary file removed. Then the object's
   temporary file is renamed onto the object's name: `make` sees the old
   object or none, or the whole new one, never part of one, and an object
   in place stands for its module files in place too. Its modification time
   is the restore's.
5. If it is not whole, or whole and for another key, the temporary file is
   removed, the entry is invalidated, and it is a miss. If it is whole and
   its header is another cactup's, or reading it fails in a way that says
   nothing about its content (an I/O error, a stale NFS handle), it is a
   miss that leaves the entry alone: a passing fault or another build must
   not cost a good entry.

An entry is never linked to the object: the object is the build's, and a
later compile writes into it in place. A restore stopped half way leaves
its `.<object name>.cactup-` file in the configuration's `build/`
directory, where `make` never looks; `cache gc` does not reach it, and a
`realclean` removes it.

**Invalidating** an entry removes it, so that the next compile of its key
can publish a good one. It is removed by name only if that name still
leads to the file that was read (same device and inode): another build may
have removed it already and published anew. The check and the removal are
two steps; a good entry that slips in between is removed, which costs one
miss.

**Two builds at once.** Two publishers of one key: one link wins, the
other finds the name taken, and both entries were the same object. A
reader and a publisher: the reader finds no entry or a whole one. A reader
and an invalidator: the reader keeps reading the file it opened, and
checks it anyway. On NFS the data is on the server before the link, and a
reader on another host revalidates at open (close-to-open consistency); a
host that has not seen a new entry yet misses.

**Trust.** The checksum shows that an entry is what its writer wrote, not
that its writer was honest: whoever can write in the store can put any
object under any key. The store is as trustworthy as its directories'
permissions. Entries are read-only for everyone, directories are created
under the user's umask, and the default root is inside `$CACTUP_HOME`. A
root shared with a group trusts every member with every build that reads
it.

**Last use** is not recorded yet; it comes with `cache gc`. Whatever form it
takes, an entry is never changed after its link.

### 18.8 Serving

With `build-cache = serve`, the wrapper keys every compile it can, as in
record mode, and then either serves it from the store (§18.7) or compiles
it and publishes the result. `build-cache = audit` serves too, but checks
every hit by compiling anyway. `record` stays what it is: a measurement
that changes nothing.

**A hit**: the key's preprocessor run (§18.5) has the store's entry for the
key restored onto the object's name, the compiler's stored messages written
to the wrapper's stdout and stderr, and the wrapper exits 0. No compiler
runs. If the recipe asked for a dependency file (§18.5's `-MD` form), that
same preprocessor run writes it: it is given the compile's dependency flags
with the last `-MF` (the one the compiler writes) pointing at a temporary
file beside the real one, created as the compiler creates its output (`0666`
less the umask, plus a directory's default ACL: make skips a `.d` file it
cannot read, and says nothing), and `-MQ <object>` when the compile names
no target (which is the target GCC
and Clang give a compile with `-o` and no `-MT`/`-MQ`; tried with both,
spaces, `$` and `#` in the name included). On a hit the file is renamed
onto the real name, on a miss it is removed: the compile writes its own.
Tried: the file is byte for byte the compile's, with and without the path
map.

**A miss**: the compile runs, given the path map's flags when the key was
made with the map (§18.5): the stored object must be the object the key
describes. **This is where the cache changes a real compile**, and where a
build with the cache serving stops being byte for byte one without it: the
objects name their sources `/cactup-root/arrangements/…` and the
configuration's files `/cactup-root/configs/@config/…` (in debug
information and in `__FILE__`, so a `CCTK_WARN` shows them too). A
debugger is pointed at them with two gdb rules, the configuration's first
(`set substitute-path /cactup-root/configs/@config
/path/to/Cactus/configs/<name>`, then `set substitute-path /cactup-root
/path/to/Cactus`; gdb uses the first rule that matches, and without line
directives Cactus compiles the copies in the configuration's `build`), or
the knob below turns the map off for a build meant for debugging. (Not
tried with a debugger: the host this was written on has none.) The compiler's stdout and stderr go through the wrapper: one thread
per stream reads it and keeps a copy (up to 4 MiB of each; beyond that the
result is not published), another passes it on as it comes. When the
compiler has ended, the reading waits up to two seconds for the streams to
close (a process the compiler started may keep one open; then the result
is not published either), and the passing on is waited for to the end,
however slow the terminal is, as the compiler's own writes would have
been. If the compile succeeded and the key still holds afterward (§18.5),
the object and the messages are published (§18.7); then the wrapper ends
as the compiler ended.

**Messages are stored as the map would have them.** A compiler's
diagnostics name files by the paths the compile used, which the path map
does not change. In a relocatable entry, the configuration directory and
the Cactus root (each spelling, with its `/`) are stored as
`@CACTUP_CONFIG@/` and `@CACTUP_ROOT@/` wherever a path begins with them
(at the start of the text, after a character that cannot be part of a
path, or after a terminal color sequence), and a hit writes them back as
this build's directories: a warning served from another installation
points into this one. (A path glued to something that can be part of a
path, such as `-I/path/...`, is stored as it is.) The messages are those of the build that
compiled the object — in its language, when the locale is not keyed (below)
— and in the order each stream had, not interleaved as they were.

**The path map can be turned off**: `build-cache-relocate = no` (default
`yes`, frozen with the rest) keys every compile without the map, so the
compiles keep the installation's absolute paths and are shared only with
builds of the same configuration in the same installation.

**The locale leaves the key behind a trial** (decision 5 in
`design/build-cache/DECISIONS.md`). Once per build attempt and compiler,
with the compiler's identity, a source with non-ASCII bytes in a comment, a
string and a wide string (not an identifier: GCC before 10 rejects those)
is compiled with debug information
twice: in the session's locale, and in the C locale (`LC_ALL=C`, `LANG`,
`LANGUAGE` and the other `LC_*` removed). A compiler that makes one object
of both is keyed without `LANG`, `LANGUAGE`, `LC_ALL`, `LC_CTYPE` and
`LC_MESSAGES`; one that does not keeps them. A compile that converts
character sets (`-finput-charset=`, `-fexec-charset=`) keeps them whatever
the trial said: the trial compiles without such flags, and a conversion can
follow the locale (GCC's `ASCII//TRANSLIT` gives `cafe` in a UTF-8 locale
and `caf?` in C, from the same text). Each session is compared with
the same reference, so two sessions whose compilers pass are interchangeable
for that source. The remembered identity depends on those variables, so a
session in another locale tries again.

**Audit mode** checks a hit instead of trusting it. The entry is restored
to a temporary file beside the object, the dependency file the hit would
have written is kept aside too, and the compile runs anyway, exactly as on
a miss. Only a compile whose inputs held still (the check after it passed,
§18.5) says anything about the entry; otherwise the verdict is that the
inputs changed. If the two objects are the same bytes and so are the two
dependency files, the hit was right; the same object with another
dependency file is a wrong dependency file. If the objects differ, the
fresh object is moved aside and the compile runs a second time, its
messages not shown again: if its object equals the first and the inputs
still held, the stored entry is wrong (**a wrong hit**: the key failed to
describe the compile), and it is removed and the fresh object published in
its place; if not, the compiler is not deterministic for this compile. A
stop signal that reaches the wrapper during the second compile ends it by
that signal, as it would the compile, with no verdict; a second compile
that ends any other way (killed by the kernel, crashed, failed) leaves the
first compile's object the build's, and the verdict that the second
compile failed. A hit whose compile now fails is
counted as well. Either way the build keeps the fresh object, the event log
records the verdict (`event::Audit`), and the build's closing line counts
it.
Audit mode is the acceptance gate for serving: a full build in two
installations with no wrong hit.

**What a build says.** The build step ends with one line, as in record
mode: `cactup: build cache: N compiles, H served from the cache, P
published, U could not be (see cactup cache report)`; in audit mode, `A
checked against the cache (W wrong, F failing to compile, D not
deterministic, C with inputs that changed), P published`. They are counted
from the event log, by the spellings `event::Outcome` and `event::Audit`
give the log.

**Signals.** A hit runs no compiler: a stop signal during it ends the
wrapper by that signal, which leaves at most the temporary files the store
leaves when a restore is cut short (§18.7), and the key's temporary
dependency file (`.<name>.cactup-…` beside the real one). A signal after
an audited compile can leave the temporary copies audit mode keeps beside
the object, under the same kind of name; for Fortran, a restore cut short
can leave `.module.cactup-…` beside the module files, and a copy cut short
`.copy-…` in `.cactup/`. The Fortran keying's own directory in the
attempt's `cc` (`.fortran-…`) is left too. `make` reads none of them. A miss forwards signals to the
compiler as record mode does; a signal during the check and the publishing
after it ends the wrapper on the spot (§18.5, §18.7).

### 18.9 Living with the store

Nothing is ever removed from the store on its own (§18.1 rule 7): an old
entry is what makes going back to an older version of a thorn cheap. The
store is looked after by three commands, a size notice, and a record of
when each entry was last used.

**Last use.** An entry is never changed after its link (§18.7), so when it
was last used is kept beside it, not in it. After the build step of a
serving or auditing build, `execute` writes one new file into the store,
`<root>/v2/<machine>/used/<random>.keys`, listing the keys that build found
there, one per line, through a temporary file renamed into place. Its
modification time, set by the write, is the fileserver's clock at the end
of that build (§2.3), and is when every key in it was last used. No two
builds write the same file, and none changes one: nothing is shared but
the directory. The logs, like entries, are readable by everyone who can
read the store (`0666` less the umask): `gc` must be able to read every
log, and a log it cannot read stops it (below). A build that finds nothing
writes nothing; one that cannot write says so in one line and is otherwise
unaffected. An entry's own
modification time is when it was published. `cache gc` folds the logs it
reads into one, `<random>.times`, with a time on each line (`<key>
<seconds>`), and removes the logs it folded, so the information outlives
them and the directory stays small.

**`cactup cache stats`** walks the store under the root a build on this
machine would use (and says which setting named it) and says, per machine: how many entries and how
many bytes, the oldest and the newest publish, how many were used in the
last 7 and 30 days, temporary files; and which directories hold another
entry format (another version of cactup's: not walked, and to be removed
by hand once no older cactup builds with the store). It works on a store
it can only read (ages then by this host's clock, which days do not
mind).

**`cactup cache gc [--unused-for <age>] [--to-size <size>] [--dry-run]`**
(at least one of the two) removes the entries of this cactup's format that
no build has used or published for `<age>` (`30d`, `12w`, `6m` …),
measured in the fileserver's clock; with `--to-size`, further entries,
least recently used first, until the store is no larger than `<size>`
(`200G`). Every directory and every use log of the store must be read
whole, or `gc` stops before removing anything, naming the log: a log it
could not look at or read would leave the entries it records as in use
looking unused. (`stats` says so and goes on; `verify` does not read
logs.) It also removes the
temporary files of publishes cut short (`.tmp-…`) older than a day, and
folds the use logs. Only one `gc` runs at a time per store: it holds
`<root>/gc.lock`, a heartbeat `lock::LinkLock` (§2.3). It never touches
another format's directory. `--dry-run` says what it would remove, and
removes nothing. An entry is removed by name after checking that the name
still leads to the file looked at: the same device, inode, size and
modification time (an entry republished meanwhile can have the old inode
number on ext4, never the old modification time). What `gc`
cannot know and need not: a use being logged while it runs (that entry may
go, which costs one miss), a restore of an entry it removes (the reader
keeps the file it opened).

**`cactup cache verify`** reads every entry as a restore does (§18.7): it
removes the invalid ones, as a restore would, leaves another cactup's
alone, and says how many of each it found (interrupted, how many it had
removed).

All three walk thousands of files on a network filesystem: they fan out
over the entry directories (`par::parallel_map`), show progress, and stop
on the first Ctrl-C with an "interrupted" error, having removed what they
say they removed.

**The size notice.** The knob `build-cache-size` (a size, `200G`; not set
by default) is frozen with the rest (§18.2). At the end of a serving or
auditing build, if it is set and the store is larger, the build says so in
one line and names `cactup cache gc`. A build never walks the store: the
size of the whole store (every machine's entries of this format) is kept
in `<root>/v2/size`, measured by `cache stats` and `cache gc`, and each
serving build adds the bytes it published and keeps the sum. Builds that
add at the same moment can lose each other's additions, so between
measurements it is an estimate, low if anything. Before the first
measurement the size is not known, and the build says that instead, naming
`cache stats`. Nothing is removed.

### 18.10 Fortran

gfortran is cached (`objcache::fortran`); other Fortran compilers are not
identified, and so not cached. Cactus hands gfortran a build copy it has
preprocessed itself (`.f`, `.f90`), compiled from the configuration's
`scratch`, which is where gfortran writes the module files of the modules a
source defines and where, first, it looks for the ones a source uses. A
source gfortran would preprocess itself (`.F`, `.F90`, `-cpp`), a module
directory of the compile's own (`-J`), more module search directories
(`-fintrinsic-modules-path`), a file read before the source
(`-fpre-include`) and a dependency file from the compile are not cached.

**What a compile reads.** Besides the source: module files (`use`, the
intrinsic ones included), included files (`include`), and the header the
driver pre-includes. No preprocessor output shows them, so gfortran is
asked: run on the source with the compile's arguments and `-cpp -undef -M
-fsyntax-only` (the *dependency run*), it prints a rule naming every file
it read, each by the name it was found under, and the module files it
writes as targets. `-fsyntax-only` runs the front end alone, which reads
every file the compile reads, and tells the driver nothing will be linked
(it would read `libgfortran.spec` for a link). A name the rule had to
escape is not read, and the compile is not cached.

That run needs the C preprocessor, which the compile does not run. `-D` and
`-U` do nothing to the compile (the compile gets them, as the recipe gave
them); the runs that preprocess do not. Even so, in
traditional mode with every macro it can drop dropped (`-undef`), the
preprocessor still acts on more than a Fortran source should give it: a
`/*` (which swallows lines up to the next `*/`), a line ending in `\`
(blanks after it too), a lone carriage return, the names it still defines
(`__FILE__`, `_OPENMP` and `_REENTRANT` under `-fopenmp`), trigraphs under
`-trigraphs`. Rather than list them, the preprocessor is run once more with
the same arguments and `-E`, and its output must be the source: every line
but its line markers, in order (lines blank on both sides aside, since it
may stand a marker for a run of them). Otherwise the compile is not
cached. All 594 of the Einstein Toolkit's Fortran build copies come back
from it byte for byte. (A source with CRLF line ends does not, since the
preprocessor drops the carriage returns: it is not cached.) A line beginning with `#` that is not a line marker
is a directive, which leaves no line to compare, and keeps the compile out
too.

The dependency run writes module files, and reads back the ones it wrote
as the compile does, so it runs in an empty directory of its own (where it
writes them, and where it finds them first) with the compile's working
directory first among its `-I` directories. Its search order is still not
the compile's: gfortran looks in the directory of the file it reads before
any `-I` directory, for module files as for included files, so the
compile searches its working directory, then the source's directory, then
its `-I` directories, and the dependency run the source's directory before
the working directory; and the dependency run searches the working
directory for included files, which the compile does not. So each module
file it read must be the first of its name in the compile's order (none of
that name in a directory the compile searches before it), and no included
file may be found under the working directory; otherwise the compile is
not cached. A file named like a module file (`.mod`, `.smod`) is checked
as one whatever its bytes, since gfortran reads module files through
zlib, which takes an uncompressed file as it is; only one that is
gzip-compressed, as gfortran writes them, is surely not an included file,
and any other is checked as an included file too. The rule for included files is the plain
one, not a check of the compile's order: it also turns away a Fortran
`include` of a file an external library installed below `scratch`
(`scratch/external/...`), a lost hit and never a wrong one, since which
name an included file was written by cannot be told from where it was
found. A module file it names in its own directory is its own output,
not an input. Both checks are made again after the compile.

The key's text part (§18.5) is the digest of the text compiled and of the
names of the module files it writes; its files part, of every file the
dependency run read, under its mapped name. Since module files are found
by names relative to the working directory, the working directory is
keyed for every Fortran compile, and so are the `-I` directories (mapped).
After the compile, what each file looked like must not have changed (§18.5),
and nothing may have appeared where the compile looks before the place it
found a file: gfortran looks for an included file, also one included from
an included file, in the directory of the file it compiles, then in its
`-I` directories, then in the intrinsic modules' directory (tried; not in
the including file's directory or the working directory), and treats
anything there as found (a directory there hangs it, tried); no module file
may appear named like a module gfortran has built in (`iso_c_binding`,
`iso_fortran_env`: a `use` that does not say `intrinsic` takes a file of
that name if it finds one, tried) where the compile looks for modules. An
included file named by an absolute path is opened by that name first, and
only where nothing is there searched for under each directory as
`<dir>//<name>` (the dependency run prints the double `/`, tried): found so,
the name itself is a place before it.
These places are looked at right before the compile (one already taken
fails the check: what appeared there since the key's dependency run may be
gone again before a second one could see it, tried) and again after it. Module files
keep their order check, when the key is made and after the compile, along
the `-I` directories as the compile names them, resolved when looked at (a
directory behind a symlink turned elsewhere leads elsewhere). And the
driver's own searches are repeated after the compile by the driver alone
(`-###`, which runs no compiler): it must read the same `specs` file or
none, and name the same compiler proper, pre-included header
(`-fpre-include=`) and intrinsic modules' directory as when it was asked
right before the compile (so a hit does not pay for it), in the compile's
working directory; and what the key's dependency run's own driver gave its
compiler proper (its `-v` says) must be the same, by absolute names; else
the dependency run reads other files than the compile would, and the check
fails. A hit compares none of this; what could lead the driver elsewhere
from another directory keeps the compile out of the cache when the key is
made (§18.5, Compiler; tried here, a `specs` file and a pre-included header found
through an empty `LIBRARY_PATH` entry). The file compiled is also watched by the name the
compile reads it by (through a symlink, maybe): the way that name leads is
the same when the key is made as before the source is read, and a copy
must hold the text keyed. No compiler runs for any of this.
Where an included file cannot be placed so (one named by an absolute
path and found there), the dependency run runs again after the compile
instead, and what it lists must be the same; but not where the compile
reads a copy (decision 13): the dependency run reads the source and is
blind to the copy's directory, so the check then fails and nothing is
stored. Left, as for a second dependency run: a file that appears and is
gone again between the key's dependency run and the check.

**What a compile writes.** The object and its module files. An entry keeps
all of them (§18.7). A hit puts back each module file whose bytes differ
from the one in the working directory, and leaves alone one that would not
change, as gfortran itself does (it keeps its modification time); audit
mode (§18.8) puts the stored
ones aside and compares them with the compile's, and a wrong module file
is a wrong hit. gfortran's module files are deterministic (gzip without a
time; the source named by its file name alone), so they are the same
across installations.

**Paths** (decision 13). gfortran writes a source's name into the object
for its runtime messages, and no `-f*-prefix-map` reaches it: the name it
was given ("In file '...', around line 7", from runtime checks), and the
name in a line marker, or else the one it was given ("At line 7 of file
..."). So under the path map a compile names its source by its path from
the working directory, `../build/<Thorn>/x.f90`, the same in every tree and
every configuration. A source whose line markers name files (Cactus writes
them with `F_LINE_DIRECTIVES = yes`, naming the original by its absolute
path) is compiled as a copy whose markers name their files by mapped names
(`/cactup-root/...`, as C's `__FILE__` under decision 8): under the
source's own file name (a module file records the name gfortran was
given), in `.cactup/` beside the source, named from the working directory
too (`../build/<Thorn>/.cactup/x.f90`, which is then what "In file"
messages name), with the source's own directory first among the `-I`
directories, right after the copy's own, where Fortran `include` would
have looked first (an included file of a name the copy's directory has
too, a copy of another source or one whose build copy is gone, keeps the
compile out; so does one named with `..`, which from the copy's directory
leads elsewhere than from the source's: the dependency run prints each
included file as it was named, after the directory it was found in). `.cactup/` is cactup's alone, and safe to delete: it is
written again by the next compile that needs it. Only the compile for the store reads the copy, so only a
serving or auditing build writes it, whole (a temporary file renamed into
place); it stays there like the build copy itself, and another compile of
the same source writes the same bytes. Its bytes are the key's text, and
after the compile it is looked at again with the files read: one changed
meanwhile keeps the object out of the store. Objects and module files from
two trees are byte-identical; the debug information names the source
relative to the compile directory, which the path map maps. The
compiler's messages name the source as it was given and the mapped names;
as they pass through, line by line, those are given back their real names
(`key::Rewrite`), so what the user reads, and what an entry keeps (§18.8),
is what the compile without the cache would have said (where the messages
cannot be passed through the wrapper, the recipe's own compile runs).
`relocates`' trial for gfortran compiles a module this way in two trees,
with and without a line marker, with runtime checks on, and compares both
objects and both module files. In record mode, and under
`build-cache-relocate = no`, the compile reads the source as the recipe
gave it.
