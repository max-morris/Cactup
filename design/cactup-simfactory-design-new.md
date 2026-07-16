# cactup: Subsuming SimFactory — Design Specification

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
| D7 | `@VAR@` substitution engine fidelity | **Literal `@NAME@` replacement plus the one computed form `@ENV(NAME)@`** (the named environment variable, read at substitution time; unset or empty = hard error), everywhere (TOML and shell templates). simfactory's `@(expr)@` Python-eval and ternary/word-operator sugar are **not** ported. Scripts and parfiles needing further logic use the Python `.py` variant escape hatch (see §6). |
| D8 | Machine-level thorn enable/disable toggles | **Kept** (see §7.5). |
| D9 | Optionlist on-disk format | **TOML + render step.** Optionlists are authored as TOML; cactup renders them to the native Cactus `NAME = value` optionlist before `make`. Render rules, the `VERSION` semantics, and ordering are specified in §7.8. |
| D10 | Pre-existing simfactory simulation dirs | **Greenfield / ignore.** cactup manages only simulations it created. A directory is recognized as a cactup simulation **iff** it contains `.cactup/simulation.toml`. cactup neither reads nor migrates legacy `SIMFACTORY/` simulations. |
| D11 | Global-DB locking | **Brief lock around DB access only.** The exclusive lock is held only while reading/mutating/persisting the database — never across a compile or a simulation run. Per-simulation coordination uses the simulation's own on-disk state, not the global lock (see §2.3). |
| D12 | OptionList-variant ↔ queue compatibility | **The optionlist variant declares its compatible queues** (and therefore which run/submit variants it can pair with). `sim submit`/`sim run` enforce it (see §4.4, §7.4). |

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
  "installation directory" is one level above the Cactus root.
- **Active installation** — the default target for installation-local commands.
- **config** — a particular build of Cactus within an installation (compile
  flags + thornlist). Each installation has an **active config**.
- **simulation** — a run of a config with a given parfile; owns its output
  directory and supports checkpoint/restart.
