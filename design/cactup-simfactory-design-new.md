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
| D3 | Cactus test-suite support | **Dropped for v1.** Deferred out of this spec; to be designed later. cactup has no `--testsuite` path yet. |
| D4 | Restart / chaining / recovery & on-disk metadata | **On-disk simulation output preserved** (numbered `output-%04d` restarts, the `output-NNNN-active` symlink, checkpoint recovery, `CACHE/`, `TRASH/`). simfactory's `SIMFACTORY/` metadata dir and `properties.ini` are an **implementation detail and are NOT preserved** — cactup uses its own TOML metadata. Per-simulation state lives in the simulation's own folder; the global cactup database holds only global cactup state and the installation registry. |
| D5 | Where simulations live | `<sim-home>/<config>/<SimName>/...`, where the per-alias `<sim-home>` = `<machine simulation-home>/<alias>` (falling back to `~/.cactup/simulations/<alias>` when the machine omits `simulation-home`). The chosen sim-home is fixed at install time and recorded per-installation; a single simulation's directory may be overridden at create time with `--sim-dir` (see §8.1). There is no `--basedir` flag. |
| D6 | Config-level metadata storage | Per-installation **on-disk TOML**, not the global DB (see §7.4). |
| D7 | `@VAR@` substitution engine fidelity | **Literal `@NAME@` replacement only**, everywhere (TOML and shell templates). simfactory's `@(expr)@` Python-eval, ternary/word-operator sugar, and `@ENV(NAME)@` are **not** ported. Scripts and parfiles needing logic use the Python `.py` variant escape hatch (see §6). |
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
   chaining, checkpoint recovery, test suites, scheduler abstraction.
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
- **MDB (machine database)** — per-cluster scripts and metadata.
- **knob** — a machine-global default value (allocation, email, queue, …).

---

## 2. Process model & global state

### 2.1 The global database (`~/.cactup/database.json`)

Already implemented (`src/database.rs`). It is the **only** global mutable state
and is concerned **exclusively** with global cactup state:

- `cactup-version`
- `installations`: alias → `{ alias, release, path }`
- `active-installation`
- **knobs** (new; see §5) — machine-global defaults.
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
   executable, the config, the parfile, topology, recovery source — is read from
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

cactup config build <name> [-f] [--thornlist P] [--variant V] [build flags…]
cactup build …                       (alias for `config build`)
cactup config show [<name>]
cactup config use <name>
cactup config delete <name>

cactup sim create [-f] <sim> <parfile> [--config C] [--sim-dir P]
cactup sim submit [-f] [--overwrite] [--force-queue] <sim> [<parfile> --config C] <TOPOLOGY…> [--no-recover] [--restart-id N]
cactup sim run    [-f] [--overwrite] [--force-queue] <sim> [<parfile> --config C] <TOPOLOGY…> [--debug] [--no-recover] [--restart-id N]
cactup sim stop   <sim> [-f]
cactup sim clean <sim>
cactup sim delete <sim> [-f]
cactup sim show [<sim>] [--long] [--all]           (--all: across every installation — §8.1)
cactup sim output-dir <sim> [--restart-id N]      (prints active/Nth restart dir)
cactup sim log <sim>                              (tail stdout/err / formaline)

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
| `--testsuite` / test-suite run | *dropped for v1* (D3) | Deferred; no `--testsuite` path in this spec. |
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
- `env-setup` (shell setup, usually module loads) is kept verbatim — it is
  prepended to every submit/run script (§6).
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
- Hardware/capacity keys kept verbatim: `ppn`, `spn`, `mpn`, `nodes`,
  `num-threads`, `max-num-threads`, `num-smt`, `max-num-smt`, `min-ppn`,
  `memory`, `cpu-freq`, `flop-per-cycle`, cache descriptors, `efficiency`, `quota`,
  `cpu`.
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
`max-walltime`, `scratchbasedir` → `scratch-home`, `cpufreq` → `cpu-freq`,
`flop/cycle` → `flop-per-cycle`, …), and cactup's own keys use hyphens too
(`config-id`, `build-id`, `job-id`, `chained-job-id`, `from-restart-id`,
`simulation-id`, `compatible-queues`). The **only** identifiers that keep
underscores are **template substitution variables**, which stay `UPPER_SNAKE`
inside `@…@` (a deliberately separate namespace — `@JOB_ID@`, `@SCRATCH_HOME@`,
`@TASKS_PER_NODE@`).

