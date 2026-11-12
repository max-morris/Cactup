# Plan: migrate user-MDB overlays forward automatically

*Status: approved by Max on 2026-11-11, with the decisions below. Line numbers are as of `master` at `01c2577` (the gen-5 PR shifts `src/mdb/mod.rs` and `src/mdb/meta.rs` a little but does not touch the gate). Generation 5 is taken from `origin/fix/allocation-directive` (`74bde9b`); the work branches from it as `feature/overlay-migration` and rebases onto master once that PR merges.*

## Decisions (Max, 2026-11-11)

The open questions of §9, answered:
1. **`toml_edit`:** yes.
2. **`.py` build-submit `ENV_SETUP` (2→3):** refuse. In general, telling the user to migrate by hand is acceptable wherever there is no good automatic migration.
3. **Backups:** kept forever.
4. **Generation 1:** not migrated. The 1→2 step refuses (a step that always stops with the hand recipe), so P3 and the frozen generation-1 parser are dropped. §4's "1→2 in detail" is kept only as a record of what the hand recipe covers.
5. **`cactup machine migrate`:** no.

Added: **the refusal is rewritten.** Today's error for an outdated overlay is too long and clunky to act on. Every refusal (`OverlayGenerationError` for an overlay older or newer than the binary, and `OverlayMigrationError`) must be short and actionable, and a little more informative than today: which machine and file, its generation and the binary's, why it could not be brought forward automatically (the step that stopped and what it found), and the next step on a line of its own, with a link to that generation's own section of the guide rather than the whole page. `brief()` stays one line. See §2.5.

## 0. Summary

When a login command loads a user-MDB machine whose `[cactup] mdb-generation` is older than the binary's, cactup brings it forward **on disk, once**. It takes a `LinkLock`, copies the overlay to `~/.cactup/machine-backups/<name>/generation-<N>/`, runs one migration step per generation (N→N+1, …, →current) over the files in memory, checks that the result loads and behaves as before, and writes it back atomically, `meta.toml` last. It prints one line. Every later load, by any process, sees a current overlay and says nothing.

A step **preserves behavior or refuses.** It does what is mechanical. Where the new binary would act differently on this overlay, and cactup can tell, it checks the overlay. If the difference can only show up later as a loud error, the step goes ahead and adds a note. If the machine could quietly do something different, the step stops: it says exactly what to change by hand and keeps whatever earlier steps finished. Optional recommendations (gen 3 provisioning split, gen 4 `inputs-home`, gen 5 allocation lines) are never applied. Scripts are never edited.

