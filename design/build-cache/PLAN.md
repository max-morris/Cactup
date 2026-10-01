# Shared build cache for Cactup: feasibility findings and plan

## Context

Every Cactus installation under one Cactup instance compiles every thorn from
scratch, and any optionlist edit forces `realclean` plus a full rebuild
(`rebuild_decision`, `src/build/mod.rs:1056`). The goal is an instance-level
cache of compiler outputs shared by all installations and configurations, so
a new installation or a rebuilt configuration reuses objects whenever, and
only when, compiling afresh would produce the same thing.

Requirements (Max):

1. **No stale objects, ever.** False misses are fine; false hits are not.
2. **Concurrency-safe** across builds, installations, sessions and hosts on
   NFS/Lustre.
3. **Conservative eviction**: nothing deleted automatically; old thorn
   versions stay available.
4. **Project shape preserved**: one static MUSL binary, pure-Rust deps.
5. **Rust thorns later** must not be precluded.
6. **Heterogeneous shared filesystems** (Frank: athena, saturn, Ubuntu nodes
   on one NFS home, one `~/.cactup`): never mix objects. Key on the cactup
   machine; a machine change keys differently, hardware/software changes
   inside a machine key differently too. Nothing here implies eviction.
7. **Where work happens**: all builds, benchmarks and anything that writes
   go in the cactup-managed install `~/cacti/build-cache` (alias
   `build-cache`). `/home/max/Cactus-2026` is read-only reference.

Decisions already made: custom cache built into cactup; relocatable paths by
default; first deliverable is a measurement pass; eviction explicit only,
with a size notice.

## Verdict: feasible, with a custom cache

No existing tool fits. None caches Fortran with `.mod` files; sccache forbids
sharing one cache directory between hosts; buildcache uses `fcntl` locks and
ships a glibc binary; ccache (C/C++/CUDA only, GPL second binary) has no
Rust and identifies MPI/Cray wrapper compilers by the wrapper's mtime.
None of the survey claims below about those tools were re-checked by me.