**meta.toml table structure.** Scalar keys are grouped into tables for clarity
(TOML requires top-level keys before any table, so grouping avoids ordering
pitfalls). The tables are: `[machine]` (descriptive + access), `[paths]`
(`install-home`, `simulation-home`, `scratch-home`), `[hardware]` (`ppn`, `nodes`,
`memory`, `num-threads`, `cpu-freq`, …), `[build]` (`make`, `make-jobs`,
`enabled-thorns`, `disabled-thorns`), `[scheduler]` (`submit`, `get-status`,
`stop`, the `*-pattern`s, `exec-host`, `stdout`/`stderr`, `env-setup`), then
`[queues.*]` and `[variants.*]`.

```toml
[machine]
name = "mike"
nickname = "mike"
status = "production"          # personal|experimental|production|storage|outdated
hostname = "mike.hpc.lsu.edu"
# … location, description, etc …

[hardware]
ppn = 16
nodes = 360
# … memory, cpu-freq, num-threads, … …

[scheduler]
submit = "sbatch @SCRIPTFILE@ 2>&1"
get-status = "squeue -j @JOB_ID@"
# … patterns, stop, env-setup, … …

[queues.checkpt]               # one table per queue
gpu = false
max-walltime = "72:00:00"
default = true                 # the queue used when -q is omitted (one queue may be default)

[queues.gpu]
gpu = true
max-walltime = "24:00:00"

# Variant → queue association (§4.4)
[variants.submitscript]
"slurm-cpu" = ["checkpt", "single"]   # this variant serves these queues
"slurm-gpu" = ["gpu"]
default = "slurm-cpu"                  # variant for queues with no explicit mapping

[variants.runscript]
"cpu" = ["checkpt", "single"]
"gpu" = ["gpu"]
default = "cpu"

# Optionlists have NO default unless there is exactly one variant (§4.4).
# Each variant just names the optionlist file under optionlists/<variant>.toml;
# that file declares its own gpu flag and compatible queues (D12, §7.8).
[variants.optionlist]
variants = ["cpu", "gpu"]               # selected at build time via --variant
```

Queue/variant consistency requirement: every queue listed in `[queues.*]` must
be served by some `submitscript` variant and some `runscript` variant (an
explicit mapping or the `default`). cactup validates this at MDB load and errors
on a queue with no resolvable script variant, rather than failing cryptically at
submit time.

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
  `gpu` flag. At `sim submit`/`sim run`, cactup checks that the chosen queue is
  in the built config's `compatible-queues`; a mismatch is a **hard error**
  (overridable with `--force` for experts). This is what prevents submitting a
  GPU binary to a CPU queue (or vice versa). `-g/--gpu` defaults from the
  *binary's* `gpu` flag, cross-checked against the queue's `gpu` flag.
- **SubmitScript and RunScript variants** are each associated with one or more
  **queues** (the `[variants.*]` tables in §4.2). At submit/run time cactup
  picks the variant mapped to the chosen `-q/--queue`; if the queue has no
  explicit mapping, the `default` variant is used. (A machine with a single
  variant may just name it `default`.) The submit-script and run-script variant
  maps are independent of each other but must each cover every queue (validated
  at load — §4.2).
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
    meta.toml                    # grouped tables (§4.2); single "local" queue
    discover.py                  # is_machine(): FQDN == melete05.cct.lsu.edu
    optionlists/default.toml     # ported from mel5.cfg; [cactup] gpu=false + [options]
    runscripts/default.sh        # @NUM_PROCS@→@TASKS@, @NUM_THREADS@→@CPUS_PER_TASK@
    submitscripts/default.sh     # @SIMFACTORY@→@CACTUP@, +--installation, PID-wait chaining