For generations 1→5: 3→4 and 4→5 only raise the number. 2→3 renames three keys and refuses only in one narrow `.py` build-submit case. 1→2 always refuses, pointing at the hand recipe (Max's decision 4).

Every generation bump ships its step from now on. A test requires one step and one fixture snapshot per older generation, and every snapshot must migrate and load.

## 1. What the code does today (checked)

| # | Finding | Where |
|---|---|---|
| 1 | The gate parses the raw `meta.toml` as a `toml::Table` **before** the typed parse, reads `[cactup] mdb-generation`, and returns `OverlayGenerationError` when it differs from `MDB_GENERATION` (older: "migrate it as described in {guide}"; newer: "run `cactup update`"). A missing key gives a warning, printed once per machine per process, and loads the machine as current. A value that is not an integer passes through to the typed parse. | `src/mdb/mod.rs:327-434`, called from `Machine::load` `:445-454` (user layer only) |
| 2 | The only ways into an overlay's `meta.toml` are `Mdb::load` → `Machine::load` and `Mdb::check_overlay` (the `--from-existing` base check). | `src/mdb/mod.rs:189-194`, `:211-217`; `src/commands/machine.rs:579` |
| 3 | Compute-node re-invocations return before anything calls `Mdb::open` (D11). | `src/mdb/mod.rs:113-118`; `src/main.rs:64-75` |
| 4 | `machine list` loads every machine. It shows an `OverlayGenerationError` as `[unusable: …]` (`brief()`), and fails on any other error. | `src/commands/machine.rs:297-336`, `src/mdb/mod.rs:346-358` |
| 5 | `machine show`, `resolve`, `ensure_local_machine` and the aggregate `cactup show` all load through `load_checked`, which also prints the origin-staleness warning. That warning compares the **system** source with `origin.hash`, never the overlay, so migration does not change it. | `src/commands/machine.rs:268-294`, `:342-376` |
| 6 | `machine create` copies the base directory verbatim (`copy_dir`, so optionlists and scripts keep their comments). It then round-trips `meta.toml` through `toml::Table`, which drops its comments ("accepted"), stamps `mdb-generation` and `origin`, and writes with a plain `fs::write`. | `src/commands/machine.rs:586`, `:602`, `:643`, `:654` |
| 7 | `Meta` is `Deserialize` only (no `Serialize`) and closed (`deny_unknown_fields` on every table). `CactupMeta` has exactly `origin` and `mdb-generation`. Any new `[cactup]` key would be breaking for an older binary of the same generation. | `src/mdb/meta.rs:96-127`, `:868-889` |
| 8 | The optionlist header has been closed since gen 2. Gen 1's was **open**: unknown header keys were silently ignored. Header edits never trigger a rebuild, because `build_relevant` drops `[cactup]` before comparing. | `src/mdb/optionlist.rs:57-60`, `:499-509`; `38e118a:src/mdb/optionlist.rs:17-19` |
| 9 | Generation-1 script choice, per partition (test/normal), filtered by build universe: the first entry listing the queue; otherwise the partition default (its sole entry, or the `default = true` one). A test command with no test entries **borrowed the normal partition**. Today's `route` has no fallback and never borrows, and validation requires the normal submit and run scripts to cover every queue. | `38e118a:src/mdb/meta.rs:690-770`; `src/mdb/meta.rs:920-940`, `:1217-1300` |
| 10 | Generation 1 offered only the flavors listed in `[variants.optionlist] variants`. Today every `optionlists/*.toml` is a flavor. | `38e118a:src/mdb/mod.rs:534`; `src/mdb/mod.rs:542-561` |
| 11 | `effective_env(universe, phase)` gives exactly the block a gen-3+ binary builds (env-setup, then the phase's setup, with universe keys replacing machine keys one by one). With no provisioning keys, it equals what gen 2 built from the old key names. | `src/mdb/meta.rs:1044-1066` |
| 12 | At gen 2, only qbd had `.py` build-submit scripts, and qbd declares no universes. So the 2→3 `ENV_SETUP` change affects no overlay copied from the in-repo MDB. | `ec7ec7a:mdb/qbd/` |
| 13 | Auto-update re-runs the user's command in the new build. So the first command after an upgrade loads the overlays with the new binary: that is where the advisor hits the refusal today, and where migration would run. | `src/main.rs:141-151` |
| 14 | `toml` 1.1.2 is the only TOML crate. There is no `toml_edit` in `Cargo.lock`, so there is no comment-preserving editor today. | `Cargo.toml:30` |
| 15 | `installation::write_toml` is atomic, but it writes through `NamedTempFile`, so files come out mode 0600. On the format branch, `format::write_atomic` fixes that (0666 less the umask). | `src/installation.rs:595-607`; `origin/feature/format-versioning:src/format.rs:443` |
| 16 | A unit test requires a `## Generation k` heading for every k ≤ N. Nothing requires a migration. | `src/build_info.rs:46-57` |
| 17 | `machines()` lists only directories that contain a `meta.toml`. A hidden lock file or a backup directory elsewhere never shows up as a machine. | `src/mdb/mod.rs:144-169` |

## 2. Principles

### 2.1 On disk, once, not in memory on every load (recommended)
In-memory migration would leave the files untouched, so an older binary sharing `~/.cactup` would keep working. It loses on everything else:
- Optionlists are read in many places (`load_header` from `flavors`/`select_optionlist`/`show_flavors`, `Optionlist::load` in build, the live header read in queue choice). Every one of them would need the upgrade.
- Gen 1's unlisted optionlists would need a filter inside `flavors()`.
- The user would keep editing an old-generation file while every doc describes the current one, and the overlay would never converge.
- Notes would repeat on every command, unless a marker file is added, which is a write anyway.

On-disk migration costs the older-binary case only. That binary sees "needs MDB generation 5; run `cactup update`", which `cactup update` fixes, and the old files are in the backup. This is the same trade the format-versioning plan accepted for login-only records (its §3, row (b)).

### 2.2 Preserve behavior or refuse
Each step returns the edited files plus a list of findings. A finding is one of:
- **silent**: mechanical and behavior-preserving (a rename, a number).
- **note**: done, and something now acts differently, but only in a way that fails loudly when it is reached. Example: `cactup test` on a machine with no test scripts now errors instead of borrowing a normal one. Printed once and written to the backup's `NOTES.txt`.
- **refuse**: no behavior-preserving rewrite exists, or one is ambiguous. The step stops. Earlier finished steps are written, stamped with the last generation reached. The error names the file, the key, and the hand fix, and points to that generation's entry in `GENERATIONS.md`. Once the user fixes it by hand and sets `mdb-generation`, the next load continues the chain from there.

A step never applies an optional recommendation, never edits a script, and never writes a key the target generation's *first* binary lacks.

### 2.3 Edit the files, keep their comments
Add `toml_edit`, a pure-Rust crate from the same family as `toml` and safe for static MUSL. Use the release built on `toml_parser`/`toml_writer` 1.x, so `Cargo.lock` gains no second `toml_datetime`. The only local copy is 0.22.27, which is too old for that, so this needs a fetch. Edits are surgical: rename a key in place (remove, then `insert_formatted` with the old key's decoration, then restore the order), move keys into a new subtable, set an integer. Inline tables and dotted keys go through `TableLike`. This matters most for optionlists, which `machine create` copies verbatim with all their comments (finding 6).
Fallback if Max declines the crate: round-trip through `toml::Table`, say in the notice that comments were not kept, and rely on the backup.

### 2.4 The transaction
In `Machine::load` (user layer) and `Mdb::check_overlay`, when the unlocked raw read finds `found < want`, call `mdb::migrate::bring_forward(name, dir, guide)`:
1. `LinkLock::acquire_wait(machines/.<name>.migrate.lock, 60 s)`, which is interrupt-aware. If the first try misses, print one line: `Waiting for another cactup updating user-MDB machine "x"…`.
2. Re-read `meta.toml`. If it is now current (another process got there first), release and return. If it is newer, return `OverlayGenerationError`.
3. Back up to `~/.cactup/machine-backups/<name>/generation-<found>/`, a full copy (minus `__pycache__`). If that directory exists *without* a `.complete` marker, it comes from an interrupted run of this same migration: reuse it. If it exists *with* one, use `generation-<found>.2`, `.3`, …. Backups are never pruned.
4. Load the overlay into memory: the `meta.toml` document, every `optionlists/*.toml` document, and the script file list (read only, for checks). Run steps `found..want`.
5. If all steps ran: parse and validate the result with today's code (`Meta` + `validate`, `validate_files`, every flavor's header), and run the step checks that need the typed result (1→2 routing, 2→3 `ENV_SETUP`). If this fails, refuse with the error and **write nothing**. Never leave unverified files behind; the backup and the original stay as they were.
6. Check `gix::interrupt::is_triggered()`, then commit. Write each changed optionlist with the same temp+fsync+rename in the directory, keeping the replaced file's mode (not 0600; see finding 15). Remove the gen-1 unlisted optionlists, which the backup holds. Write `meta.toml` **last**: its stamp is the commit point. Every file-level edit is idempotent on an already-edited file, so an interrupted commit is finished by the next load.
7. Write `NOTES.txt` (cactup version, the steps run, the findings), then `.complete`. Release the lock. Print the notice.

### 2.5 What the user sees (only the process that migrates)
```
Updated user-MDB machine "qbd" from MDB generation 2 to 5; the old files are in '~/.cactup/machine-backups/qbd/generation-2'.
  note: <one line per note finding>
  Optional changes for generations 3, 4, 5 are described in <guide>.
```
The third line appears only for generations whose entry lists optional changes.

**Refusals (Max: today's is too long and clunky to act on).** One typed error, `OverlayGenerationError`, now carries why: older and could not be brought forward (with the step that stopped and its finding), or newer than the binary. `OverlayMigrationError` may be the same type with a reason field; the implementer picks. Shape (wording is the implementer's, length and content are not):

```
user-MDB machine "qbd" is MDB generation 2, and this cactup reads 5. It could not be updated automatically: its build-submit script "build.py" reads ENV_SETUP, and the build and submit environments differ (universe "apptainer" adds `module load gcc`).
  To fix by hand: follow "Generation 3" in <guide>, then set `mdb-generation = 3` in ~/.cactup/machines/qbd/meta.toml; cactup does the rest.
  Or start over (loses your edits): cactup machine delete qbd && cactup machine create qbd
```
```
user-MDB machine "old" is MDB generation 1, which cactup does not update automatically (this cactup reads 5).
  To fix by hand: follow "Generation 2" in <guide>, then set `mdb-generation = 2` in ~/.cactup/machines/old/meta.toml; cactup does the rest.
  Or start over (loses your edits): cactup machine delete old && cactup machine create old
```
```
user-MDB machine "x" is MDB generation 6, and this cactup reads 5: run `cactup update`.
```
(plus, when a backup exists, a line naming the generation-5 copy, §5.)

Rules: the headline says which machine, both generations, and why in one sentence; each next step on its own line; the guide points at the one generation section to follow, by its heading (and by URL anchor where the guide is the documentation page and the anchor exists; check it); the path shown is the file to edit; "start over" says it loses the user's edits. `brief()` (for `machine list`) stays one line: `MDB generation 2, could not be updated automatically; see `cactup machine show qbd`` / `needs MDB generation 6; run `cactup update``. No §-refs in any of these strings. Tests pin the shape (line count, the pieces named here), not every word.

### 2.6 Do not reuse `format.rs`'s `Format<D>` (decided here)
Its model does not fit:
- Its version key sits at top level, is stripped before the parse, and is reinserted first. Ours is nested (`[cactup] mdb-generation`) and stays where it is.
- It writes by serializing a typed value, but `Meta` has no `Serialize`, and that would drop comments.
- Its machinery exists for open schemas and frozen readers: putting back unknown keys, sticky versions, down-steps. The MDB is closed, overlays have no frozen readers (D11), and a generation covers a whole directory, not one file.
- Its rule "never rewrite on read" is the opposite of §2.1.
- The branch is an unmerged draft (PR #31).

Reuse only its shape: `STEPS[i]` migrates generation i+1 to i+2, plus a length test. Reuse the atomic text write too: copy `write_atomic`'s body (mode-preserving) into `migrate.rs` now, and fold it onto `format::write_atomic` when PR #31 merges (a one-line follow-up). The work **branches from `master` once the gen-5 PR has merged**. If it has not, branch from `origin/fix/allocation-directive` and rebase. It does **not** branch from `feature/format-versioning`.

## 3. Who touches an overlay

| Actor | What happens |
|---|---|
| New binary, overlay older | Migrates (§2.4). |
| Parallel new-binary processes, possibly on several hosts over NFS | One holds the `LinkLock` and migrates. The others wait, re-read, find the overlay current, and stay silent. A reader that reads `meta.toml` without the lock either sees the old stamp, and so goes for the lock, or sees the new stamp, which is written after everything else. |
| Older binary sharing `~/.cactup` (autoupdate off, an explicit `bin/cactup-<old>`) | Refused with today's "needs MDB generation N; run `cactup update`". Files untouched. A brief race is accepted: an older process that read the old `meta.toml` just before the commit could then read an already-migrated optionlist and fail once. |
| Queued job (compute binary) | Never reads overlays (D11). Everything it needs was frozen at submit, so rewriting the overlay cannot affect it. As a backstop, `bring_forward` refuses to run if `main` marked the process as a compute-node run (a new `static COMPUTE_NODE: AtomicBool`, set where `compute_node_path` is computed). In that case it behaves exactly as today. |
| An editor with `meta.toml` open | Not lockable. The editor warns that the file changed. If the user saves the old text back, the next load migrates it again (idempotent). Accepted. |

## 4. Decisions per generation step

| Step | Automatic (silent unless noted) | Note | Refuse |
|---|---|---|---|
| 4→5 | Set `mdb-generation = 5`. Scripts untouched: `@ALLOCATION@`/`ALLOCATION` still work, and their new meaning comes from the binary, whatever the overlay says. | — | — |
| 3→4 | Set `mdb-generation = 4`. `inputs-home` is not added: without it, inputs go to `~/.cactup/inputs`, the gen-4 default. | — | — |
| 2→3 | In `[environment]` and every `[universes.*]`: `env-build-setup`→`build-env-setup`, `env-submit-setup`→`submit-env-setup`, `env-run-setup`→`run-env-setup`. No provisioning split: setups run where they ran before, so leaving the lines in place keeps gen-2 behavior. | A setup matching a short fixed pattern list (`mkdir`, `curl`, `wget`, `pip install`, `conda`/`micromamba` create or install, `git clone`, `activate`): "may belong in `*-provision`", pointing to step 2 of generation 3. | Both the old and the new name in one table. Or the **`.py` `ENV_SETUP` case**: a `[buildsubmitscripts]` route whose `.py` file mentions `ENV_SETUP`, where `effective_env(U_b, Submit)` differs from `effective_env(U_s, Submit)` for some candidate build universe U_b (every flavor header's `universe`, `[build].universe`, the host) and the route's submit universe U_s (the route's `universe`, else a declared `host`). A refusal names both universes and the differing lines. |
| 1→2 | — | — | Always: "generation 1 overlays are not migrated automatically", with the hand recipe. |

**1→2 in detail (not implemented, decision 4; kept as a record of what the hand recipe covers).** Gen-1 entries are read by a small frozen copy of gen 1's parser (array or inline table; `default-universe` as the one string key), kept in `migrate.rs`.
1. Rename `[variants.submitscript]`/`runscript`/`buildsubmitscript` to `[submitscripts]`/`[runscripts]`/`[buildsubmitscripts]`.
2. `default-universe = U`: delete it, and add `universe = U` to every entry without its own (an array entry becomes the table form).
3. **Routing.** For each kind, partition (normal and test), universe context (host plus declared universes) and queue that context reaches, run gen 1's `select` on the original (finding 9). Where it chose an entry *by fallback*, add the queue to that entry's `queues`. Then drop `default` from every entry. After the typed parse (§2.4 step 5), compare every (kind, partition, context, queue) choice, as (entry name, effective universe), between gen-1 `select` and today's `route`.
   - **Silent** where they agree. This includes the qbd/saturn case where a sole GPU test script served CPU queues by fallback: preserved as it was. See open question 4.
   - **Note** where gen 1 *borrowed* a normal script for a test command and today errors: "`cactup test` used the normal runscript "x" on queue q; it now needs a test runscript".
   - **Refuse** on any other mismatch, or when validation fails. Typical causes: a queue served by no normal script, because gen 1 had several entries, no default, and errored at run time too; or a fallback that now lands on two compatible entries. The refusal names the queue and the kind, and says to list the queue in one entry.
4. Build-submit entries: drop `tasks` (a build job never read it); delete a `test = true` entry from the route table, keeping its file (builds never chose one). Each is a note.
5. Delete `[variants.optionlist]`. Remove from `optionlists/` every `.toml` not listed in `variants`; the backup holds them. Generation 1 never offered them, and leaving them would add flavors, or even change the implicit one through a stray `default = true`. Note each one.
6. In every remaining optionlist: move `[cactup] gpu` and `compatible-queues` into a new `[cactup.runs-on]` as `gpu` and `queues`, placed right after `[cactup]`. Remove any other header key the gen-2 header does not know. Generation 1 ignored those keys, so this is a note naming each one: a typo such as `defualt` is the likely case.
7. Set `mdb-generation = 2`.

Optionlists outside the overlay (`build --optionlist FILE`) are out of scope. Today's `reject_generation_1_header` already says where their keys went.

## 5. Commands and edge cases

- **`machine list`** (Q8): loading migrates each stale overlay in turn. A notice goes to stderr just before that machine's line. An `OverlayMigrationError` or a lock timeout is shown as `[unusable: <brief>]` like a generation error and does not fail the list. Other load errors behave as today.
- **`machine show`**: migrates, then prints the summary. When `machine-backups/<name>/` exists, it adds one line: `earlier generations kept in: <dir>`.
- **`machine create --from-existing <stale overlay>`**: `check_overlay` migrates the base before copying (today it refuses).
- **`cactup show`** (aggregate, "read-only") migrates too. That writes the user's machine directory but never prompts and never touches the DB; acceptable, noted in a comment there.
- **Newer overlay** (Q5): refused as today, never downgraded. A down-migration would be lossy, and the generation mechanism promises nothing backward. New this time: if `machine-backups/<name>/generation-<want>/` exists, the error adds "a copy as it was for generation <want>, from before a newer cactup brought it forward, is in '<dir>'". Only binaries that have this feature can say so.
- **No recorded generation** (Q6): still assumed current, with the per-process warning, and **not** migrated or stamped. Guessing a start generation and rewriting is exactly the silent change §2.2 forbids. `machine create` has stamped overlays since generation 1 (`38e118a`), so only hand-made overlays reach this case. Improvement: if the raw table carries a marker of an older generation (a `variants` table → 1; an `env-*-setup` key → 2), the warning says so instead: "looks like generation N; add `mdb-generation = N` under [cactup] and cactup will bring it forward".
- **Compute node** (Q7): cannot happen. See §3 and finding 3, plus the backstop.
- **Read-only overlay** (permissions): the commit fails, and the error is the I/O failure plus today's hand-migration pointer. There is no in-memory fallback.

## 6. Enforcement: every bump ships its migration

- `migrate::STEPS: &[Step]`, where `STEPS[i]` migrates generation i+1 to i+2. Test `every_generation_has_a_migration`: `STEPS.len() == MDB_GENERATION - 1` (next to `every_generation_is_documented`, `src/build_info.rs:46`).
- `fixtures/mdb-overlays/gen-<N>/<machine>/` for every N < MDB_GENERATION, never edited once committed. Test `every_snapshot_migrates`: each directory must exist, and each overlay in it must migrate to current, load, pass its semantic checks, and do nothing the second time.
- `ci/overlay-fixtures.sh N`, run on the **pre-bump** tree: in a scratch `CACTUP_HOME`, `cargo run -- --hostname fixture.example.org machine create <m> --from-existing <m> --silent --no-discover` for a fixed machine list. Then scrub the autodetected hardware and the home paths to fixed values.

## 7. Phases

Every phase must: never run `cargo fmt` (format only touched lines, by hand); use American spelling; keep §-refs in comments only; respect the interrupt contract (lock wait polls, commit after the last check); add no progress bar (milliseconds of small-file work, plus the one waiting line); never run cactup against the real `~/.cactup` (use a scratch `CACTUP_HOME` under `$TMPDIR`, and `cactup knob autoupdate off` there). An **Opus review** follows each phase.

### P0: fixtures (**Sonnet**)
- Write `ci/overlay-fixtures.sh`. Build the binaries of the four bump commits `38e118a` (gen 1), `ec7ec7a` (2), `17ef80f` (3), `13928d4` (4) in worktrees under `$TMPDIR`, and with each, create overlays from `generic`, `qbc.loni.org`, `qbd`, `db1.hpc.lsu.edu` and `mel5` (plus `rostam` from gen 2 on). Commit them under `fixtures/mdb-overlays/gen-{1..4}/`. These are real `machine create` output, exactly what the advisor has.
- Add hand-written edge fixtures under `fixtures/mdb-overlays/edge/` with an `expect.toml` per case (result generation, findings): gen-1 `default-universe`; unlisted optionlist with `default = true`; no test scripts; queue without coverage (must refuse); open-header typo; build-submit `tasks`; gen-2 universes + `.py` build-submit `ENV_SETUP` (must refuse); old+new key both present (must refuse); comments everywhere.
- **Reviewer checks:** the fixtures came from the old binaries, not hand edits; the scrub left nothing personal.

### P1: framework, 3→4, 4→5, integration (**Opus**)
- **Files:** new `src/mdb/migrate.rs` (steps, frozen readers, transaction, `OverlayMigrationError`, notice); `src/mdb/mod.rs` (call sites `:211`, `:452`, newer-overlay backup hint, marker hint for unmarked overlays); `src/commands/machine.rs` (`list` `:309`, `show` backup line, create base); `src/main.rs` (`COMPUTE_NODE`); `Cargo.toml` (`toml_edit`).
- **Tests:** the enforcement tests (§6), with the gen-3 and gen-4 snapshots passing. Idempotence (a second load writes nothing: hashes unchanged). A newer overlay is refused and left byte-identical. An interrupted commit (a `cfg(test)` fault after the first file) is finished by the next load, reusing the backup. Concurrency: a new `tests/overlay_migration.rs` runs eight `cactup machine show <m>` processes through `CARGO_BIN_EXE_cactup` on one scratch `CACTUP_HOME`; all must succeed, exactly one may print "Updated", and the result must equal a single-process run, with one backup.
- **Real-binary check:** two binaries on one scratch home, a generation-4 one built from `74bde9b^` in a worktree and the new one. The old binary creates the overlay. The new one migrates it, and `machine show`/`--flavors` match the old output apart from the generation line. Then the old one refuses with "run `cactup update`", files unchanged (`sha256sum`), and its `machine list` annotates only that machine.
- **Reviewer checks:** `meta.toml` is written last; temp files keep the replaced file's mode; nothing is written on refusal before the first finished step; the lock is released on every path; no §-refs or British spellings in strings; no `libc`; `Cargo.lock` gains one crate and no duplicate `toml_*`.

### P2: 2→3 (**Opus**)
- **Files:** `src/mdb/migrate.rs`.
- **Tests:** the gen-2 snapshots; the rename inside inline-table universes and dotted keys; the edge fixtures for both refusals; the provisioning hint fires on rostam-like setups and stays silent on `generic`. `effective_env` for every (universe, phase) after migration must equal, string for string, the gen-2 block built from the original under the old names.
- **Reviewer checks:** the `.py` check uses the same universe choice as `commands/build.rs` (the test at `:1833`); no provisioning key is ever written.

### P3: 1→2 (**dropped**, decision 4: the step only refuses, and lands in P1)
- **Files:** `src/mdb/migrate.rs` (frozen gen-1 `ScriptVariants` and `select`, ported from `38e118a:src/mdb/meta.rs:400-770`).
- **Tests:** the gen-1 snapshots (qbd's sole-script test fallback must be preserved); every 1→2 edge fixture; the routing comparison over every (kind, partition, context, queue); comments in optionlists survive the header move; unlisted optionlists end up only in the backup.
- **Reviewer checks:** first verify against `38e118a` that a build-submit `test = true` entry really was unreachable and `tasks` unread; gen 1's universe filtering of the default is reproduced exactly; refusals name the queue, the kind and the fix.

### P4: docs (**Sonnet drafts, Opus reviews**)
- `CLAUDE.md` (§8.1), `mdb/GENERATIONS.md` (preamble §8.2, plus one "Automatically:" paragraph per generation entry), `cactupdocs/content/authors/mdb-generations.md` ("Overlay machines" and the bump procedure, §8.3), `cactupdocs/content/users/updating.md:168-180` and its backup list (`:262`, add `machine-backups/`), spec §17.5's "Overlays" paragraph (`design/cactup-simfactory-design-new.md:5838-5844`).

## 8. Proposed text

### 8.1 CLAUDE.md: replace the "Procedure, all in one commit" paragraph, then add one
> **Procedure, all in one commit:** before bumping, snapshot this generation's overlays with `ci/overlay-fixtures.sh N` (into `fixtures/mdb-overlays/gen-N/`; never edited afterward); increment `mdb/GENERATION`; add the overlay migration step N→N+1 to `mdb::migrate::STEPS` (a step that only raises the number is still a step); append `## Generation N+1` to `mdb/GENERATIONS.md` saying what changed, what the automatic migration does, and the hand recipe for what it refuses and what is optional; migrate every in-repo machine. Tests fail unless every generation up to N+1 has a `GENERATIONS.md` entry, and every older one a step and a snapshot that migrates to the current generation and loads.
>
> **Overlay migration (`src/mdb/migrate.rs`).** A user-layer machine older than the binary is brought forward on disk the first time a login command loads it: under a `LinkLock`, after a full copy into `~/.cactup/machine-backups/<name>/generation-<N>/`, verified by today's parse and validation before anything is written, `meta.toml` last. A step preserves behavior or refuses: it applies mechanical edits, notes a difference that can only surface as a loud error, and stops (keeping the steps already done) where the machine could quietly act differently. It never applies an optional recommendation, never edits a script, and never writes a key the target generation's first binary lacks. An overlay newer than the binary is refused, never downgraded; one with no generation is assumed current and never rewritten. Migration never runs on the compute path (D11).

### 8.2 `mdb/GENERATIONS.md` preamble: replace "An overlay from an older generation is refused … bring it forward."
> A cactup newer than an overlay brings it forward on its own the first time it loads it, and keeps a copy of the old files in `~/.cactup/machine-backups/<name>/generation-<N>/`. Each entry below says what that migration does for you and what it leaves to you. Where it cannot keep the machine behaving exactly as before, it stops and says what to change by hand: follow the recipe of that entry, then set `mdb-generation` to its number, and cactup goes on from there. Changes marked optional are never made for you. (cactup builds older than this feature refuse an older overlay instead; the recipes below are written for that case too.)

### 8.3 `mdb-generations.md`: replace "Updating an overlay machine"
> cactup updates an overlay machine itself the first time it loads it after an upgrade, and prints one line saying so and where the old files went. If it stops instead, its message says what to change and in which file. Make that change, set `mdb-generation` to the generation it names, and run any cactup command again. Another cactup sharing the same `~/.cactup` that is still on the older generation will ask you to run `cactup update`. To go back, copy the backup over the machine directory.

Add to "Bumping the generation": step 0 (the snapshot) and step 2b (the migration step), mirroring §8.1.

## 9. Open questions for Max (answered: see Decisions)

1. **New dependency:** may we add `toml_edit` (pure Rust, static-safe) to keep comments in overlay `meta.toml` and optionlists? Without it, migration rewrites through `toml::Table` and comments survive only in the backup.
2. **The `.py` build-submit `ENV_SETUP` case (2→3):** refuse (recommended: the batch build could quietly get a different module set), or migrate with a loud note? No overlay copied from the in-repo MDB triggers it.
3. **Backups:** is `~/.cactup/machine-backups/<name>/generation-<N>/`, kept forever, acceptable, or should `cactup update --prune` also remove backups older than 30 days?
4. **Gen-1 fallbacks that were bugs:** preserve them (recommended: qbd/saturn-style CPU queues added to a GPU-only test script, exactly as generation 1 ran them, with a note), or refuse so the user picks?
5. **Explicit command:** is automatic migration enough, or do you also want `cactup machine migrate [NAME] [--dry-run]` to show what would change? Not needed for the advisor; recommended later if at all.

## Critical files for implementation
- /home/max/src/Cactup/src/mdb/migrate.rs (new)
- /home/max/src/Cactup/src/mdb/mod.rs
- /home/max/src/Cactup/src/commands/machine.rs
- /home/max/src/Cactup/src/mdb/meta.rs (read: `route`, `validate_script_kind`, `effective_env`)
- /home/max/src/Cactup/src/mdb/optionlist.rs (read: header schema)
- /home/max/src/Cactup/src/main.rs
- /home/max/src/Cactup/src/build_info.rs
- /home/max/src/Cactup/mdb/GENERATIONS.md
- /home/max/src/Cactup/fixtures/mdb-overlays/ (new), /home/max/src/Cactup/ci/overlay-fixtures.sh (new)
- `38e118a:src/mdb/meta.rs` (generation-1 script selection to port)
