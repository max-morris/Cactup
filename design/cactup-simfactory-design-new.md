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
| D7 | `@VAR@` substitution engine fidelity | **Literal `@NAME@` replacement plus two computed token families, `@ENV(…)@` and `@KNOB(…)@`**, each in a required form (unset or empty = hard error) and two `-OPTIONAL` forms (empty, or a quoted/bare default) — see §6.1. `ENV` reads the named environment variable at substitution time; `KNOB` reads the knob snapshot frozen with the variable set (§5). Everywhere (TOML and shell templates, parfiles). simfactory's `@(expr)@` Python-eval and ternary/word-operator sugar are **not** ported. Scripts and parfiles needing further logic use the Python `.py` variant escape hatch (see §6). |
| D8 | Machine-level thorn enable/disable toggles | **Kept** (see §7.5). |
| D9 | Optionlist on-disk format | **TOML + render step.** Optionlists are authored as TOML; cactup renders them to the native Cactus `NAME = value` optionlist before `make`. Render rules, the `VERSION` semantics, and ordering are specified in §7.8. |
| D10 | Pre-existing simfactory simulation dirs | **Greenfield / ignore.** cactup manages only simulations it created. A directory is recognized as a cactup simulation **iff** it contains `.cactup/simulation.toml`. cactup neither reads nor migrates legacy `SIMFACTORY/` simulations. |
| D11 | Global-DB locking | **Brief lock around DB access only.** The exclusive lock is held only while reading/mutating/persisting the database — never across a compile or a simulation run. Per-simulation coordination uses the simulation's own on-disk state, not the global lock (see §2.3). |
| D12 | OptionList-variant ↔ queue compatibility | **The optionlist variant declares its compatible queues** (and therefore which run/submit variants it can pair with). `sim submit`/`sim run` enforce it (see §4.4, §7.4). |
| D13 | Linking & system libraries | **Fully static MUSL binary.** cactup deploys to clusters as a single copyable binary: the release artifact targets `x86_64-unknown-linux-musl` and must stay fully statically linked (`ldd`: "statically linked"). No crate that binds a system shared library (no openssl/native-tls — reqwest uses rustls; no libgit2 — git is pure-Rust gix; no pkg-config'd C deps); C code a dependency compiles in statically at cargo-build time is fine. Our own code never uses the `libc` crate directly — OS facts come from `/proc` or std (e.g. §2.3's pid probing). |

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
- **`detected-machine`** (new; see §4.3) — the resolved machine name for *this*
  machine, a single string (**not** keyed by hostname). A given
  `~/.cactup` logically lives on one machine, so login and compute nodes of a
  cluster share one value transparently.

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
| `CACTUP_ROOT` | `~/.cactup` (existing) |
| Installations root (default) | `<install-home>/<alias>/`, where `install-home` defaults to `~/.cactup/cacti` when the machine omits it (§4.2) |
| System MDB (production) | `~/.cactup/mdb` (cloned from a dedicated git repo; **read-only** to cactup, overwritten on update) |
| System MDB (development) | a hard-coded absolute path to `<project root>/mdb` |
| **User MDB (writable overlay)** | `~/.cactup/machines/` (user-created/customized machines; never touched by MDB updates) |
| Database | `~/.cactup/database.json` |

**ASSUMPTION:** MDB source resolution mirrors the manifest repo handling already
in `src/manifest.rs`: in production cactup clones/updates the MDB git repo into
`~/.cactup/mdb`; in development a compile-time-selected constant points at the
in-repo `mdb/`. A `--mdb-path` global flag overrides for testing.

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
that subtree onto its one item: labelled with the phase running now, filled
by whichever phase is actually *moving* (the server's "Compressing objects"
until the pack starts, then the pack's bytes, then the checkout's files),
and silent about gix's own chatter. The renderer's level filter stops at the
wrapped item. One `progress::Layout`, built per renderer from every name the
batch will carry, fixes the width of the name column across all of its lines
(the headline and the scrollback included), so the phase, the numbers and
the bar start in the same place on every line rather than sliding about as
the phases change; the same layout is what renders the component's name
louder than the phase it is in, since prodash paints a task's whole name in
one style and the only way to split the two is to end that style inside the
string. Work that counts for itself with no phases — a plain download
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
    discover.py                     # def is_machine(hostname: str) -> bool
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
machine counts cactup deals with, and one fewer file to keep in sync.

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
both layers, only the user-MDB copy participates — only its `discover.py` is run
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
- `aliaspattern` (hostname regex) is **removed**; replaced by `discover.py`
  (§4.3).
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
(`install-home`, `simulation-home`, `test-home`, `scratch-home`), `[hardware]` (`max-cpus-per-node`,
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

### 4.3 Discovery functions (`discover.py`)

Replaces simfactory's `aliaspattern` hostname regex (`simfactory-docs.txt` §9).

- Each machine ships `mdb/<machine>/discover.py` containing
  `def is_machine(hostname: str) -> bool:`.
- cactup determines the local `hostname` (`--hostname` override → `~/.hostname` →
  system FQDN) and **passes it as the argument** to every machine's
  `is_machine(hostname)`. The implementation may use the supplied string or
  ignore it and do its own probing (e.g. read an env var, check a sentinel
  file). Exactly one `True` → that machine. More than one → **prompt the user to
  disambiguate** — **except in non-interactive mode** (`--silent`, or no tty),
  where cactup cannot prompt: it then **errors and requires `--machine <name>`**
  to pick one explicitly. (Recall name-shadowing already removes the
  common self-collision, §4.1, so a genuine multi-match means two distinct
  machines both claim this host.)
- **Zero matches → fall back to `generic`, do not fail (§4.6).** An unrecognized
  host (e.g. a personal laptop with no scheduler) is the *common* case for
  newcomers, not an error. cactup uses the built-in `generic` machine — with
  hardware autodetected at runtime — so the command works out of the box, and
  prints a one-line notice suggesting `cactup machine create` (§4.7) to persist a
  tuned local machine. (The `generic` machine's own `discover.py` returns
  `False`, so it never participates in matching; it is *only* the zero-match
  fallback.)
- `--machine <name>` overrides outright and skips discovery entirely (e.g.
  `--machine generic`).

**Discovery result is cached persistently, not "per session."** cactup is a
one-shot CLI — there is no session to remember a choice in. The resolved machine
(including a disambiguation choice) is cached in the global DB as a **single
string** — **not** keyed by hostname:

```jsonc
// database.json
"detected-machine": "mike"
```

Rationale: a given `~/.cactup` logically belongs to one machine and does not
move, so keying by hostname would only fragment the cache across a cluster's
many login/compute node FQDNs (`mike1`, `mike2`, …) and re-prompt on each. A
single value makes all of a cluster's nodes resolve transparently. (Edge case: a
`~/.cactup` on a home filesystem *shared* between two genuinely distinct
clusters would need `--machine` or `machine forget`; this is rare and accepted.)
A cache hit skips re-running discovery (and re-prompting). `--machine` and
`cactup machine forget` bypass/clear the cache.