```

It exercises every load-bearing new mechanism: discovery function, grouped
`meta.toml`, single-queue/single-variant resolution, the optionlist
TOML→native render (§7.8), the renamed topology variables (§6.3), and the
compute-node re-invocation locator (§8.3.1). Because mel5 has no scheduler, its
`submit` backgrounds the script and echoes a PID; chaining is emulated by
waiting on that PID in plain bash (no `.py` variant needed).

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
omits hardware keys), cactup fills the missing values at load time from the OS:

| Var | Linux | macOS |
|-----|-------|-------|
| `ppn` / `num-threads` | `nproc` (or `/proc/cpuinfo`) | `sysctl -n hw.ncpu` |
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
- **Autodetects and writes concrete hardware** (`ppn`, `num-threads`, `memory`
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

---

## 5. Knobs

Machine-global default values, replacing simfactory's `defs.local.ini [default]`
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

**Storage:** knobs live in the **global database** (`~/.cactup/database.json`),
keyed by machine name, because they are machine-global and not tied to any one
installation. This is consistent with D4 (the global DB holds global cactup
state).

```jsonc
// database.json (excerpt)
"knobs": {
  "mike": { "allocation": "hpc_xxx", "mail": "me@lsu.edu", "mail-type": "all", "queue": "checkpt" }
}
```

### 5.1 Value precedence

For any value that can come from several places (mirrors `simfactory-docs.txt`
§6.2, adapted):

1. Explicit CLI flag (e.g. `-q/--queue`, `-a/--allocation`) — highest.
2. Knob for the current machine.
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

Per D7, cactup implements **literal `@NAME@` replacement only**. There is no
expression evaluation, no ternary sugar, no `@ENV()@`. Any MDB script that
needs conditional logic (e.g. simfactory's
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

**`env-setup` handling differs by template kind.** `env-setup` from
`meta.toml` (module loads, etc.):

- **`.sh` templates:** cactup **auto-prepends** `env-setup` to the generated
  submit/run script before execution, exactly as simfactory did
  (`simfactory-docs.txt` §6.4, §18 `ExecuteCommand`).
- **`.py` variants:** cactup does **not** auto-prepend — the `.py` author has full
  control over the emitted script and is responsible for placing `env-setup` where
  they want it. cactup makes the value available to the script: it is bound as the
  global **`ENV_SETUP`** (string) — and, like every variable, is also a literal
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
> removed (compute from `@TASKS@`/`@CPUS_PER_TASK@`/`@PPN@` if needed),
> `@SIMFACTORY@`→`@CACTUP@`.

**Topology (canonical — one variable per §8.5 flag):**
`NODES` (`-n`), `TASKS` (`-T`, total MPI ranks), `TASKS_PER_NODE` (`-t`/tpn),
`CPUS_PER_TASK` (`-c`/cpus), `GPU` (`-g`; `1`/`0`), `ALLOCATION` (`-a`),
`QUEUE` (`-q`), `MAIL` (`-m`), `MAIL_TYPE` (`-M`), `JOB_NAME` (`-j`),
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
`MACHINE`, `HOSTNAME`, `USER`, `EMAIL`, `EXECHOST`, `JOB_ID`, `CHAINED_JOB_ID`,
`FROM_RESTART_COMMAND`.

**Machine-derived** (read from `meta.toml`, available to scripts but not topology
flags): `PPN` (logical cores/node), `MEMORY` (per-node MB), `CPUFREQ` (GHz),
`NUM_SMT` (default 1; §8.5 assumption), `ENV_SETUP` (the machine `env-setup`
block; auto-prepended for `.sh`, author-emitted for `.py` — §6.1).

**Build-time only** (optionlists/build): `MAKEJOBS`, `DEBUGGER`, `RUNDEBUG`.

Dropped (no longer produced): the simfactory proc-layout names above, the bare
`SUBMITSCRIPT` machine key (variants replace it), and every remote/archive
variable.

---

## 7. Config subsystem

Local to the active installation. Replaces `sim-build` (`simfactory-docs.txt`
§16).

### 7.1 Commands

```
cactup config build <name> [-f] [--thornlist P] [--variant V] [flags…]
cactup build <name> …              # alias
cactup config show [<name>]
cactup config use <name>
cactup config delete <name>
```

- `build`: builds (or rebuilds with `-f`) config `<name>` in the active
  installation. `--thornlist` defaults to
  `<Cactus root>/thornlists/einsteintoolkit.th`. `--variant` selects the
  optionlist variant (required iff the machine has >1 optionlist variant — §4.4).
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
line change in the optionlist forces a full rebuild).

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
portable across machines).

**Rebuild-decision snapshot.** At build time cactup also copies the chosen
**source optionlist TOML** verbatim to `configs/<name>/cactup-optionlist.toml`.
The rebuild decision (§7.8 rule 5) diffs the freshly-selected source TOML against
this stored copy; *any* difference triggers a full realclean + reconfigure +
rebuild. This is the source-of-truth for "did the optionlist change?" — the
rendered native file is never diffed (it exists only for the Cactus build system
to consume, §7.8).

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

[options]                        # rendered to native NAME = value lines
VERSION = "2024-06-01"           # first emitted line; a change forces full rebuild
CPP = "cpp"
CC  = "gcc"
CFLAGS = "-O2 -g @SOME_TEMPLATED_VALUE@"
# … one key per native option …
```