C/C++ caching is well-trodden (ccache's preprocessor mode) and clearly
feasible. Fortran is feasible but is the riskiest part and comes later, one
compiler family at a time. The store is the easy part.

How much it will actually save is unknown until measured: that is Stage 0.

## What I verified myself

- cactup never wraps compilers; `prepare` freezes a build script that
  `execute` runs verbatim, possibly on a compute node or inside a container
  universe (`src/build/mod.rs:1778`, `1882-1911`, `2229`;
  `src/mdb/meta.rs:640`).
- `main` does interrupt-handler, clap, `Db::open()`, auto-update before
  dispatch (`src/main.rs:73-128`); no hidden verbs or argv[0] dispatch exist.
- Cactus compiles from cwd `<config>/scratch` with absolute paths
  (`lib/make/make.config.rules.in:150-237`). Objects do not depend on
  `make.config.defn` (`make.subdir:44`).
- 37 of 47 MDB optionlists set `C_LINE_DIRECTIVES`/`F_LINE_DIRECTIVES = yes`,
  so the build copy begins `#line 1 "<absolute source path>"` and `__FILE__`
  (used by `CCTK_WARN`, `cctk_core.h:437`) is the installation path. All 47
  use `-g`; 11 use `-march=native`, 6 `-xHost`.
- `make.config.defn` exports `CC` etc., and make exports command-line
  overrides to every recipe; ExternalLibraries build scripts read `${CC}` at
  build time (`HDF5/src/build.sh:115`). So a `make CC='wrapper gcc'`
  override leaks into external-library builds. Tested on make 4.4.1: a
  pattern-specific `private` variable (`%.c.o: private CC = WRAP gcc`)
  puts the wrapper only in object recipes, survives sub-makes, and leaves
  prerequisites and the exported environment on the original compiler.
- Machine detection keeps one `detected` record stamped with hostname and
  login session, re-verified when either changes (spec §4.3). When no
  machine claims a host, the cached machine is kept, and `generic` covers
  every unrecognized host. So the machine name alone cannot separate
  architectures; a host fingerprint must back it.
- This host: machine `plato`, gcc/g++/gfortran 14.2, clang 19, make 4.4.1,
  no MPI wrapper, no nvcc. `build-cache` is registered at
  `~/.cactup/cacti/build-cache`, with no configuration built yet.

Reported by the reviews, not re-checked by me (to confirm when relied on):
gfortran looks up modules in cwd before `-I`/`-J` dirs; gfortran `-M`
requires `-cpp`; NFS rejects `renameat2` flags with `EINVAL`; the NFS client
drops a same-size truncate; `-ffile-prefix-map` behavior on `#line` names.

## Design

**Principle.** The cache sits strictly below make. make and cactup still
decide *whether* to compile; the cache only answers *what this compile would
produce*. A hit must be byte-identical to what the wrapper's own compile
would produce here, now.

### Interposition

- Wrapper mode inside the cactup binary, dispatched as the first statement of
  `main` (before `take_updated_marker` and the interrupt handler), reachable
  by a hidden verb and by argv[0] (cargo's `RUSTC_WRAPPER` takes a bare
  path). It never returns through `main`.
- Injection by makefile fragment, not optionlist or command-line override:
  after `<make> <name>-config`, the build script runs a probe verb that
  reads `config-data/make.config.defn`, writes `<attempt>/cc/inject.mk`
  (pattern-specific `private` CC/CXX/CUCC/F90/F77 on Cactus's `*.c.o`,
  `*.cc.o`, ... targets, guarded on `make.subdir` being in
  `MAKEFILE_LIST`), and the build step runs with `MAKEFILES` pointing at
  it. config-data stays pristine; `-E -M` runs, `datestamp.c`, external
  libraries and configure never see the wrapper.
- Fail-open at three levels: probe fails (container cannot see cactup or the
  cache, make older than 3.82, self-test fails) => plain build; wrapper
  config unreadable => exec the real compiler; any wrapper error or panic
  (hook, since release is `panic = "abort"`) => exec the real compiler
  before the compile, or pass its status through after. Failed compiles are
  never cached. The wrapper prints nothing of its own.
- Per-build settings (cache root, roots for path mapping, mode, machine,
  universe, digest of the frozen `build_env`) are written by `prepare` into
  the attempt dir. The compute-node path reads only that plus the cache
  root (an explicit, documented addition to the D11 list).

### Key

SHA-256 (already linked via aws-lc-rs) over length-prefixed frames
(`feed` framing, `src/build/mod.rs:868`):

- **Platform**: cactup machine name, universe, `build_env` digest (frozen
  at `prepare`); plus, measured by the wrapper on the host that compiles:
  target arch, CPU fingerprint from `/proc/cpuinfo` (vendor, family, model,
  flags), `/etc/os-release` digest. Memoized per attempt, host and boot id.
  The CPU fingerprint is always in the key, which covers `-march=native`,
  `-xHost` and compilers that tune for the host by default, at the cost of
  no sharing between login and compute nodes with different CPUs.
- **Compiler identity**: content digest of the resolved executable (ELF
  only), version output, and per family the real back ends (`cc1plus`,
  `as`, specs). Wrapper compilers (mpicc, Cray, nvcc) only when known, via
  their show-command output plus the underlying identity. Unknown => not
  cached.
- **Arguments**: explicit per-family allowlist; anything unrecognized or
  with extra inputs/outputs (plugins, profiles, PCH, modules, split dwarf,
  coverage, `-MD`) => not cached.
- **Inputs**: the compiler's own `-E -C` output (comments kept, so even a
  comment edit changes the key), with `-dD` under `-g3`.
- **Environment**: allowlist (locale, include-path and compiler variables,
  `LOADEDMODULES`/`_LMFILES_`, MPI/Cray/NVHPC prefixes); a few variables
  make the compile uncacheable. Hashing everything is unworkable: Cactus
  exports about 150 make variables, many path-valued, into every recipe.
- **Double check**: on a miss, `-E` runs again after the compile; the result
  is stored only if both digests match (a header edited mid-build can
  otherwise poison an entry).

**Residual risk, stated plainly:** an environment variable outside the
allowlist that changes a known compiler's output would be a false hit. The
`build_env` digest, module lists, ELF-only rule and audit mode bound it;
Stage 0 logs which variables actually vary so a stricter model can be
judged on data.

### Relocatable paths (cross-installation sharing)

For families with `-ffile-prefix-map` (GCC 8+, Clang 10+ and derivatives),
the wrapper adds maps for the config dir and Cactus root, and applies the
same textual map to path arguments and `-E` line markers when hashing.
`__FILE__` then reads like `./arrangements/CarpetX/CarpetX/src/driver.cxx`
and debug info needs gdb `substitute-path`. Families without the flag keep
absolute paths in the key (correct, no cross-installation hits). A knob
turns mapping off per build. This replaces the relative-argument rewrite I
described when asking: it does not change include resolution, and it is the
only thing that works when line directives are on.