**ASSUMPTION (Python runtime):** cactup shells out to a `python3` on `PATH` to
evaluate discovery and `.py` script variants (cactup is Rust; embedding CPython
is heavier than needed). A machine whose `discover.py` raises is treated as "did
not match" with a warning under `-v`.

**Security & cost note.** `discover.py` (and `.py` script variants) are arbitrary
code from the MDB git repo, executed on the user's machine. The MDB repo is
therefore a **trust boundary**: cactup runs only the MDB it cloned from the
configured `manifest`/MDB URL, and the spec assumes that repo is trusted. To
avoid spawning Python on every command, discovery runs **only** on a
`detected-machine` cache miss (or `--machine` absence), and the `.py` calling
convention (§6.1) batches all variable injection into a single `python3`
invocation per script.

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
    discover.py                  # is_machine(): FQDN == melete05.cct.lsu.edu
    optionlists/default.toml     # ported from mel5.cfg; [cactup] gpu=false + [options]
    optionlists/debug.toml       # DEBUG optionlist; reached with cactup build --variant debug (§4.4)
    runscripts/default.sh        # @NUM_PROCS@→@TASKS@, @NUM_THREADS@→@CPUS_PER_TASK@
    runscripts/test.sh           # test=true; drives make <config>-testsuite → test-home (§11.6)
    submitscripts/default.sh     # @SIMFACTORY@→@CACTUP@, +--installation, PID-wait chaining
    submitscripts/test.sh        # test=true; re-invokes `cactup test run`, no chaining (§11.6)
```

It exercises every load-bearing new mechanism: discovery function, grouped
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
- Its `discover.py` returns `False` — `generic` is never auto-matched; it is the
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
- **Generates a `discover.py`** that matches the current host (exact FQDN, plus
  short-name match) so the machine is auto-detected on subsequent runs, *and*
  sets the `detected-machine` value (§4.3). Pass `--no-discover` to create a
  machine that is only selectable via `--machine` (useful for cloning a remote
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
    the ready-to-run `<basename>.par`. (Literal `@` in a `.par` is written
    `@@`, which the run-time substitution collapses to a single `@` — §6.1.)
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
portable across machines). The resolved build **`universe`** (§4.8) is recorded
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
cactup sim create [-f] <sim> <parfile> [--config C] [--sim-dir P]
```

Port of `create()` (`simfactory-docs.txt` §14.1):
1. Resolve config = `--config` or the installation's active config; locate
   `<Cactus root>/exe/cactus_<config>` (fatal if missing).
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
cactup sim submit [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <sim> <TOPOLOGY…>
cactup sim submit [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <sim> <parfile> [--config C] <TOPOLOGY…>   # implicit create
```

Port of `submit()` (`simfactory-docs.txt` §14.2):
- If `<sim>` doesn't exist and a parfile is given, create it first (the
  `create-submit` fusion). If `<sim>` **does** exist and a parfile is *also*
  given, that is an **error** unless `--overwrite`/`-f` is passed — cactup
  will not silently ignore a parfile that contradicts an existing sim.
- Validate the built config's `compatible-queues` against the chosen queue
  (§4.4 / D12); error on mismatch unless `--force-queue`/`-f`.
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
cactup sim run [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <sim> <TOPOLOGY…> [--debug]
cactup sim run [-f] [--overwrite] [--force-queue] [--universe U | --no-universe] <sim> <parfile> [--config C] <TOPOLOGY…>
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
   D12) exactly as a sim does; `--force-queue`/`-f` overrides.
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
| Machine discovery | `aliaspattern` regex | `mdb/<m>/discover.py` |
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
| Machine detection | `aliaspattern` regex on hostname | `discover.py`; result cached in DB as a single `detected-machine` string (not per-hostname — §4.3) |
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
   `--mdb-path` override (§2.2, §4).
2. Python runtime: shell out to `python3` for `discover.py` and `.py` script
   variants rather than embedding CPython (§4.3).
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
| 9 machine detection | `discover.py` (§4.3) |
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