**Render rules (deterministic; the rendered native file is fed to Cactus only —
it is never diffed for the rebuild decision):**

1. Only the `[options]` table is rendered. `[cactup]` is stripped (snapshotted
   into the config metadata, §7.4).
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
cactup sim submit [-f] [--overwrite] [--force-queue] <sim> <TOPOLOGY…>
cactup sim submit [-f] [--overwrite] [--force-queue] <sim> <parfile> [--config C] <TOPOLOGY…>   # implicit create
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
- `makeActive()` — create the `output-NNNN-active` symlink (§9.2) **only for the
  restart that will run first**; chained pre-submissions (below) are created
  un-activated.
- Run the machine `submit` command; parse the job id with `submit-pattern`; store
  it in the restart metadata (`job-id`; `-1` ⇒ failed/unknown).
- **Auto-recover + auto-chaining** (the simplified model — §8.8): if the
  simulation already has restarts, the new restart recovers from the latest one
  automatically (unless `--no-recover`). If requested walltime > the effective
  walltime ceiling (§4.2), cactup transparently pre-submits
  `ceil(walltime / ceiling)` chained restarts, each scheduler-dependent on the
  prior job id and each recovering from the prior restart. There is no
  user-facing chaining command; the dependency flag is emitted by the
  submit-script variant (a `.py` variant, since this is conditional — §6).

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
    --restart-id=@RESTART_ID@ @FROM_RESTART_COMMAND@
```

`--sim-dir` (the absolute simulation directory) + `--restart-id` fully identify
the restart (`@SIMULATION_DIR@/output-<RESTART_ID>`) with no registry lookup;
`--installation` and `--machine` supply the alias and machine name without
touching the global DB. `cactup sim run --restart-id` reads everything else
(config, executable path, recovery source, topology) from that restart's on-disk
`.cactup/` metadata (§9.3) — satisfying the D11 rule that the compute-node path
does not touch the global DB or the registry. Scheduler stdout/stderr filenames
come from the template via `@STDOUT_FILE@` / `@STDERR_FILE@`, not from cactup
code (preserving simfactory's design point).

#### 8.3.2 Active-symlink handoff across a chain

The exactly-one-active invariant (§9.2) is maintained across a pre-submitted
chain as follows. At submit time, only the first restart of the chain is made
active; restarts `K>first` are created with full metadata (including
`from-restart-id = K-1` and `chained-job-id`) but **no** `-active` symlink. When
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
cactup sim run [-f] [--overwrite] [--force-queue] <sim> <TOPOLOGY…> [--debug]
cactup sim run [-f] [--overwrite] [--force-queue] <sim> <parfile> [--config C] <TOPOLOGY…>
```

Port of `run()` / `userRun` / `submitRun` (`simfactory-docs.txt` §14.3): runs
interactively, bypassing the queue. With `--restart-id` it runs that restart —
this is the **compute-node path** the submit script takes (§8.3.1), and in that
mode it accepts `--installation`/`--sim-dir`/`--machine` to locate the
simulation without the global DB or the registry, performs the chain handoff
(§8.3.2), and recovers checkpoints (§8.8). Throughout its run it holds the
per-restart liveness marker and periodically touches the heartbeat file (§9.3)
so the reaper (§8.3) never mistakes it for dead. Without `--restart-id`, it
builds a fresh restart, makes it active, forks, and tees child stdout/stderr to
`<SimName>.out` / `<SimName>.err` while echoing to the terminal. `--debug`
launches under the debugger (`@RUNDEBUG@`/`@DEBUGGER@`).

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

Derivation (produces the canonical §6.3 names directly — no legacy aliases):
- `CPUS_PER_TASK` = `--cpus` (default 1).
- `TASKS_PER_NODE` = `--tpn` if given, else `floor(PPN / CPUS_PER_TASK)`, min 1
  (fill the node).
- `TASKS` = `--tasks` if given, else `NODES * TASKS_PER_NODE`.
- `GPU` = `1` if `--gpu` or the chosen queue's `gpu = true`, else `0`,
  cross-checked against the built binary's `gpu` flag (§4.4 / D12).
- A script that needs "total cores" or "cores requested" computes them from
  `TASKS`, `CPUS_PER_TASK`, `NODES`, and `PPN` — cactup no longer pre-derives
  `PROCS`/`PROCS_REQUESTED`/`PPN_USED`.