- **test run** (**test-sim**) — one execution of a config's testsuite (`make
  <config>-testsuite`); the simplified, one-shot analogue of a simulation,
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
- `installations`: alias → `{ alias, release, path }`
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
minutes-to-days — `config build` (compiles Cactus) and `sim run` (runs the
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
4. **Per-config build lock.** `config build` releases the global lock before
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

---

## 3. CLI surface

Top-level (clap, mirroring `src/args.rs` style). Global flags: `-v/--verbose`,
`--manifest-url`, `--mdb-path` (new), `--machine <name>` (new; overrides
discovery — §4.3), `--installation <alias>` (new; target a non-active
installation for one command instead of `cactup use`-ing it first).

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
cactup list [--all]                                                       (existing)
cactup show                                                               (existing; lists installations)
cactup use <alias>                                                        (existing; set active installation)
cactup uninstall <alias> [-f]                                             (new; see §3.1)

cactup config build <name> [-f] [--thornlist P] [--variant V] [--universe U | --no-universe] [build flags…]
cactup build …                       (alias for `config build`)
cactup config show [<name>]
cactup config use <name>
cactup config delete <name>

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
| `sim build` | `cactup config build` / `cactup build` | §7 |
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
| `sync`, `--remote`, `login`, `checkout`, `execute` | *dropped* (D1) | `checkout` is replaced by `GetComponents` at install time (already in `install`). |
| `get-archived-simulation`, `list-archived-simulations` | *dropped* (D2) | |
| `setup` / `setup-silent` | `cactup machine create [--silent]` (§4.7); `install` calls the silent form on an unrecognized host | Creates a local machine in the user MDB from `generic` with autodetected hardware. No per-tree `defs.local.ini`. |
| `remove-submitscript` | *dropped* | Submit/run scripts are resolved at submit time from the MDB, not baked into the config (§7.6), so there is nothing to remove. |

---

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
  consume**, and renamed for what they mean to cactup (honouring the task=rank
  / CPU=core terminology): `ppn` → `max-cpus-per-node` (it is CPUs/cores per
  node, **not** ranks), `num-threads` → `default-cpus-per-task` (the default
  `CPUS_PER_TASK` when `--cpus` is omitted), `num-smt` → `threads-per-cpu`,
  `memory` kept (plus the `autodetect` control flag, §4.6). simfactory's
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
  analogue of `simulation-home` for the `cactup test` subsystem (§11.5); optional,
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
`enabled-thorns`, `disabled-thorns`), `[environment]` (`env-setup` and the
phase-specific `env-build-setup` / `env-submit-setup` / `env-run-setup` — §6.1;
grouped here rather than under `[scheduler]` because `env-setup` now spans build
as well as submit/run), `[scheduler]` (`submit`, `get-status`, `stop`, the
`*-pattern`s, `exec-host`, `stdout`/`stderr`), then `[queues.*]`, `[variants.*]`,
and (optional) `[universes.*]` (§4.8 — a wrapper spec and/or per-universe
`env-*-setup` overrides; the always-available `"host"` universe needs no table
at all unless it is being customized).

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
# threads-per-cpu = 1          # optional; defaults to 1 (§8.5)

[scheduler]
submit = "sbatch @SCRIPTFILE@ 2>&1"
get-status = "squeue -j @JOB_ID@"
# … patterns, stop, … …

[environment]
env-setup = """
module load gcc/11 openmpi/4
"""                            # applied to build, submit, AND run (§6.1)
env-build-setup = "module load cmake/3.27"   # appended only for `config build`
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
availability fact = CPUs/cores per node, **not** ranks; drives the §8.5
process-layout defaults and `@MAX_CPUS_PER_NODE@`), `default-cpus-per-task`
(simfactory's `num-threads`; the request-side default for `CPUS_PER_TASK` when
`--cpus` is omitted — §8.5), `memory` (`@MEMORY@`, per-node MB), and
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
  user **must** pick one at `config build` time via `--variant`; there is no
  default. The chosen variant is recorded in the config metadata (§7.4).
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
    optionlists/debug.toml       # DEBUG optionlist; reached with config build --variant debug (§4.4)
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
§4.2), cactup fills the missing top-level values at load time from the OS:

| Var | Linux | macOS |
|-----|-------|-------|
| `max-cpus-per-node` | `nproc` (or `/proc/cpuinfo`) | `sysctl -n hw.ncpu` |
| `memory` (MB) | `/proc/meminfo` `MemTotal` | `sysctl -n hw.memsize` |

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

- **build** — the `make` invocations of `config build` (§7.2).
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

1. CLI `--universe <name>` / `--no-universe` (on `config build`, `sim run`, or
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
cactup knob                       # print all knobs and values
cactup knob <name>                # print one knob
cactup knob <name> <value>        # set one knob
```

Recognized knobs (from `cactup-simfactory-design.txt`): `allocation`, `mail`,
`mail-type` (default `all`), `queue`. **ASSUMPTION:** also `account`-style
extras (`user`, `email`) are derived automatically (`$USER`, `git config
user.email`) the way simfactory's `setup` did, but can be overridden as knobs.

**Storage:** knobs live in the **global database** (`~/.cactup/database.json`)
as a single flat map — a `~/.cactup` lives on exactly one machine, so there is
nothing to key them by. This is consistent with D4 (the global DB holds global
cactup state).

```jsonc
// database.json (excerpt)
"knobs": { "allocation": "hpc_xxx", "mail": "me@lsu.edu", "mail-type": "all", "queue": "checkpt" }
```

### 5.1 Value precedence

For any value that can come from several places (mirrors `simfactory-docs.txt`
§6.2, adapted):

1. Explicit CLI flag (e.g. `-q/--queue`, `-a/--allocation`) — highest.
2. Knob.
3. Machine `meta.toml` value / default.
4. Built-in default.

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