### Store

`<root>/v<N>/<machine>/ab/<key>`: one immutable, self-describing file per
result (key, schema, checksum, outputs, captured stderr). Publication:
temp file in the same directory, `sync_all`, `hard_link` plus `nlink == 2`
check (the `src/lock.rs:226` pattern). No locks, no shared index. Every
restore verifies the checksum while copying (never hard-links) into a temp
sibling, then renames. Anything invalid is a miss and is unlinked and
republished. Root defaults to `$CACTUP_HOME/cache`, movable by knob.

### Eviction

Never automatic. `cactup cache gc --unused-for <age> [--to-size <n>]`
under a heartbeat `LinkLock`, ages in the fileserver clock
(`lock::mtime_age`). Last use is restamped by `execute` after make from the
build's own hit log, at most every few days per entry. `cactup cache stats`,
`cache verify`, and a notice when a size knob is exceeded.

### Audit mode

Knob: on a hit, compile anyway and compare bytes; on mismatch compile a
second time to tell a false hit from a non-deterministic compiler. This is
the acceptance gate for every compiler family and stays available.

### Fortran and Rust

Both need inputs the preprocessed text does not show, so the key gains a
second level: immutable candidate records listing consumed inputs (module
files, `include`s; for Rust dep-info sources, extern rlibs, env-deps,
negative-existence facts), re-resolved through the compiler's real search
order at lookup. Fortran constraints already known: no private module
output dir, statement-level scanner that fails closed, digests before and
after the compile, gfortran first, audit cross-check. Nothing in the store
or wrapper is incompatible with a rustc adapter.

## Working arrangement

**Branch and worktree.** All code lives in a git worktree of the cactup repo
on branch `feature/build-cache`, pushed to the remote with upstream tracking
from the first commit and after every signed-off milestone. master is not
touched. Commits carry no AI attribution.

**Sessions and accounts rotate.** Nothing may depend on a particular Claude
session, its name, or its per-account memory. Everything needed to resume is
on disk: this plan, `STATUS.md` next to it (milestone status, review
verdicts, the next step), and the shared contract file
`~/tmp/build-cache-speedup-contract/CONTRACT.md`. `STATUS.md` is updated at
every milestone and whenever work stops mid-milestone.

**Review gates.** Each milestone below ends at a gate. Progress past it
requires sign-off from a twin pair of harsh reviewer agents:

- Two independent reviewers, launched in parallel with the same brief. Each
  gets the milestone's full diff, this plan, `CLAUDE.md` and the relevant
  spec sections; neither gets my conclusions or the other's findings.
- Each reviews all of: correctness (including every false-hit and
  fail-open path), code quality, performance (wrapper per-invocation cost,
  NFS round trips), safety/security (cache poisoning, path handling, what
  the wrapper executes), and adherence to cactup's principles, design
  philosophy and UX language (contracts in `CLAUDE.md`, message wording,
  progress and interrupt behavior, help text).
- Each ends with an explicit verdict: SIGN-OFF, or a list of blocking
  findings with file and line. Non-blocking remarks are listed separately.
- I fix every blocking finding (or answer it with evidence, which the
  reviewer must then withdraw or sustain), and both reviewers re-review the
  whole milestone diff. This repeats until both sign off in the same round.
  A finding I cannot resolve with a reviewer goes to Max instead of being
  overridden.
- The milestone is then committed, pushed, and reported with the reviewers'
  final verdicts and any non-blocking remarks left open.

**Compatibility with the build-speed work** (session `speedup-build-d4`,
which appears to work in `~/cacti/speedup-build`; its flesh checkout is
still unmodified at `ae66cd2`). I could not reach that session by message:
it is not in my session list, by name or by ref. So:

- First action after approval: write the shared interface file
  `~/tmp/build-cache-speedup-contract/CONTRACT.md`, containing the eight
  make-system assumptions listed under "Interposition" and "What I verified"
  (object naming `*.c.o`, compiles in a `make.subdir` sub-make,
  `make.config.defn` variable lines, one source per `-c -o` invocation from
  cwd `scratch`, dependency generation as a separate `-E -M` run, flat
  `.mod` files in `scratch`, objects independent of `make.config.defn`,
  make 3.82+), what my wrapper adds (an extra `-E` per cached unit, a
  `MAKEFILES` fragment, `-ffile-prefix-map` flags, edits to `prepare` and
  `BuildMeta`), and my questions (where its work lives, planned recipe
  changes, cactup or MDB edits). Max points the other agent at it, or makes
  the session reachable, and I retry the direct message at each milestone.