**ASSUMPTION:** SMT (`NUM_SMT`) defaults to the machine `num-smt` (1) and is not
a topology flag in v1; expose later if needed.

### 8.6 `stop` / `clean`

Ports of `simfactory-docs.txt` §14.8 / §14.5:
- `cactup sim stop <sim>`: if `TERMINATE` exists and not `-f`, write `1` into it
  (graceful termination trigger created by the running Cactus job), then finish.
  Otherwise run the machine `stop` command (forced) and finish.
- `cactup sim clean <sim>`: deactivate the active restart (remove the
  `-active` symlink), tighten `TERMINATE` perms, delete half-written checkpoints
  (`*.chkpt.tmp.it_*.*`), and run the Formaline tarball **hard-link dedup** across
  prior restarts. Dedup semantics are preserved verbatim from simfactory
  (`simfactory-docs.txt` §14.5): for each `*.tar.gz` ≥ 1000 bytes, scan prior
  `output-%04d` dirs (descending) for a file **of the same name whose contents
  are byte-identical** (`filecmp`-style full comparison — *not* name-only), and if
  found replace this copy with a hard link to it via a `.tmp` rename. Because the
  match requires identical content, dedup can never alias two different tarballs.
  All of this is preserved because it shapes on-disk output (§9).

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

### 8.8 Checkpoint recovery & the simplified restart CLI (D4)

On-disk recovery is the verbatim port of `PrepareCheckpointing`
(`simfactory-docs.txt` §14.6): locate checkpoint files (`*chkpt.it_*`) in the
recovery-source restart's working dir and **hard-link** them into the current
restart (fall back to copy), rewriting the path prefix. No checkpoints ⇒
recovery is a no-op (fresh start).