Per D7, cactup implements **literal `@NAME@` replacement plus the one
computed token form `@ENV(NAME)@`**: the value of environment variable
`NAME`, read at substitution time — which always happens on the machine in
question — with an unset or **empty** `NAME` being a hard error, never an
empty splice. `@ENV()@` is the mechanism for machine paths only the machine's
environment knows (TACC/LRZ-style hashed storage roots in `[paths]`, §4.2);
because of it, `[paths]` values are resolved at **use time**
(`Meta::resolved_paths`), not at MDB load, so entries for other machines
still load and validate everywhere. There is no expression evaluation and no
ternary sugar. Any MDB script that needs conditional logic (e.g. simfactory's
`@("@CHAINED_JOB_ID@" != "" ? "-d afterany:@CHAINED_JOB_ID@" : "")@`) is
rewritten as a Python `.py` variant.

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
   (`typed["NODES"] == 4`) so authors needn't re-parse. The JSON-on-stdin choice
   keeps values out of the process table and argv length limits.

**`env-setup` handling — the effective block and where it is injected.** For any
given phase (build/submit/run), the **effective env-setup** is the concatenation
of the machine's `env-setup` and that phase's optional `env-<phase>-setup`
(`env-build-setup` / `env-submit-setup` / `env-run-setup`, §4.2), in that order,
joined by a newline (an unset companion contributes nothing). The `ENV_SETUP`
variable (§6.3) always holds the **already-combined** effective block for the
current phase, so scripts and `.py` authors never see the split. Injection then
depends on the artifact:

- **Build (`config build`, §7.2):** the build has no submit/run *script* — cactup
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
    and written as the ready-to-run `<basename>.par`. (Literal `@` in a `.par` is
    written `@@`, which the run-time substitution collapses to a single `@` —
    §6.1.)
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
`CPUS_PER_TASK` (`-c`/cpus), `GPU` (`-g`; `1`/`0`), `ALLOCATION` (`-a`),
`QUEUE` (`-q`; the scheduler-facing name — the selected queue's `name` override
when set, else its `[queues.<q>]` key — §4.2), `MAIL` (`-m`), `MAIL_TYPE` (`-M`),
`JOB_NAME` (`-j`),
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
`MEMORY` (per-node MB), `THREADS_PER_CPU` (from `threads-per-cpu`, default 1;
§8.5 assumption), `ENV_SETUP` (the **effective** env-setup
block for the current phase — `env-setup` plus the phase's `env-<phase>-setup`,
already combined; auto-prepended for `.sh`, author-emitted for `.py` — §6.1).

