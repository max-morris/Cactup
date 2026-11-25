# Plan: versioned, forward-migratable cactup formats

*Status: approved by Max on 2026-11-10, with the decisions below. Line numbers are as of `d9f4a94` (Cactup) and `e94c1ca` (cactup-ecosystem-common).*

## Decisions (Max, 2026-11-10)

The open questions of §7, answered:
1. **Shape pin:** enforced from the first phase.
2. **Breaking changes to `frozen_read` records:** allowed, but each must ship its `vN+1 → vN` down-step (test 5 enforces it).
3. **`latest.json`:** no version field; the path is the version.
4. **Cactarium API:** the server keeps answering older-generation clients in their generation alongside the new one, until a deliberate cutoff. (A rule for the Cactarium repo; cactup's side is unchanged.)
5. **Spec home:** a new §2.5 "On-disk formats and versioning".
6. **The restart finish-write bug** (`sim/start.rs`): fixed separately, before P2a, not as part of this plan.

Added to P1: `database.json` is a JSON object whose key order the write must keep (version key first, the rest as they are). Cactup's `serde_json` has no `preserve_order` today, so its `Map` sorts keys; enable the `preserve_order` feature (it uses `indexmap`, already a dependency, so no new crate).

## 0. Summary

Every file cactup writes and later reads back gets three things: its **own** version constant, a **read path that looks at the version first** (with a chain of upgrades from older versions), and a **write path that keeps keys it does not know**. Queued jobs run an older, frozen binary against files that a newer login binary also rewrites. For the records those jobs read, a rewrite **never raises the file's version** unless its content needs the newer one. One helper module does all of this. Fixture files, one per shipped shape, keep the old versions loading.

This plan **bumps no version**. Every record stays at its current version: 1, or 2 for the config record. So the binaries already deployed keep reading everything the new binary writes.

## 1. What the code does today (checked)

| # | Finding | Where |
|---|---|---|
| 1 | Eight records share one constant, `database::SCHEMA = 1`. | `src/database.rs:31` |
| 2 | Five readers parse the whole typed struct **before** checking the version. A newer, incompatible file therefore gives a parse error instead of "please upgrade". | `database.rs:417-419` and `:512-513` (the lock-free read also retries 3×, then takes the lock, then fails to parse); `sim/restart.rs:194-197`; `build/attempt.rs:379-383`; `testsuite/mod.rs:199-202`; `build/mod.rs:225-229` (`from_table`, then the check) |
| 3 | Only `installation::read_toml` reads `schema` first. `Simulation::open` uses it, so `simulation.toml` already gets this right. | `installation.rs:565-586`, `sim/mod.rs:156` |
| 4 | **Latent bug:** every `default_schema()` returns the *current* constant. On the first bump, a file written before the `schema` key existed would claim the new version. `ConfigMeta` gets away with it only because `upgrade_config_record` runs first and treats a missing key as 1. | `database.rs:34`, `installation.rs:28`, `sim/mod.rs:32`, `sim/restart.rs:22`, `build/attempt.rs:28`, `testsuite/mod.rs:29`, `build/mod.rs:67` vs `:270` |
| 5 | No record uses `deny_unknown_fields`. A read-modify-write by any binary drops keys it does not know. All `skip_serializing_if` values are `Option::is_none`, `is_empty` or `Not::not`, each paired with `#[serde(default)]`. | grep of the 8 record modules |
| 6 | How the version gets written varies: `Db::update` restamps `SCHEMA` (`database.rs:543`), `ConfigMeta::store` always writes `CONFIG_SCHEMA` with a **non-atomic** `fs::write` (`build/mod.rs:260-264`), and the rest write back whatever is in the struct field. | |
| 7 | `FetchState.schema` is required (it has no default) but nobody checks it. | `fetch/mod.rs:620-689` |
| 8 | Nobody checks `FrozenStack.format`. It arrives only over the API, behind the `api-generation` header, and cactup never stores it on disk. | shared `stack.rs:220-232`, `cactarium.rs:150-158,182` |
| 9 | `build.toml` renames `variant`→`flavor` without a version bump. This is a "legacy normalization". | `build/attempt.rs:372-377` |
| 10 | The spec's on-disk policy is in **§2.1** ("Version guard & on-disk schema policy", spec l.147), **§9.3** ("Schema versioning", l.4560), **§7.4** ("Schema 2…", l.2566) and **§14 item 12** (l.5503). It is **not in §2.4**, which covers only interrupts, progress and §-hygiene. Even so, CLAUDE.md l.3 and spec l.6611 and l.6647 cite "§2.4: no backward compatibility". Spec l.6646 also says the store's entry format is `v2`, but `store::FORMAT` is 4. | `design/cactup-simfactory-design-new.md` |
| 11 | Who reads what on the compute nodes. **`sim run --sim-dir`** reads `simulation.toml`, then `restart.toml`, and writes `restart.toml` at start and finish (`sim/start.rs:828-839`, `:939-969`). **`build run`** reads `build.toml` and `cactup-config.toml` fresh at `execute` (`build/mod.rs:2521`), writes both, and its compiler wrapper reads `cc/config.toml`. **`test run`** reads and writes `test.toml`. `fetch-state.toml` is login-only (`fetch::plan` at `fetch/mod.rs:1028`; `sim/export.rs`). | |
| 12 | Already looked at, no problem found: the restart's final write at `sim/start.rs:964-969` stores a copy loaded before the run, which could overwrite known keys a login command wrote during the run. This plan keeps *unknown* keys only. See open question 6. | |

## 2. Principles (decided)

### 2.1 One version per format
Each format gets a `static` descriptor in its own module (§4 lists them) instead of the shared `SCHEMA`.
- Local records keep the key name **`schema`**. Renaming the key would itself break things.
- The shared crate's interchange formats keep **`format`**.
- A missing key always reads as **1**.

### 2.2 One shared helper: `src/format.rs`
It is generic over the raw document type, implemented for `toml::Table` and `serde_json::Map` (the latter for `database.json`). Sketch, where the exact shape is load-bearing:

```rust
pub struct Format<D> {
    pub name: &'static str,          // "restart record", for messages
    pub key: &'static str,           // "schema"
    pub current: u32,
    pub upgrades: &'static [fn(&mut D) -> Res<()>],      // [i]: v(i+1) -> v(i+2); len == current-1
    pub downgrades: &'static [fn(&mut D) -> Result<(), CannotExpress>], // [i]: v(i+2) -> v(i+1), from down_floor up
    pub down_floor: u32,             // oldest version the down-chain must reach (CONFIG: 2, others: 1)
    pub frozen_read: bool,           // read by a queued job's frozen binary
    pub legacy: Option<fn(&mut D)>,  // same-version normalizations shipped before this (build.toml `variant`); closed list
}
pub fn read<T: DeserializeOwned, D>(path: &Path, f: &Format<D>) -> Res<Option<T>>;   // None = file absent
pub fn parse<T: DeserializeOwned, D>(text: &str, path: &Path, f: &Format<D>) -> Res<T>;
pub fn write<T: Serialize + DeserializeOwned, D>(path: &Path, value: &T, f: &Format<D>) -> Res<()>;
```

**Reading:**
1. Parse the raw document.
2. Read the version key. If it is missing, use 1. If it is not a positive integer, error.
3. If the version is newer than `current`, return the upgrade error, which names the file. One message for every format: `'<path>' was written by a newer cactup (<name> schema N; this cactup, <VERSION>, reads up to M); upgrade cactup`. No §-refs. This verdict is final, including in `Db::read_unlocked`. Only a raw-parse failure is retried.
4. Strip the key, run `legacy`, then run the upgrades from the file's version up to `current`.
5. Do the typed parse.

**Never rewrite a file just because it was read.** This keeps today's config-record rule, because rewriting on read races concurrent writers.

**Writing:** writes are atomic (temp file, fsync, rename), using the body of today's `installation::write_toml`.
1. **Re-read the target on disk first.** If it is absent or can't be parsed, write with no grafts. If its version is newer than `current`, **refuse**: never overwrite a newer file.
2. Serialize the value at `current`.
3. Choose the version to write: the oldest version that is (a) no older than the file's version on disk and (b) reachable by the down-chain. A down-step returns `CannotExpress` when the content needs the newer version. For a new file this is ".sim's rule": the oldest version that holds the content. With no down-steps, which is every format today, it simply writes `current`.
4. **Put back the unknown keys** (§2.3).
5. Insert the version key **first** and write.

**The version is a property of the file, not of the value.** The `schema: u32` fields are removed from all structs, and the helper owns the key. The config record embedded in `build.toml` keeps its own version through `#[serde(deserialize_with = "ConfigMeta::deserialize_upgrading", serialize_with = "ConfigMeta::serialize_versioned")]`.

### 2.3 Unknown keys: put them back on write, decided from disk
Two alternatives were rejected:
- **A flattened `extra` table** in each struct. `BuildMeta` and `ConfigMeta` already flatten `OptionlistSource`. Flatten buffers everything through serde's `Content`, which has known traps (TOML datetimes, for example). It also keeps the keys *as they were when loaded*, so the compute binary's write 48 hours later would put back stale values.
- **`serde_ignored`.** It reports nothing at all under `flatten`.

**Chosen:** at write time, take the disk table after `legacy` and the upgrades, and compare it with that same table typed-parsed and serialized again. A key present on disk but missing after that round trip is one this binary does not model. Copy such keys into the new document only where the key is absent there, recursing into **maps only**. Never put keys back inside array elements: if elements are reordered, a key would land on the wrong element. Dropping a key wrongly is acceptable; attaching one to the wrong place is not.

This works through `flatten` and `deserialize_with` (`tolerant_runs_on`, the embedded config record). It needs no struct changes and no new crate. Because it reads the disk at write time, the newest addition wins. `Db::update` already holds the lock and has just read the file, so it reuses that read.

Two invariants the reviewer must check:
- **`skip_serializing_if` may only skip the field's deserialize default.** Otherwise a skipped value that was written to disk would come back as "unknown". This holds today.
- **The version key never takes part in the round-trip comparison.**

### 2.4 When a change needs a bump
| Change | Bump? | Mechanism |
|---|---|---|
| New optional key: when absent it means the old behavior, and an older binary can safely ignore it | **No** | Older readers ignore it; older writers put it back (§2.3). |
| New key that an older binary would get *wrong* by ignoring it (must be understood) | **Yes** | Its down-step returns `CannotExpress` only when the key is used, so only files using the feature get the new version. This is the `.sim` `unpack` pattern. |
| Rename, retype, removal, new meaning, stricter validation, new required key | **Yes** | `vN→vN+1` upgrade plus a new fixture. Records with `frozen_read` **must** also ship the `vN+1→vN` down-step (§3). |
| A change inside a cache | — | Fresh namespace (store `FORMAT`/`KEY_LABEL`, "cactup input tree 1"). |

### 2.5 Fixtures and the shape pin
`fixtures/formats/<format>/` holds one file per shipped shape. Once committed, a fixture is **never edited or deleted**; the one exception is `current.{toml,json}`. The v1 fixtures must come from the code **before** the change: real files from the dev `~/.cactup` with paths and names scrubbed, plus a version with no `schema` key where one existed. They must not be written by the new serializer.

Tests, all generic over each `Format` in `format.rs`:
1. **Every fixture loads.** Writing it back and reading it again gives the same value. The version written equals the expected `max(on-disk, needed)`.
2. **Shape pin**, in the spirit of `key::tests::the_key_of_fixed_parts_is_pinned`.
   - Each format builds an `exhaustive_sample()`. It is a struct literal **without `..`**, so adding a field fails to compile. Every `Option` is `Some` and every collection is non-empty.
   - The test compares the sample's serialized form with `current.*` as a parsed table, not bytes, so a `toml` crate upgrade can't break it.
   - The failure message says what to do. For an additive change, copy the old `current` to `v<N>-<k>.*`, then re-pin. Otherwise, bump the version, add the upgrade (and the down-step if `frozen_read`), and add `v<N+1>`.
3. **Newer files are refused** before the typed parse. The test body is invalid for the typed struct and has `schema = current+1`. The error must name the file, and the file must be unchanged afterward.
4. **Unknown keys survive.** Unknown top-level and nested keys survive a write. A known key that the writer cleared, such as `finished` set to `None`, stays cleared.
5. **`frozen_read` formats have every down-step:** `downgrades.len() == current - down_floor`.

## 3. Compatibility matrix: who can be older than whom

The three sources of an older reader:
- **(a)** A queued job's frozen binary (`@CACTUP@`), for months.
- **(b)** Another login binary sharing `~/.cactup`: autoupdate off, an explicit `bin/cactup-<old>` path, or a home directory shared between clusters.
- **(c)** Deployed clients of a server or update site.

| Format | Writers | Older readers | What newer→older guarantees | What older→newer guarantees |
|---|---|---|---|---|
| `database.json`, `installation.toml`, `simulations.toml`, `tests.toml`, `fetch-state.toml`, `credentials` | login binaries only (never compute; D11) | (b) | Same version: older binaries ignore new keys and put them back on write. A bump: older binaries refuse with "upgrade". That is acceptable because `cactup update` fixes it, but avoid bumping. | Upgrade chain. |
| `simulation.toml` | `sim create` and `set_knobs` (login) | (a): the compute binary of every queued restart | Sticky version + put-back. A newer binary writes nothing that must be understood into an existing file. | Chain. |
| `restart.toml` | submit binary S (submit, compute start/finish) and newer login L (stop, clean, reaper, `job-stopped`) | (a) S | Sticky + put-back. | Chain. |
| `build.toml` (+ embedded config record) | S (submit, compute), L (stop) | (a) S | Sticky + put-back. The embedded record: see P2b. | Chain (+ `legacy`). |
| `test.toml` | S, L | (a) S | Sticky + put-back. | Chain. |
| `cactup-config.toml` | S at build finish; L on build, the up-to-date refresh (`build/mod.rs:2031`) and the thornlist retarget | (a) S at `execute` start | Sticky + put-back. The 1→2 bump is grandfathered (`down_floor = 2`), which keeps spec §7.4's accepted "narrow window". | Chain [1→2]. |
| `<sim>/.cactup/cfg/cactup-config.toml` | `sim create`, once | login readers (`sim export`) | Written once, at `current`. | Same chain as the config record. |
| `.sim` | cactup export | (c) every cactup import, the server | Oldest version that holds the content (done). | Reads 1..=`SIM_FORMAT`. |
| `cactarium*.toml` | authors | the server | Exact-match today. When it bumps, read 1..=N **forever**, because tags are immutable. | — |
| API JSON, `FrozenStack` | the server | (c) all clients | The header generation: clients refuse a newer one. **The server should answer in the generation the client sent** (out of scope; other repo). | — |
| `latest.json` | CI (`ci/assemble-site.sh:95`) | (c) every deployed binary and the installer | **Never break it.** A refusal here would leave old binaries unable to update themselves forever. Keys may only be added (already read leniently, `update.rs:206`). A breaking change publishes a **new path** next to the old one: the path is the version. | — |

**The hard case.** A newer L rewriting `restart.toml` with a bumped version would make the older frozen S of a queued job refuse its own record. This plan prevents that with three rules:
1. A rewrite never raises a file's version unless its content needs it.
2. A breaking bump of a `frozen_read` record must ship its down-step, which test 5 enforces. With the down-step, L writes old files back at their old version.
3. L must not write anything that must be understood into an **existing** `frozen_read` record. A new restart created by L is run by L, so it may carry the new version.

**If the rules are broken anyway**, S fails cleanly:
- At load, before it runs anything (compute reads `restart.toml` first), with the upgrade message.
- If it happens mid-run, at the finish write: S refuses to overwrite the newer file, so the outcome is not recorded, but the file is never corrupted.

Rejected alternative: forbidding breaking changes to these records outright. It is simpler but stricter. See open question 2.

## 4. Decisions per format

Priority: **H** = shared across binaries and checks the version after the parse today; **M** = gap in a single binary; **L** = cosmetic or consistency.

| Format | New descriptor | Migrations | Write policy | Unknown keys | Pri | Phase |
|---|---|---|---|---|---|---|
| `~/.cactup/database.json` | `database::DATABASE` (JSON), 1 | none | `current` (restamp kept) | put back (top level, `installations.<alias>`, `detected`) | H | P1 |
| `<inst>/.cactup/installation.toml` | `installation::INSTALLATION`, 1 | none | `current` | put back; `[[added]]` elements are not covered (array) | M | P2a |
| `simulations.toml` / `tests.toml` | `SIM_REGISTRY` / `TEST_REGISTRY`, 1 | none | `current` | put back (per entry, a map) | M | P2a |
| `<sim>/.cactup/simulation.toml` | `sim::SIMULATION`, 1, `frozen_read` | none | sticky | put back | H | P2a |
| `output-NNNN/.cactup/restart.toml` | `sim::restart::RESTART`, 1, `frozen_read` | none | sticky | put back (incl. `[universe]`; `vars`/`knobs` are open maps) | H | P2a |
| `<test>/.cactup/test.toml` | `testsuite::TEST`, 1, `frozen_read` | none | sticky | put back | H | P2a |
| `.cactup-builds/NNNN/build.toml` | `build::attempt::BUILD`, 1, `frozen_read`, `legacy` = `variant` rename | none | sticky | put back (incl. `[timestamps]`, embedded `[config-meta]`) | H | P2b |
| `configs/<c>/cactup-config.toml` | `build::CONFIG`, 2, `frozen_read`, `down_floor` 2 | [1→2] = `upgrade_config_record` (`build/mod.rs:267`) | sticky; store becomes atomic | put back (incl. `[runs-on]` via `tolerant_runs_on`) | H | P2b |
| sim's frozen cfg copy | reuses `CONFIG` | same | written once (`sim/mod.rs:439`) | n/a | M | P2b |
| `<inst>/.cactup/fetch-state.toml` | `fetch::FETCH_STATE`, 1 | none | `current` (**always write the key**: deployed binaries require it) | put back (per repo) | M | P3 |
| `~/.cactup/credentials` | `cactarium::CREDENTIALS`, 1 (new key; missing = 1) | none | `current`; keep the 0600 temp-file creation | put back | L | P3 |
| `FrozenStack` (API) | shared `STACK_FORMAT = 1` | none | (server) | stays `deny_unknown_fields` | L | P3 |
| `cactarium.toml` / `-stack.toml` | `MANIFEST_FORMAT` stays 1 | — | — | strict | L | P3 (read 1..=N) |
| `.sim` | `SIM_FORMAT` = 2 (done) | — | oldest that holds the content | strict | — | P3 (fixtures only) |
| `latest.json` | **no key: the path is the version** | — | additive only | ignored (already) | L | P3 (doc + test) |
| `<attempt>/cc/config.toml` | **stays unversioned.** It names the exact binary that reads it (`BuildConf.cactup`, `objcache/mod.rs:229-231`), a stronger stamp than an integer. Newer `cactup cache` reads it with `.ok()`. | | | | — | — |
| `cc/events.jsonl` | stays unversioned: kept per build, never across versions; the reader counts lines it can't parse (spec §18.6) | | | | — | — |
| objcache memos (`cc/compilers/*.toml`, `shell-*.toml`) | stays unversioned: a cache. If parsing fails, the compiler is examined again (`identity.rs:528`, `lookup.rs:100`). | | | | — | — |
| Build-cache store | **Nothing needed.** `v<FORMAT>/` namespace (`store.rs:308`); each entry header's format and label are checked on read (`store.rs:725-728`). A bump means a fresh namespace. | | | | — | — |
| inputs store | stays as is: content-addressed; `tree_key` is labeled "cactup input tree 1" (`inputs.rs:181`) | | | | — | — |
| lock stamps | stay positional. Rule: **only ever append lines**. `parse_stamp` ignores extra lines (`lock.rs:500-506`). | | | | — | — |
| `log.txt`, CRL thornlists | stay as is: simfactory's format (appended text) and an external GetComponents format (`!CRL_VERSION` belongs to CRL) | | | | — | — |
| `update-check`, `*.retired` | stay as is: markers whose mtime is the content | | | | — | — |
| MDB, overlays, sync stamp | **out of scope**: the MDB-generation mechanism stays the only one (CLAUDE.md "MDB generation") | | | | — | — |

## 5. Phases

Order: P1 → P2a → P2b → P3 → P4, with P5 optional. P2b follows P2a because both touch the import of `installation::write_toml`. P4 can land right after P2b.

Every phase must follow these:
- Never `cargo fmt`. Format only the lines you touch, by hand.
- American spelling.
- §-refs go in comments only: none in `bail!` strings or fixtures that users see.
- The interrupt and progress contracts are unchanged. These are small-file operations, nothing long-running.
- No new crates.

### P1: the helper, plus `database.json` as the pilot (**Opus**)
- **Files:** new `src/format.rs`; `src/main.rs` (module declaration); `src/database.rs` (delete `SCHEMA` after P2b, `default_schema`, `check_schema`, `Database.schema`; `read_from`, `read_unlocked`, `update` and `persist` go through the helper); `fixtures/formats/database/{v1-noschema.json, v1.json, current.json}`. The database tests that build `schema: SCHEMA` change.
- **Tests:** the five generic tests of §2.5 on a toy TOML struct and on `Database`.
  - Lock-free read: a newer DB gives the upgrade error **without** falling back to the locked read. JSON that can't be parsed is still retried.
  - Unknown keys under `installations.<alias>` survive `Db::update`.
- **Why Opus:** this phase fixes the API every later phase copies. It is generic over TOML and JSON, has the round-trip semantics of §2.3, and touches NFS read-retry semantics and the atomic write.
- **Reviewer checks:**
  - The upgrade verdict is final, and nothing is written on refusal.
  - The version key is stripped before the typed parse and excluded from put-back.
  - Keys are put back into maps only, and only where absent.
  - A key the writer cleared stays cleared.
  - The version key comes first in the output.
  - The write is still temp-file + fsync + rename in the same directory.
  - Upgrade messages carry no §-refs.
  - `Db::update` does not read the file twice.
- **Real-binary check:** `cargo build --release`; `export CACTUP_HOME=$(mktemp -d)`; copy in `~/.cactup/database.json`. Use only commands that don't open the MDB (`cactup knob …`).
  1. `sha256sum` the DB, run the read-only `cactup knob`, check the hash is unchanged.
  2. Hand-add `"future": {"x": 1}` at top level and `"future-field": 2` inside an installation. Run `cactup knob queue workq`. Both keys must survive.
  3. Set `"schema": 2`. `cactup knob` must print the upgrade message naming the file, exit non-zero, and leave the hash unchanged.
  4. **Two binaries:** build `master` in a `git worktree`. Its binary must read and rewrite the new binary's DB, and the new binary must read the result.

### P2a: TOML records onto the helper (**Sonnet**)
- **Files:**
  - `src/installation.rs`: descriptors; delete `read_toml` and `default_schema`; move `write_toml` into `format.rs`, leaving a re-export until P2b.
  - `src/sim/mod.rs` (`SIMULATION`, `:156`, `:237`); `src/sim/restart.rs` (`RESTART`, `load` `:186-206`, `store` `:208`); `src/testsuite/mod.rs` (`TEST`, `open` `:193-210`, `store_meta` `:247`).
  - Sites that construct these structs: `sim/start.rs:539,778`, `sim/manage.rs:1335`, `testsuite/run.rs:412`.
  - Fixtures for all six formats.
  - **New `tests/formats.rs`**: an integration test through `CARGO_BIN_EXE_cactup`, like `tests/objcache.rs`.
- **Tests:** the generic five per format. In `tests/formats.rs`, a hand-built sim directory (`simulation.toml`, `.cactup/par/x.par`, `output-0000/.cactup/restart.toml`, `run-script` = `#!/bin/sh\ntrue`) runs the **compute path** `cactup sim run x --sim-dir D --restart-id 0`:
  - An unknown key in `restart.toml` and in `simulation.toml` survives the start and finish writes.
  - With `schema = 2`, the binary fails before running anything, with the upgrade message, and leaves the file byte-identical.
- **Why Sonnet:** the API is fixed by P1. Each record gets the same mechanical change, and the compiler finds every construction site.
- **Reviewer checks:**
  - No `toml::from_str` remains on these records, and no `schema:` fields.
  - `frozen_read` is true exactly for simulation, restart and test.
  - "Absent file" behavior is unchanged: D10 detection in `Simulation::open`, `Restart::load`'s "has no metadata", and registries defaulting to empty.
  - The v1 fixtures came from the pre-change code or real files, not the new serializer.
  - `exhaustive_sample`s really are exhaustive.
  - Fixture files contain no §-refs.
- **Real-binary check:** beyond `tests/formats.rs`, run `cactup sim list` / `cactup test list` / `sim show <name>` against the real `~/.cactup`. Compare `sha256sum` of every `.cactup/*.toml` before and after: reads must not rewrite.

### P2b: the config record and `build.toml` (**Opus**)
- **Files:**
  - `src/build/mod.rs`: `CONFIG` with upgrades = [`upgrade_config_record`] and `down_floor` 2; `load_path` `:214-235`; `from_table` and `deserialize_upgrading` share the chain; new `serialize_versioned`; `store` becomes the atomic helper write; drop `ConfigMeta.schema` and its `default_schema`.
  - `src/build/attempt.rs`: `BUILD` with `legacy` = the `variant` rename; `read_meta` `:366-392`; `store_meta` and `store_meta_keeping_job` `:411-434`.
  - `src/sim/mod.rs:439` (the frozen copy).
  - Construction sites: `build/mod.rs:2210,2240`, `attempt.rs:472`, `commands/build.rs:2568`, `commands/config.rs:476`, `sim/start.rs:1408`, `testsuite/run.rs:968`.
  - Remove `database::SCHEMA`.
- **Tests:**
  - These schema-1 config fixtures load both from `cactup-config.toml` and embedded in `build.toml`: `variant`, flat `gpu`/`compatible-queues`, and the version with no key.
  - An attempt whose top level has `variant`.
  - An unknown key inside `[runs-on]` and inside `[config-meta]` survives `store_meta_keeping_job`.
  - Switching `flavor`→`optionlist` never leaves both keys after the put-back.
  - The config write is atomic: no torn read under a concurrent write.
- **Why Opus:** this is the only existing migration and a versioned record nested inside another. The put-back passes through `flatten` and `deserialize_with`, and spec §7.4's window must stay exactly as narrow as documented.
- **Reviewer checks:**
  - The embedded record is written at `CONFIG.current`, the same as today. Note in a comment that stickiness for the embedded copy must be designed when CONFIG next bumps.
  - `build::execute` still reads the config record fresh.
  - No path now writes the config record that didn't before.
  - `tolerant_runs_on` is kept.
- **Real-binary check:** with the two-binary worktree setup, on a scratch config, check both directions: `master` builds (or hand-copies a `build.toml` it wrote) and the new binary reads it; the new binary writes and `master` reads. Also check `cactup config list` / `config show` against the real installation with the hash comparison.

### P3: the remaining formats (**Sonnet**)
- **Files:**
  - `src/fetch/mod.rs:620-689` (`FETCH_STATE`).
  - `src/cactarium.rs:300-349` (`CREDENTIALS`; keep `NamedTempFile`, which is 0600).
  - `src/cactarium.rs`, the stack response: check `format <= STACK_FORMAT` before the typed parse, giving an upgrade message.
  - `src/update.rs:206` (doc comment: the path is the version; never break it) plus a test that a manifest with extra keys still parses.
  - Shared crate: `lib.rs` gets `STACK_FORMAT = 1`. `manifest.rs:160,172` read `1..=MANIFEST_FORMAT` (a no-op today). Add `tests/fixtures/` with `sim/v1.sim`, `sim/v2.sim`, `manifest/v1.toml`, `stack/v1.toml`, plus a test that loads them all.
  - Then bump `Cargo.toml:38`'s `rev` in Cactup.
- **Why Sonnet:** small and independent, following patterns already set.
- **Reviewer checks:**
  - The fetch-state writer always emits `schema` (deployed binaries require it).
  - Credentials stay owner-only, and tokens never reach knobs or logs.
  - `latest.json` reading stays lenient.
  - The shared crate keeps no system-library dependencies (it is linked into the static MUSL binary).
- **Real-binary check:** `CACTUP_HOME` scratch: run `cactup login` against a dummy token, check the file mode is 0600 and has `schema = 1`, add an unknown key, log in again, and check the key survives. Run `cactup installation refetch` on a small install to rewrite fetch-state.

### P4: CLAUDE.md and the spec (**Sonnet drafts, Opus reviews**)
- **Files:**
  - `CLAUDE.md`: replace "On-disk formats" with §6 below, and fix l.3's "§2.4" pointer.
  - Spec: add a new **§2.5 "On-disk formats and versioning"**, holding the principles of §2 and the matrix of §3. Cut §2.1's "Version guard" paragraph (l.147-161), §9.3 "Schema versioning" (l.4560-4574), §7.4's last sentences (l.2584-2587) and §14 item 12 (l.5503) down to pointers. Fix l.6611 and l.6647 to cite §2.5 / the cache rule, and l.6646's `v2`.
- **Reviewer checks:** the docs match the code (descriptor names, which formats are `frozen_read`). §-refs stay out of strings users see. American spelling.

### P5 (optional, later): CI `formats-compat` job (**Opus**)
It mirrors `ci/mdb-generation-guard.sh`. Check out the last deployed build in a worktree (the `build` in `latest.json`), copy in HEAD's `fixtures/formats/`, and run *its* fixture-load test. That catches an "additive" change an older binary can't actually read. It only means something once P1–P2 are deployed.

## 6. Proposed CLAUDE.md text (replaces "On-disk formats: no backward compatibility (being softened)")

> ## On-disk formats: versioned, read old, refuse newer, keep unknown keys (spec §2.5)
>
> Every file cactup writes and reads back goes through `format::read`/`format::write` (`src/format.rs`), described by its own `Format` (`database::DATABASE`, `sim::restart::RESTART`, `build::CONFIG`, …). Local records carry `schema`, the shared crate's interchange formats `format`; a missing key is 1. Structs never hold the version — it belongs to the file.
>
> - **Read: version first.** A newer file is refused with an upgrade message naming it, before any typed parse, and is never overwritten. An older one runs the format's `vN → vN+1` upgrades over the raw table, then parses. A read never rewrites the file. Upgrades are kept forever.
> - **Additive needs no bump**: a new optional key whose absence means the old behavior and that an older binary may safely ignore. Records are not `deny_unknown_fields`, and every write puts back the keys this binary does not model from the file on disk (maps only), so an older binary's rewrite keeps a newer one's additions. `skip_serializing_if` may only skip the field's default.
> - **Anything else bumps**: rename, retype, removal, new meaning, stricter validation, a key an older binary would get wrong by ignoring it. Add the upgrade and a fixture.
> - **Frozen readers.** A queued job runs the binary that submitted it for months (`@CACTUP@`) against files a newer login binary rewrites. Formats such a job reads (`simulation.toml`, `restart.toml`, `build.toml`, `test.toml`, `cactup-config.toml`) are `frozen_read`: a write uses the oldest version, no older than the file on disk, that holds the content; a bump ships its `vN+1 → vN` down-step (a test enforces it); and nothing an older binary must understand is written into an existing record of one.
> - **Fixtures.** `fixtures/formats/<format>/` keeps a file per shipped shape, never edited once committed (except `current.*`); every one must load. When the shape pin fails: additive → copy `current` to a new numbered fixture and re-pin; otherwise bump.
> - **Interchange formats** have deployed readers we cannot update. `.sim` writes the oldest format that holds its content (`SimFile::required_format`); manifests are read `1..=N` forever (tags are immutable); the API is gated by `api-generation`; `latest.json` never breaks — a breaking change publishes a new path beside it.
> - **Deliberately unversioned:** caches (the build cache's `store::FORMAT`/`KEY_LABEL` namespaces, memos, `events.jsonl`, the inputs store) — a change is a fresh namespace or a miss; lock stamps (append lines only; readers ignore extras); `log.txt` (simfactory's); CRL thornlists (external); `cc/config.toml` (it names the exact binary that reads it). The MDB has its own generation (below).
>
> The config record's schema 1→2 upgrade is the first entry of `build::CONFIG`'s chain; its `down_floor` is 2 (that bump predates down-steps — spec §7.4's accepted window). We have no real users yet: a cleanup that would force people to re-create state can still be proposed, but ask first.

## 7. Open questions for Max

1. **Shape-pin strictness.** Turn the pin test on now, so every new field means copying a fixture? Or only at the first public release? I recommend now: the cost is one file copy.
2. **Breaking changes to `frozen_read` records.** Require a down-step (recommended)? Or forbid such changes outright, which is simpler and stricter?
3. **`latest.json`.** Is "the path is the version" OK, with no `format` field?
4. **Cactarium API.** Adopt "the server answers in the generation the client asked for" as a rule in the server repo? Today a server bump locks out every deployed client until it updates.
5. **Where the spec says it.** A new §2.5 (recommended), or expand §2.1?
6. **Out of scope, but noticed:** the compute run's finish write (`sim/start.rs:964-969`) stores a copy of `restart.toml` loaded before the run, so it can overwrite *known* keys a login command wrote during the run. Should that get its own fix (re-read under the sim lock before the write)?

### Critical files for implementation
- /home/max/src/Cactup/src/format.rs (new)
- /home/max/src/Cactup/src/database.rs
- /home/max/src/Cactup/src/build/mod.rs
- /home/max/src/Cactup/src/build/attempt.rs
- /home/max/src/Cactup/src/sim/restart.rs
- /home/max/src/Cactup/src/installation.rs
- /home/max/src/cactup-ecosystem-common/src/generation.rs (the model)