**Recovery-source selection (best effort, no silent data loss).** The
recovery source is **not** blindly "the highest-numbered restart" — a restart
that crashed before writing any checkpoint has none, and picking it would
silently cold-start and discard the last good run. Instead cactup scans restarts
**backward from the latest** and picks the newest one that actually contains
recoverable `*chkpt.it_*` files. In the common linear case this is unambiguous
and silent. If the history is **divergent** (e.g. multiple checkpoint-bearing
branches after a manual `--restart-id` recovery, or the latest restart's
checkpoints look older than an earlier restart's), cactup **prompts** the user to
choose the source and records the choice as `from-restart-id`. The prompt only
happens on the **interactive login-node path**; the **compute-node path**
(`sim run --restart-id`, §8.3.1) **never prompts**.

**Compute-node re-scan for pre-submitted chains.** The compute-node path does
**not** blindly trust the `from-restart-id` stored at submit time as a *hard*
recovery target. In a pre-submitted chain, restart `K`'s `from-restart-id` is
fixed to `K-1` at submit time (§8.3.2) — but segment `K-1` may have died before
writing any checkpoint, so recovering from it verbatim would silently cold-start
and discard the last good run (exactly the failure the login-node scan avoids). To
close that gap, at recovery time the compute-node run performs the **same backward
scan**: it starts from its stored `from-restart-id` and, if that restart has no
`*chkpt.it_*` files, walks further back to the newest restart that does. This
preserves the no-silent-data-loss guarantee for chains while staying **fully
deterministic and prompt-free** — the scan is data-driven, and the stored
`from-restart-id` is a *starting hint*, not a hard target. (A queued/chained job
therefore recovers from the newest checkpoint-bearing restart at or before its
hint, regardless of how many predecessors crashed checkpoint-less.)

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

1. **Auto-recover on resubmit.** `cactup sim submit <sim>` (or `sim run`) on a
   simulation that already has restarts automatically allocates the next
   `output-%04d` and recovers from the newest checkpoint-bearing restart (the
   best-effort selection above). The first submit of a fresh simulation starts
   from `output-0000` with no recovery. Recovery is silent when there's nothing
   to recover — there is no "is this a fresh run or a continuation?" decision for
   the user to make.
2. **Automatic walltime chaining.** If the requested total `--wall-time` (in
   seconds, §8.5) exceeds the effective per-job walltime ceiling (§4.2), cactup
   transparently pre-submits `ceil(total-walltime / ceiling)` chained restarts,
   each scheduler-dependent on the previous job, each *reserving* the ceiling as
   its scheduler wall, and each recovering from the prior restart's checkpoints
   (§8.3). The user asks for "100 hours" on a 24-hour-max queue; cactup figures
   out it needs 5 chained jobs. No manual chaining command exists. (Whether each
   segment actually checkpoints before its wall so the next can resume is the
   parfile author's responsibility — cactup only reserves the wall; see above.)

   **No-op tail jobs after early completion (accepted).** The chain is a fixed
   set of dependency-gated jobs sized from `--wall-time`. If the simulation reaches
   its termination condition partway through (say segment 2 of 5), the remaining
   pre-submitted segments still launch when their scheduler dependency clears; each
   recovers the final checkpoint, sees the run already terminated, and exits
   quickly. cactup does **not** cancel the tail — doing so would require it to
   inspect Cactus termination state, which it deliberately does not model
   (see the walltime paragraph above). These tail jobs are harmless (no data
   change) but do consume a queue slot and startup each. **Sizing the chain
   sensibly by passing a reasonable `--wall-time` is the simulation runner's
   responsibility**; over-requesting simply yields a few no-op tail jobs.

**Minimal manual knobs (escape hatches only):**

| Flag | On | Effect |
|------|-----|--------|
| `--no-recover` | submit, run | Start the new restart cold even though prior restarts exist (ignore checkpoints). |
| `--restart-id N` | submit, run | Operate on/restart from a specific `output-%04d` instead of the latest. Primarily for the submit-script's own re-invocation on the compute node and for recovering a non-latest segment. |
| `--checkpt-buffer W` | submit, run | Override the checkpoint buffer (default `max(reserved-walltime/24, 10 min)`) that sets the `@CHECKPOINT_WALLTIME@` hint = hard wall − buffer (§8.8 above). Affects only the exposed hint variables; cactup still reserves the full hard wall and injects nothing into Cactus. |

That's the entire manual surface. simfactory's `--from-restart-id` collapses
into `--restart-id` (the recovery source is the newest checkpoint-bearing restart
at or before the named one — or before the latest, if none is named — per the
best-effort scan above); `--recover`/its inverse collapse into the default +
`--no-recover`;
and there is no user-facing chaining flag at all. Internally, cactup still
tracks `from-restart-id` and `chained-job-id` in `restart.toml` (§9.3) — they're
just computed, not asked for.

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
— plus cactup's `alias`. (No `testsuite`/`select-tests` keys: test-suite support
is dropped for v1 — D3.) `restart.toml` carries `schema` plus the submit/run keys
(`nodes`, `tasks`, `tpn`, `cpus`, `queue`, `allocation`, `walltime`,
`checkpt-buffer`, `job-id`, `chained-job-id`, `checkpointing`, `from-restart-id`,
the last observed `status`, and the creation/marking timestamps that simfactory
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

## 11. Test suites (dropped for v1 — D3)

Cactus test-suite support is **deferred out of this spec** and will be designed
later. cactup v1 has no `--testsuite`/`--select-tests` flags, no test-selection
metadata, and no `make <configuration>-testsuite` path. simfactory's testsuite
machinery (`copyTestsuiteData`, the `output-0000-active → .` self-link special
case, `simfactory-docs.txt` §13.7) is intentionally out of scope here; when
test-suite support is added it will get its own design section.

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
| Substitution | `@NAME@` + `@(expr)@` + `@ENV()@` | **`@NAME@` only**; `.py` for logic (JSON-on-stdin convention, §6.1) |
| cactup binary var | `@SIMFACTORY@` | `@CACTUP@` |
| Machine detection | `aliaspattern` regex on hostname | `discover.py`; result cached in DB as a single `detected-machine` string (not per-hostname — §4.3) |
| Per-installation state | n/a | `<installation home>/.cactup/installation.toml` (active config, sim-home) + `simulations.toml` (name→dir registry) |
| Sim root key | machine `basedir` | machine `simulation-home` (optional; falls back to `~/.cactup/simulations`) — §8.1 |
| Install root key | machine `sourcebasedir` (source base; also sync/disambiguation) | machine `install-home` (optional default install prefix; falls back to `~/.cactup/cacti`; `--install-prefix` overrides) — §4.2 |
| Locking | none (per-tree) | `link()`-based (NFS-safe) global-DB lock + per-sim lock + per-config build lock (D11, §2.3) |

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
11. Test-suite support dropped for v1 (D3, §11) — to be designed later.
12. Schema/version handling: newer binaries **must read older `schema`**
    (backward-compatible reads), refusing only a `schema` newer than understood;
    the `schema` integer is bumped only on a breaking change and in-place
    up-migration is deferred (§2.1, §9.3).

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
| 12 optionlists/run/submit | variants (§4.4, §6.2) |
| 13 on-disk layout | **preserved** (§9) |
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