**Build-time only** (optionlists/build): `MAKEJOBS`, `DEBUGGER`, `RUNDEBUG`.

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
cactup config build <name> [-f] [--thornlist P] [--variant V] [--universe U | --no-universe] [flags…]
cactup build <name> …              # alias
cactup config show [<name>]
cactup config use <name>
cactup config delete <name>
```

- `build`: builds (or rebuilds with `-f`) config `<name>` in the active
  installation. `--thornlist` defaults to
  `<Cactus root>/thornlists/einsteintoolkit.th`. `--variant` selects the
  optionlist variant (required iff the machine has >1 optionlist variant — §4.4).
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
has been built and selected. Before the first successful `config build`, or after
the last config is deleted, the installation is in the **null-config state**: its
`installation.toml` records no active config. Any command that needs a config
(`sim create`, and the implicit-create path of `sim submit`/`sim run` when
`--config` is omitted) fails fast in this state with guidance to build or select
one. `config build` and `config show` remain available (that is how you leave the
state); the first build automatically becomes active. The null-config state is
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
variant = "gpu"                 # optionlist variant used
gpu = true                      # copied from the optionlist [cactup].gpu at build (D12)
compatible-queues = ["gpu"]     # copied from the optionlist [cactup].compatible-queues (D12)
thornlist = "thornlists/einsteintoolkit.th"
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
**source optionlist TOML** verbatim to `configs/<name>/cactup-optionlist.toml`.
The rebuild decision (§7.8 rule 5) diffs the freshly-selected source TOML against
this stored copy; *any* difference triggers a full realclean + reconfigure +
rebuild. This is the source-of-truth for "did the optionlist change?" — the
rendered native file is never diffed (it exists only for the Cactus build system
to consume, §7.8). The recorded **`universe`** is compared the same way (its value
can come from CLI/machine default, not just the optionlist TOML, so it is checked
separately): a resolved universe that differs from the stored one also forces a
full realclean + rebuild, since a host build and an in-container build are not
interchangeable (§4.8).

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

**Rebuild trigger (one rule).** The decision to rebuild is made by diffing
the freshly-selected **source optionlist TOML** against the copy stored at build
time (`configs/<name>/cactup-optionlist.toml`, §7.4). *Any* difference — a
changed flag, a new key, or a bumped `VERSION` — triggers a full
`make <config>-realclean` + reconfigure + rebuild. (So a `VERSION` bump forces a
rebuild only *because* it is a diff; there is no separate VERSION-only path.) The
rendered native file plays no part in this comparison and its comments — which
TOML drops on parse — are irrelevant, since nothing diffs it. This collapses
simfactory's finer "VERSION → realclean vs. other change → reconfigure-only"
distinction into "any change → full rebuild": simpler and always safe, at the
cost of a from-scratch rebuild on every optionlist edit.

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
one `build-id` for a config exists at a time. When `config build` produces a new
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
6. Copy the source optionlist and the parfile into the simulation metadata
   (`.cactup/cfg`, `.cactup/par`).

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
| `-j/--job-name` * | job name | `<SimName>` |
| `-w/--wall-time` * | total walltime, canonical format below | machine/queue default |
| `-o/--out` * | stdout filename | template default |
| `-e/--err` * | stderr filename | template default |

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
  `max-cpus-per-node` (§4.2).
- `TASKS` = `--tasks` if given, else `NODES * TASKS_PER_NODE`.
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
  cfg/  par/                       master copies of the optionlist + parfile (§8.2)
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
without re-reading the MDB), and the creation/marking timestamps that simfactory
kept in the separate `timestamp`/`simulation` mark files — folded in here).
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
  test run needs nothing a normal `config build` doesn't already produce. `test
  run`/`test submit` target `--config C`, defaulting to the installation's
  **active config** (§7.4) — the same config a bare `sim run` would use. There is
  no `test build`, no `test use`, and no `active-test-config`.

  A config built with a DEBUG optionlist (to surface assertion/bounds errors the
  testsuite is meant to catch) is just a config built with `config build
  --variant <debug>` — see §4.4 optionlist selection; the old test-optionlist
  partition is gone.
- **test run** (a.k.a. **test-sim**) — one execution of a config's testsuite
  against a chosen topology and test selection. It is the analogue of a
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
under test uses whatever optionlist its `config build` selected.)

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
(A config is built with `cactup config build` — there is no `test build`.)

Notes:
- `[<test>…]` is the optional test selection (`test run` / `test submit`). Omitted
  ⇒ run **all** tests (the successor to simfactory's `--select-tests all`
  default). A selector is a test name, a thorn (`arrangement/Thorn`), or an
  arrangement; cactup passes the resolved selection to the flesh harness (§11.6).
- `--config C` defaults to the **active config** (§7.4); both `test run` and
  `test submit` fail fast in the null-config state with guidance to
  `config build`. The config must be a complete build.
- Unlike `sim submit`/`sim run`, there is **no parfile argument and no implicit
  create** — a test run's "parfile" is the thorn test data, chosen by `[<test>…]`.
  (This is where simfactory's empty-parfile `""` sentinel goes away entirely.)
- `-f`/`--overwrite`/`--force-queue`/`--universe`/`--no-universe` carry the same
  meanings as on `sim run`/`sim submit` (§3, §4.4, §4.8).

### 11.4 Building the config under test

There is no `test build`. A testsuite runs the binary that `cactup config build`
already produces (`make <config>-testsuite` invokes it), so any complete config
is runnable as-is. Nothing about a build is test-specific: the `configs/<name>/`
layout (§7.2), the make flow, the optionlist render (§7.8), and the metadata
(§7.4) are exactly as §7 describes.

If you want the testsuite to run against a DEBUG binary (assertions / bounds
checking, to surface errors the tests exist to catch), build the config with a
DEBUG optionlist variant: `cactup config build <name> --variant <debug>` (§4.4).
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
  — the test analogue of `simulations.toml` (§8.1): `<TestName>` → `{ dir,
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

- `TEST_HOME` — the per-alias test output root (§11.5), the test analogue of
  `SIM_HOME`.
- `TEST_DIR` — absolute path to this test run's directory (passed to the
  compute-node re-invocation as `--test-dir`, §11.6), the analogue of
  `SIMULATION_DIR`.