- Both of us record planned and landed changes there; the file also names
  who owns which cactup source files. I keep my edits to `prepare` to one
  call into `src/objcache` so conflicts stay small, and rebase on master
  at every milestone.
- The design absorbs likely speedups instead of forbidding them: result
  entries hold any number of outputs with a per-adapter restore hook, so a
  merged `-MD -MF` depfile (paths rewritten on restore) fits; the injection
  patterns and guard come from one table that the probe checks against the
  actual tree. If the make system no longer matches (renamed objects, no
  `make.subdir`, non-recursive make, batched compiles, another compiler
  wrapper), the probe disables the cache for that build with a one-line
  reason; it never breaks the build or guesses.
- I never write in `~/cacti/speedup-build`. To test against its changes I
  check its flesh branch out in my own install.
- Every review gate includes: the contract file is current, and the
  milestone works on both the stock flesh and the latest speedup branch
  (or the probe declines cleanly).

**Milestones.**

- M0a: wrapper dispatch, fail-open paths and panic hook, probe and
  `inject.mk`, per-build config, knobs. No keys yet. Gate.
- M0b: argument parser, platform/identity/environment digests, key,
  `events.log`, `cache report`. Gate.
- M0c: measurements in `~/cacti/build-cache` and a written results summary
  (hit rates, overhead, Fortran share, variables that vary). Reported to
  Max; the Stage 1 design is adjusted to the data before any object is
  served.
- M1a: store (publish, restore, invalidate). Gate.
- M1b: serving, double check, audit mode; the two-installation audit build.
  Gate.
- M1c: `cache stats|gc|verify`, size notice, spec section, `CLAUDE.md`
  contract, user docs. Gate.

## Stages

**Stage 0: measurement (the next thing built).**

- `src/objcache/`: wrapper entry and panic hook, per-family argument
  parser (GCC, Clang), environment and platform digests, compiler
  identity, key, probe/`inject.mk` writer. Record-only: computes keys and
  appends one line per invocation (cacheable or reason, component digests,
  timings, sizes) to `<attempt>/cc/events.log`; serves nothing.
- `src/main.rs` dispatch; `prepare` script composition and per-build config
  (`src/build/mod.rs:1882`); one optional `BuildMeta` field
  (`src/build/attempt.rs`); knobs `build-cache` (off/record), `build-cache-dir`
  (`src/database.rs:67`), resolved and frozen at `prepare`.
- `cactup cache report` comparing two attempts' logs: would-hit rate by
  language and thorn, and which key component differed.
- Measurements in `~/cacti/build-cache`: two configurations (cross-config);
  line directives and `-g` on/off; edit-and-revert a thorn; a fresh shell
  session; wall-clock overhead against an unwrapped build; share of build
  time that is Fortran. Cross-installation numbers need a second tree: I
  would `cactup install` one sibling alias (`build-cache-b`) for that and
  touch nothing else.
- Tests: table-driven parser/env/key tests; make self-test; integration test
  with a fake compiler (exit codes, signals, argv[0], induced panic).

**Stage 1: serve C/C++** (GCC, Clang): store, restore, double check, audit
mode, `cache stats|gc|verify`, size notice. Gate: a full audit build in two
installations with zero mismatches. Spec section and CLAUDE.md contract
added here; user docs in `cactupdocs`.

**Stage 2:** nvcc/hipcc adapters; gfortran with the two-level key.
**Stage 3:** further Fortran families; rustc adapter when Rust thorns exist.

Contracts: no `mdb/GENERATION` bump (knobs and a cactup-generated script
only); no new C dependency; interrupt polling and progress for the `cache`
commands via `par::parallel_map` and `manifest::setup_prodash*`; no §-refs
in user-visible strings; no `libc` crate; hand-format touched files only
(never `cargo fmt`).

## Verification

- Stage 0: `cargo test`; build `build-cache` with `build-cache = record`
  and confirm the executable is unchanged from an unwrapped build, external
  libraries never saw the wrapper, and `cache report` output is produced.
  A container or missing-binary probe failure must leave a normal build.
- Stage 1: audit gate above; concurrent publishers of one key; kill -9
  mid-publish; truncated entry; header edited between `-E` and compile;
  two fake hosts with different CPU fingerprints.
- CI static-link check still passes on both MUSL targets.