- `TEST_NAME` — the test-run name (analogue of `SIMULATION_NAME`).
- `RESULTS_ID` — the active `results-%04d` id (analogue of `RESTART_ID`).
- `TESTSUITE_RESULTS_DIR` — absolute path to the active `results-%04d`, where the
  harness must write (§11.6, step 6).
- `TESTSUITE_SELECT` — the test selection (`all` or the resolved selector list,
  §11.3), the successor to simfactory's `select-tests`.

`CONFIGURATION` (§6.3) is the config name for `make @CONFIGURATION@-testsuite`;
`TASKS` (§6.3) feeds `CCTK_TESTSUITE_RUN_PROCESSORS`; `EXECUTABLE`, `SOURCEDIR`,
`ENV_SETUP`, and the topology/scheduler variables are the existing §6.3 ones. No
new *build-time* variables are needed — the config is built by ordinary
`config build` (§7.8).

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
| Parfile | user-supplied `.par` / executable `.rpar` | user-supplied `.par` (literal `@NAME@`) / `.py` variant (JSON-on-stdin, emits `.par` to stdout — §6.1/§6.2) |
| Config metadata | `configs/<name>/properties.ini` | `configs/<name>/cactup-config.toml` |
| Sim metadata | `SIMFACTORY/properties.ini` | `.cactup/simulation.toml`, `.cactup/restart.toml` (schema-versioned) |
| Sim detection | dir has `SIMFACTORY/properties.ini` | dir has `.cactup/simulation.toml` (greenfield — D10) |
| Sim output dirs | `output-%04d/…` | **identical (preserved)** |
| Active restart | `output-NNNN-active` symlink | **identical (preserved)** |
| Substitution | `@NAME@` + `@(expr)@` + `@ENV()@` | **`@NAME@` + `@ENV(NAME)@`** (unset/empty env = hard error); `.py` for logic (JSON-on-stdin convention, §6.1) |
| cactup binary var | `@SIMFACTORY@` | `@CACTUP@` |
| Machine detection | `aliaspattern` regex on hostname | `discover.py`; result cached in DB as a single `detected-machine` string (not per-hostname — §4.3) |
| Per-installation state | n/a | `<installation home>/.cactup/installation.toml` (active config, sim-home) + `simulations.toml` (name→dir registry) |
| Sim root key | machine `basedir` | machine `simulation-home` (optional; falls back to `~/.cactup/simulations`) — §8.1 |
| Test-suite command | `sim create --testsuite` (overloads `sim`) | `cactup test run`/`submit` against any built config (own command tree — §11) |
| Test output root | inside a simulation dir (`output-NNNN/exe/…`) | machine `test-home` (optional; falls back to `~/.cactup/tests`) — §11.5 |
| Config under test | n/a (a `testsuite` property on a sim) | any built config; `--config C` defaults to the active config (no separate test-config kind) — §11.1 |
| Test run metadata | sim `properties.ini` + `output-NNNN/exe/` copytree | `<test-home>/…/<name>/.cactup/test.toml` + `tests.toml` registry (§11.8); no copytree (§11.10) |
| Test-script marking | separate faked machine defs | `test = true` on the meta.toml run/submitscript variant entry (§11.2) |
| Install root key | machine `sourcebasedir` (source base; also sync/disambiguation) | machine `install-home` (optional default install prefix; falls back to `~/.cactup/cacti`; `--install-prefix` overrides) — §4.2 |
| Locking | none (per-tree) | `link()`-based (NFS-safe) global-DB lock + per-sim lock + per-config build lock (D11, §2.3) |
| Execution universe | faked via separate machine defs (e.g. `db-sing-*`) | `[universes.*]` command-wrapper in `meta.toml`; wired for `config build`, `sim run`, and `sim submit` (§4.8) |

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
    `config build`, `sim run`, and `sim submit` — via one `[universes.*]` registry
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
