# Build cache: status

Read `DECISIONS.md` (what Max has decided) and `HANDOFF-M1.md` (where to
start M1) first when picking the work up; this file is the record. The plan is `PLAN.md` in this
directory (as approved; this file and the spec say where the work has moved
since); the spec is §18 of `design/cactup-simfactory-design-new.md`.

The interface contract with the build-speed work is a live file outside the
repository, `~/tmp/build-cache-speedup-contract/CONTRACT.md` on the host
where both workstreams run (Max's workstation, `plato`): the other side
cannot write to this branch. `CONTRACT.snapshot.md` here is a copy taken at
the last milestone, for when that host is not at hand.

## Rules that are easy to forget

- Work only in this worktree (`.claude/worktrees/build-cache`, branch
  `feature/build-cache`). master is not touched.
- Test builds, benchmarks, and anything that writes into a Cactus tree go in
  the install `~/cacti/build-cache` (alias `build-cache`), plus one sibling
  alias `build-cache-b` for cross-installation measurements.
  `/home/max/Cactus-2026` is read-only reference. Never write in
  `~/cacti/speedup-build`.
- While developing on this shared instance, turn the cache on per command
  (`cactup -K build-cache=record build …`), not with `cactup knob`: the
  instance's database is shared with other sessions and with installed
  cactup builds that do not know the knob yet. (Users of a released cactup
  set the knob normally.)
- While reviewers are at work, leave the worktree alone: they build and
  read in it, and edits appearing under them cost them a clean export.
  Prepare fixes only once both have reported (or in a second worktree).
- Every milestone ends at a review gate: two independent harsh reviewer
  agents, same brief, full milestone diff. Fix or answer every blocking
  finding and re-review until both sign off in the same round. Record the
  verdicts below.
- Re-read the contract file at every milestone, keep the cache-side
  sections current, and refresh the snapshot.
- Test the make-facing parts against more than one GNU make:
  `CACTUP_TEST_MAKES=$HOME/tmp/build-cache-tools/make-4.2.1/make:$HOME/tmp/build-cache-tools/make-4.3/make
  cargo test` (built from GNU sources during M0; 3.82 does not run on a
  current glibc).
- `CLAUDE.md` is untracked and shared by every session in the repository:
  do not edit it from this branch. Its text waits in `CLAUDE-contract.md`.
- Commits carry no AI attribution. Never run `cargo fmt`.

## Milestones

| Milestone | Scope | State |
|---|---|---|
| M0a | Wrapper dispatch, fail-open paths, panic hook, probe and `inject.mk`, per-build config, knob | **passed the gate** at `300fd0b` (four review rounds) |
| M0b | Argument parser, platform/identity/environment digests, key, richer `events.jsonl`, `cache report` | **passed the gate** at `045eb76` (four review rounds) |
| M0c | Measurements in `~/cacti/build-cache`, written results | **done**: results in `RESULTS-M0c.md`, answered by Max on 2026-10-02; the code changed since M0b's gate **passed review** at `129ecf7` (three rounds) |
| M1a | Store: publish, restore, invalidate; the `build-cache-dir` knob; thorn stand-down on `include`/`define`/`eval`; the shell asked what the compiler's name resolves to; link-only GCC specs files | **passed the gate** at `9d8f62b` (three review rounds) |
| M1b | Serving, the locale trial, dependency file on a hit, audit mode, two-installation audit build | **passed the gate** at `6b8efb1` (four review rounds; audit builds GCC and Clang, 0 wrong) |
| M1c | `cache stats/gc/verify`, size notice, the cache beside the installations, a knob per `[paths]` key; contract into `CLAUDE.md` with the merge | **passed the gate** at `01bd00a` (three review rounds) |
| M2a | gfortran (§18.10): keyed by its dependency run, served with its module files (store format 2), audited; the source named from `scratch` under the path map (decision 13) | **passed the gate** at `a14f279` (four review rounds; Einstein Toolkit audit builds on that commit, both optionlists, 0 wrong) |
| M3a | The check after the compile runs no compiler: the files read again and the compiler's lookups repeated by cactup (decision 14) | **built** (`46f4c8c`); Einstein Toolkit gate passed on `afa94e7`; review next |
| M3b | Keys from the compile's own dependency list, no preprocessor run on a miss or a hit (decision 14) | **only if Max decides so** on M3a's measured lookup cost |
| later | the CUDA compilers; the narrower key revisited with audit mode | not started |

## What M0a is

- `src/objcache/mod.rs`: the `build-cache` knob, the frozen per-build
  `BuildConf` (`<attempt>/cc/config.toml`), `stage` (called from `prepare`),
  and the two build-script steps.
- `src/objcache/probe.rs`: `cactup __cc-probe`, the `inject.mk` writer, the
  two self-test makefiles.
- `src/objcache/wrapper.rs`: `cactup __cc`, dispatched first thing in
  `main`; pass-through, record mode, signal handling, panic hook.
- `tests/objcache.rs`: the binary driven as make drives it, including a
  miniature Cactus sub-make under every make in `CACTUP_TEST_MAKES`.
- Tests in `src/build/mod.rs` that drive `prepare`/`execute` with the cache
  on and off.

How the design differs from the plan (the plan's "Interposition" section
describes what review round 1 rejected):

- **Injection redefines Cactus's compile recipes** (`override define
  COMPILE_C …`), copied from the configuration's own `make.config.rules`
  with the compiler reference replaced. The plan's pattern-specific
  `private` compiler variables leak into the environment of prerequisite
  recipes on GNU make before 4.4 (so into ExternalLibraries builds),
  silently beat a thorn's less specific pattern, and pin a foreign
  makefile's `.c.o` objects to Cactus's compiler.
- **The fragment decides when it is read** whether it is in a Cactus object
  sub-make (`CCTK_TARGET` set, working directory under the configuration's
  `build/`), unexports `MAKEFILES` there, stands down for a thorn that
  mentions the compile recipes in its own make fragments, and removes
  itself from `MAKEFILE_LIST` everywhere.
- **The compiler text travels as one quoted argument**, with make's
  `$(SHELL)` as another, so the wrapper sees exactly what `$(CC)` expanded
  to for that target. The recipe's shell is the reference: anything that is
  not a plain command, and any plain command the wrapper cannot start
  itself (a shell keyword, a function, a script without `#!`), goes to a
  shell of that kind.
- **`rustix` is a direct dependency**, for `kill(2)` and `waitpid(2)` only
  (spec D13 says so).
- **Dropped from M0a** as not yet needed: the `build-cache-dir` knob and
  the cache root, machine, universe and environment digest in `BuildConf`
  (they come back with the code that reads them), the configuration format
  version, and the `cactup-cc` file-name entry point (it returns with the
  rustc adapter).

## What M0b is

Record mode now keys every compile it can and logs the key; nothing is
stored or served, and the real compile is untouched (spec §18.5, §18.6).

- `src/objcache/hash.rs`: framed SHA-256.
- `src/objcache/compile.rs`: the GCC/Clang command-line reader (a list of
  known flags; anything else is "not cached").
- `src/objcache/identity.rs`: which compiler, by content (the file `execvp`
  would run, the name it is run by, GCC's back ends, assembler and `specs`
  file, Clang's configuration files, the shared libraries they load),
  remembered per attempt, also when the answer is "not one the cache works
  with"; and the trial of the path map (`relocates`).
- `src/objcache/platform.rs`: machine, universe, environment-setup digest
  (frozen by `prepare`), and the compiling host's architecture, processor
  kinds (with each processor's caches) and OS release; whether the host's
  processors are all of one kind.
- `src/objcache/environment.rs`: the allowlisted environment.
- `src/objcache/key.rs`: the path map, the preprocessor run, the bytes of
  every file it names, the six-part key.
- `src/objcache/event.rs`: the event log's format.
- `src/commands/cache.rs`, `CacheCommand` in `src/args.rs`: `cactup cache
  report`.
- `tests/objcache.rs`: besides the wrapper tests, the claim itself tried
  with the real compilers of the host (`gcc`, `g++`, `clang`, `clang++`,
  nine flag sets, build copies with and without a line directive): two
  trees at different paths with configurations of different names; where
  the keys agree, the objects compiled with the map's flags must be the
  same bytes.
- The points carried over from M0a's round 4 (all but the `SIGPIPE` one,
  which is documented in spec §18.4 as a limit): the compiler's `_`, the
  narrower `~` rule with a reason on record, the self-test's third run and
  its `&&` chains, a compiler on a continuation line, the `BASH_ENV`
  wording, the self-test test over `CACTUP_TEST_MAKES`, more shell words.

**How the key changed after review round 1** (both reviewers showed pairs
of compiles with one key and two objects; see the verdicts below):

- The key now has a sixth part: the bytes of every file the preprocessor
  names. `-C` is gone (it turned a directive behind a comment into text).
- The path map follows the compilers' own rule (a plain string prefix, at
  the start of a name; maps given with a trailing `/`), and is used only
  for a compiler that passed a trial: a miniature Cactus compile in two
  places must give one object. Clang with `-fopenmp` keeps its paths.
- Not keyed at all: `-x`, `-imacros`, sanitizers, `-mllvm`, every
  `-g…`/`-O…` flag not listed by name, a source with `.incbin`/`.include`,
  a compile that would use a precompiled header, Clang with `-include`,
  `-march=native` on a host with more than one kind of processor, and a
  compiler that takes flags from behind its command line (a GCC `specs`
  file, a Clang configuration file, `CCC_OVERRIDE_OPTIONS`).
- The compiler that is identified is the file that runs.

Numbers (2026-10-01, `plato`, GCC 14.2, make 4.4.1, an *unoptimized*
cactup, `-j 8`; M0c is where these get measured properly):

- `smoke` (25 thorns): 357 compiles, 307 keyed (all C and C++), all 307
  with keys free of the installation's paths; the 50 Fortran compiles are
  2% of the compile time. All 357 objects are byte for byte those of a
  build without the cache.
- `smoke2`, the same sources under another configuration name, against
  `smoke`: **307 of 307 keyed compiles would be served** (98% of the
  compile time).
- `ext`, which adds HDF5 and two thorns that use it, against `smoke`: 185
  of 334 (55%, 64% of the compile time). Before the rework it was 301 of
  334: 116 compiles now miss because a generated header they include
  (`cctk_DefineThorn.h`, `CParameterStructNames.h`) has other bytes in a
  configuration with more thorns, though it gives them the same tokens.
  This is the price of keying bytes, and a decision for Max (below).
- Cost: keying summed to 19% of the compile time and checking again to
  18%; wall clock 17 to 21 s against 16 s plain for `smoke`. (Before the rework:
  30% and 25%, 39 s; `-C` made the preprocessor's output much larger.)

**Found on the way: `-march=native` is not a function of its inputs on
`plato`.** Its sixteen cores are of three kinds, and GCC resolves `native`
to three different sets of cache parameters depending on the core it lands
on, so one compile run twice gave two objects (the audit test caught it as
"one key, two objects", one run in six). Such compiles are now not keyed
on a host whose processors differ. The configurations above do not use
`-march=native`; 11 of the MDB's optionlists do.

The logs behind these numbers are `smoke` attempt 0013, `smoke2` attempt
0004 and `ext` attempt 0007 in `~/cacti/build-cache`, all recorded by the
revision under review.

"Would be served" rests on the path mapping of spec §18.5 producing the
same object, which the per-compiler trial and the audit test support and
audit mode (M1b) has yet to try on real Cactus compiles.

## What M0c is

`RESULTS-M0c.md` has the numbers and what they mean. How they were taken:

- A static release build of cactup at `045eb76`, copied to
  `~/tmp/build-cache-m0c/cactup`; scripts and build logs beside it.
- A second installation, `cactup install master -a build-cache-b`
  (`~/.cactup/cacti/build-cache-b`). The active installation was not
  changed.
- In `~/cacti/build-cache`: thornlist `et-trim.th` (the master thornlist
  minus the CarpetX stack, which does not build on `plato`: ADIOS2 and
  AMReX do not find the from-source MPI) with option list `et.toml`
  (config `et`: attempt 0007 recorded, 0008 plain); `linedir.toml`
  (configs `ld1`, `ld2`, line directives on); `depend.toml` (config `dep`,
  dependencies written by the compile, built with the flesh at the
  build-speed branch `5d8deb7` and the flesh then put back on master);
  `smoke` attempts 0014 to 0017 (edit and revert of a source and a
  header, both restored); `smoke2` built from `env -i` login shells.
- In `~/cacti/build-cache-b`: `et` (attempt 0002) and `ld1`.

Code changed after M0b's gate (to go through the reviewers before anything
builds on it):

- Dependency output written by the compile (`-MD`/`-MMD`, `-MP`, `-MF`,
  `-MT`/`-MQ`) is understood: not keyed, kept from the preprocessor runs.
  The build-speed side's recipe needs it (contract A6).
- The two round 4 remarks: the driver is asked in English
  (`LC_MESSAGES=C`, other locale categories untouched), and Clang's answer
  must carry `InstalledDir:` or it is no answer.

## What M1a is

The store exists and is tested on its own; no build writes to it or
reads from it yet (that is M1b). Around it, the points `HANDOFF-M1.md`
put into M1a. Spec §18 was written first, the code after.

- `src/objcache/store.rs` (spec §18.7): one immutable file per key under
  `<root>/v1/<machine>/<ab>/<key>`; magic line, a line of the four
  lengths, TOML header (format, key, the six parts, and an `about` table
  for people), object, compiler stdout and stderr, SHA-256 of all of it.
  Publish: temporary file in the entry's directory, `sync_all`, mode
  `0444`, `hard_link` with the `nlink == 2` check. Restore: checks the
  lengths against the file size, copies the object into a temporary file
  (created `0666`, left to the umask) beside the target while digesting,
  compares the checksum, only then reads the header, and renames the
  object into place. An invalid entry is removed if its name still leads
  to the file read (device and inode); an I/O error, or a whole entry
  with another cactup's header, leaves it alone. (As after round 1.) Tested with
  threads and with separate processes publishing and restoring one key
  at once, every kind of damage, strict headers, a publish cut short.
- The knob `build-cache-dir` (absolute path, default
  `$CACTUP_HOME/cache`), resolved in `prepare`, frozen as `store` in
  `BuildConf`. Spec §18.1 rule 6 (D11) now names the store.
- Decision 7a (spec §18.3): the fragment stands down for a thorn whose
  `make.code.defn` or `make.code.deps` matches `probe::STAND_DOWN` (any
  `COMPILE_`, an `include`/`-include`/`sinclude` directive, a `define`
  directive with or without `override`/`export`/`private`/`unexport`,
  `$(eval`/`${eval`), read by `grep -E` once per object sub-make; it wraps
  only on exit status 1. The shell picks the files that exist, because
  `$(wildcard …)` crashes GNU make 4.2.1 built against a current glibc
  (found by the test under 4.2.1). A missing `grep` fails the self-test.
- Decision 7b (spec §18.4): `src/objcache/lookup.rs`. Before a compile
  in record mode, the wrapper asks `<shell> -c 'printf "\n<marker>";
  type "$1"'` (output to a file, in English) and starts the compiler
  itself only if the answer is `<name> is <path>` (or `hashed (<path>)`)
  for the file `find_program` finds (physical paths compared); otherwise
  the compile goes to the shell, with the answer in the log. (`type` and
  the file since round 1.) Remembered per attempt in
  `<attempt>/cc/compilers/shell-*.toml`, tied to the found program,
  `PATH`, `BASH_ENV`, `ENV`, `ZDOTDIR`, `HOME`, the shell's file and the
  files `BASH_ENV`/`ENV` name, and the working directory when `PATH` has
  a relative entry. Cactus's `SHELL` is `/bin/bash`, so this is live.
- Decision 3's refinement (spec §18.5): `src/objcache/specs.rs`. A GCC
  specs file is accepted when every section it defines is unchanged from
  `-dumpspecs`, is on the `LINK_ONLY` list, or is new and referred to only
  from that list (and not as `%(name)`/`%[name]` anywhere in the driver's
  bytes, which hold GCC's compile steps); no directives, suffix entries,
  duplicates or comments. The file's bytes join the compiler's identity,
  `Compiler::specs` records it, and every compile must say it read
  exactly that file. The list was checked against the GCC 14 driver's
  strings: every reference to those sections is inside the link command,
  which is guarded by `%{!c:…}`.
- The two small points from M0c's review: the doubled blank line, and
  `identity::tests::a_driver_is_asked_in_english`.
- Docs: `building-configs.md` (the knob, the stand-down, the shell, the
  specs files), the knob lists in `meta-toml.md` and
  `running-simulations.md`, `cactup build --help`, `CLAUDE-contract.md`.

Real builds (2026-10-02, `plato`, debug build of `659f434`, `smoke`, `-f
-j 8`; logs and object hash lists in `~/tmp/build-cache-m1a/`):

- Record against plain: 357 recorded, 307 keyed, all 307 relocatable,
  every object byte for byte the plain build's (as at M0).
- With `BASH_ENV` naming a file that defines nothing: bash started 2708
  times in the build and cactup asked it 3 times (gcc, g++, gfortran);
  307 keyed; objects identical.
- With `BASH_ENV` defining a function `gcc`: the 280 C compiles were
  left to the shell and ran through the function; objects identical.
  (Before M1a the wrapper would have bypassed the function for them.)
- The flesh at the speed side's `build-speedup` `2500bc4` (put back on
  master after): `smoke`, and `dep` with `-MD -MP`, objects and `.d` files
  identical with and without the cache (714 files each); `dep` against
  `smoke`, 307 of 307 would be served.

Not verified: NFS or Lustre (`plato` has neither; the cross-process test
ran on ext4), a real Spack or site GCC (the specs tests use a copy of
this host's GCC 14 driver with a specs file beside it).

## What M1c is

Spec §18.9, written before the code.

- Last use: after the build step of a serving or auditing build,
  `objcache::after_build` (called from `execute`, whatever became of the
  build) writes one new file `<root>/v1/<machine>/used/<random>.keys`
  listing the keys the build found; its modification time is when. `gc`
  folds the logs it read into `<random>.times` (`<key> <seconds>` per
  line) and removes them. Nothing shared is written by two processes.
- `src/objcache/upkeep.rs`: the walk (`scan`, over the entry directories
  with `par::parallel_map` and a progress line), `plan` (a pure function of
  the walk and the fileserver's "now": unused for the age, then least
  recently used down to `--to-size`, temporary files a day old, the folded
  logs), `carry_out` (removes by name only what is still the file walked,
  stops on Ctrl-C saying what it removed), the use log, sizes and ages,
  the size stamp `<root>/v1/size`.
- `cactup cache stats`, `cache gc --unused-for <age> [--to-size <size>]
  [--dry-run]` (under a heartbeat `LinkLock` `<root>/gc.lock`), `cache
  verify` (`Store::check`: a restore that writes nothing; removes the
  invalid, leaves another cactup's).
- Knob `build-cache-size`, frozen as `BuildConf.size_limit`; the size
  notice after a serving build, the store measured at most once a day.
- The three points M1b's round 4 left (the closing line counts
  `second-compile-failed`; a stop signal is kept however audit's second
  compile ends; the no-birth-time limit in §18.5).
- Docs: the user docs' cache section is no longer "in development", and
  covers the commands and the knob; `cactup build --help`.

Tried on the stores of the M1b gate runs: `stats` reads 2783 entries;
`verify` finds those of an earlier key label to be another cactup's and
leaves them; `gc --to-size 300M` on a copy removed the 2150 least recently
used.

`CLAUDE-contract.md` goes into `CLAUDE.md` in the step that merges the
branch to master (decision 10).

## What M1b is

Spec §18.8, written before the code; §18.1 rules 2 and 4 now name the one
change a serving cache makes to a compile.

- `build-cache = serve | audit` (`Mode::serves`). `src/objcache/wrapper.rs`
  `cached`: key; on a hit restore the object (`store::Store::restore`),
  put the dependency file in place, write the stored messages, exit 0; on
  a miss compile with the path map's flags (`Keyed::compile_flags`), the
  compiler's stdout and stderr passed on as they come and kept (4 MiB
  cap), check the key again, publish (`store::Store::publish`). Audit
  mode restores beside the object, compiles anyway, compares, and on a
  difference compiles once more: `same`, `wrong hit`, `not
  deterministic`, `compile failed`. The signal handlers are registered
  once per process (audit runs a second compile).
- Dependency files on a hit (`key::depend_flags`): the key's preprocessor
  run is given the compile's `-MD` flags with `-MF` pointing at a
  temporary file beside the real one, plus `-MQ <object>` when the compile
  names no target (what the GCC and Clang drivers do with `-o`; checked on
  both, also with spaces, `$` and `#` in the name).
- Messages of a relocatable entry are stored with the configuration
  directory and the Cactus root as `@CACTUP_CONFIG@/` and `@CACTUP_ROOT@/`
  (`PathMap::messages_for_the_store`) and written back as this build's
  directories (`key::messages_for_this_build`).
- Decision 5: `identity::locale_neutral`, a trial compile of non-ASCII
  source in the session's locale and in C; a compiler that passes is keyed
  without `LANG`, `LANGUAGE`, `LC_ALL`, `LC_CTYPE`, `LC_MESSAGES`
  (`environment::digest(locale)`); the remembered identity depends on the
  locale. GCC 14 and Clang 19 pass here.
- Knob `build-cache-relocate` (`yes`/`no`), frozen as `BuildConf.relocate`.
- The build step's closing line counts served, published, and in audit
  mode checked, wrong and not deterministic, from the event log; `cache
  report` has a serving section.
- Not cache code, but in this milestone's diff: `sim::start::script_command`
  runs a `#!` script through its interpreter (the "Text file busy" fix,
  commit `7fd1a38`, kept separate for master).

Real builds so far (2026-10-02, debug build of `9a92835`, store in
`~/tmp/build-cache-m1b/store`, logs there):

- `smoke`, serve, from scratch: 307 published (19 s); again: 307 served
  (12 s; Fortran, configure and the link remain); the served objects are
  byte for byte the compiled ones.
- `smoke2` (another configuration name) and `smoke` in `build-cache-b`:
  307 of 307 served; every C and C++ object identical to `smoke`'s (only
  the 50 Fortran objects, compiled with absolute paths, differ).
- `smoke`, audit: 307 checked, 307 the same.
- Clang (`clang.toml`: `et.toml` with `clang`/`clang++`, config
  `clsmoke`): 307 published, 307 served, 307 audited the same.

## Verification done for M0a (2026-10-01)

All in `~/cacti/build-cache`, machine `plato`, GCC 14.2, GNU make 4.4.1,
`cactup -K build-cache=record build …`:

- `smoke` (thornlist `~/cacti/build-cache/smoke.th`: 25 thorns, C, C++,
  F77, F90): 357 events for 357 objects; every object byte-identical to a
  rebuild without the wrapper; `config-data` names the real compilers; no
  line from cactup in the build output.
- `ext` (`ext.th` and `ext.toml`: the same plus HDF5 built from source):
  384 events for 384 Cactus objects; HDF5's installed `h5cc` has
  `CCBASE="gcc"`; nothing under `scratch/external` or `scratch/build`
  carries a trace of the wrapper (`__cc`, `inject.mk`).
- `smoke` again with the flesh at the build-speed branch `build-speedup`
  `996c71f` (checked out in this install's flesh, then restored to master):
  357 events, 357 objects, identical.
- A full rebuild (`-f`) first showed the fail-open path for real: after
  `realclean` there is no `build/` directory, the probe declined, the build
  ran uncached with one line. The probe now creates the directory.
- `smoke -f --clean`: the probe runs after the clean step; 357 events.
- `cargo test` with `CACTUP_TEST_MAKES` naming make 4.2.1 and 4.3: all
  pass, on the glibc build and on `--target x86_64-unknown-linux-musl`
  (`--test objcache`; CI now runs that too). Make 3.82 could not be run on
  this host, and 4.2.1 crashes on Cactus's real `Makefile` with or without
  the cache, so real trees are covered on 4.3 (by reviewer B, on a copy)
  and 4.4.1 only.

The builds above were all repeated after the round 2 and round 3 changes.

Not verified: a container universe, a compute node, NFS or Lustre, any
compiler but GCC, any machine but `plato`.

## Log

- 2026-10-01: Plan approved. Worktree and branch created from master
  `47c67f5`. Contract file written; the build-speed session
  (`speedup-build-d4`) was not reachable by direct message, so coordination
  runs through the contract file. Questions a-d in the contract are open and
  the speed side has not written in the file yet.
- 2026-10-01: M0a implemented (commit `781984e`) and sent to the twin
  review.
- 2026-10-01: Review round 1: both reviewers BLOCKED (see below). Injection
  redesigned, wrapper reworked, tests rewritten, spec §18 rewritten
  (commit `07cbd2b`).
- 2026-10-01: Review round 2: both BLOCKED again, on the wrapper's starting
  of compilers and on recipes with the compiler behind something else.
  Fixed (commit `6bdd9b8`); sent to round 3.
- 2026-10-01: Review round 3: reviewer A signed off, reviewer B blocked on
  a self-test that had stopped testing and on shell-shadowed compiler
  names. Fixed (commit `300fd0b`); sent to round 4.
- 2026-10-01: Review round 4: both reviewers SIGN-OFF on `300fd0b`. M0a has
  passed its gate.
- 2026-10-01: M0b implemented, with the points carried over from M0a's
  round 4, and sent to the twin review.
- 2026-10-01: M0b review round 1: both BLOCKED, on the soundness of the
  key. Key reworked (bytes of every file read, compiler-faithful path map
  behind a per-compiler trial, stricter flag list, compiles with no one
  object declined), the audit test added; sent to round 2.
- 2026-10-01: M0b review round 2: every round 1 finding confirmed
  resolved; both BLOCKED on new ones (three between them). Fixed; sent to
  round 3.
- 2026-10-01: M0b review round 3: A signed off, B blocked on one finding
  (a Clang configuration file chosen by target). Fixed; sent to round 4.
- 2026-10-01: M0b review round 4: both reviewers SIGN-OFF on `045eb76`.
  M0b has passed its gate. M0c (measurements) started.
- 2026-10-01: M0c measured and written up (`RESULTS-M0c.md`). Found on
  the way: the build-speed branch can add `-MD -MP -MF -MT` to compiles;
  the cache now understands that. The code changed since the gate sent to
  the reviewers.
- 2026-10-01: M0c review: both reviewers found the code sound and the
  numbers right, and blocked on one wrong sentence in the results'
  summary. Corrected, with their other remarks; sent back.
- 2026-10-01: M0c review round 2: A signed off, B blocked on a test
  assertion that does not hold for Clang. Fixed; sent back.
- 2026-10-01: M0c review round 3: both reviewers SIGN-OFF on `129ecf7`.
  Everything on the branch up to that commit has passed review. Waiting
  for Max.
- 2026-10-02: Max answered the open questions and gave the go-ahead for
  M1. Recorded in `DECISIONS.md`; `HANDOFF-M1.md` written for the start
  of M1.
- 2026-10-02: Max ran decision 3's check on qbd: a site-built GCC 13.2
  whose specs file only adds an rpath to the link. The refinement joined
  M1a. Spec §18 written for M1a (`37c2fbb`), then the code: the store
  and knob (`b16ebb3`), the stand-down (`d2413f8`), asking the shell
  (`787f0a1`), link-only specs files (`1a8de98`).
- 2026-10-02: M1a review round 1: both BLOCKED (6 findings between
  them); fixed in `916b0cb`. Round 2: B SIGN-OFF, A BLOCKED on flaky
  tests; fixed in `9d8f62b`. Round 3: both SIGN-OFF on `9d8f62b`. M1a has
  passed its gate.

## Review verdicts

### M0a, round 1 (on `781984e`): BLOCKED by both

Both reviewers independently reproduced the same five defects; reviewer A
added a sixth.

1. The self-test (rightly) rejected every GNU make before 4.4: there a
   `private` pattern-specific value of an exported variable is exported
   into the object recipe's and the prerequisites' environment. The cache
   would have been off on most clusters, and CI (make 4.3) red.
   *Fixed by the recipe-override injection.*
2. Replacing inherited "ignore" signal dispositions: a build under `nohup`
   died on hangup. *Fixed: signals ignored on entry are left alone.*
3. A comma or parenthesis in a path broke every wrapped compile, and the
   self-test did not notice. *Fixed: paths outside a plain character set
   make the probe decline; the self-test now runs the real wrapped recipe.*
4. With the guard off, the fragment still pinned a foreign makefile's
   `.c.o` objects to Cactus's compiler, and `MAKEFILES` put the fragment
   first in every make's `MAKEFILE_LIST`. *Fixed: the fragment defines
   nothing outside Cactus's object sub-makes, unexports `MAKEFILES` there,
   and removes itself from `MAKEFILE_LIST`.*
5. A compiler that is a script without a `#!` line failed (the static musl
   binary has no `execvp` fallback). *Fixed: such a file is handed to
   make's shell.*
6. (A) A thorn setting its compiler by a less specific pattern was silently
   replaced. *Fixed: the wrapper no longer sets or reads compiler
   variables.*

Non-blocking points taken: panic-hook hang and reap window (no thread any
more; exit status recorded immediately after the wait), `stage` failing
`prepare` (now a warning and an uncached build), the duplicate `sh_quote`,
the configuration format version, unused `BuildConf` fields and knob, the
`cactup-cc` entry with no producer, `$(SHELL)` for non-plain compilers,
tests through `prepare`/`execute`, a test that a failed build step stops
the script, spec drift (SIGQUIT, rule list, D13), user docs for the knob,
the contract's A and C lists.

Left as is, with the reason: with `SILENT=no` Cactus echoes its recipes, so
the wrapper shows in front of the compiler on each echoed line (that is
make's output; spec §18.1 rule 4 says so). `PLAN.md` stays as approved, with
a note at its top.

### M0a, round 2 (on `07cbd2b`): BLOCKED by both

Both confirmed the round 1 blockers resolved (the injection on make 4.0
through 4.4.1, on real Cactus under 4.3 by reviewer B) except the script
without `#!`, and found:

1. (both) The fallback for a script without `#!` put the arguments before
   the script on the pass-through path. Only the static musl binary shows
   it; glibc's `exec` does the fallback itself, so the tests were green.
   *Fixed by removing that code: whatever cannot be started directly goes
   to the recipe's shell. The wrapper tests now also run against the musl
   target, locally and in CI.*
2. (both) A compiler only a shell can resolve — `time gcc` under bash,
   `command gcc`, an exported shell function, `~` in `PATH` — failed with
   127 where the recipe works. *Fixed by the same rule.*
3. (A) A recipe with the compiler behind something else (`$(LAUNCHER)
   $(CC)`) was wrapped and every compile failed; the self-test passed.
   *Fixed twice over: the probe wraps only a reference in command
   position, and the wrapper is now run as a plain command with quoted
   arguments instead of an environment-assignment prefix.*

Non-blocking points taken: a user's own `MAKEFILES` entries survive below
object sub-makes (the fragment filters itself out instead of unexporting);
the self-test also checks `/proc/self/status`, and every build with the
cache on ends its compile step with a count, so a build the cache sat out
is visible; an empty rule for the fragment itself, so a forwarding
makefile's match-anything rule is not run for it; the rules file is read
strictly (one unconditional plain definition); the probe writes nothing
before it has decided, and runs after the clean step; the end-to-end test
no longer runs the test harness as "cactup"; spec, user docs and contract
brought in line.

Stated as limits rather than fixed, with the reason:

- The stand-down for a thorn's own compile recipe reads two files as text.
  A recipe defined in a file the thorn includes from them, or under a
  computed name, is not seen and loses to the fragment. No thorn in the
  Einstein Toolkit defines a compile recipe; following includes would mean
  re-implementing make. Spec §18.3 and contract A3 say so. **Max should
  know this one: it is the remaining way the cache could change what gets
  compiled, and accepting it is his call.**
- A compiler text that is not a plain command runs in a new shell, so it
  cannot use the recipe's own shell variables. Spec §18.4 says so.
- A terminal's signal reaches the compiler twice (terminal, then passed
  on). Harmless for compilers; spec §18.4 says so.
- The spelling hook rejects British spellings anywhere in a file it sees
  edited, so three pre-existing comment words in `src/build/mod.rs` were
  changed (Modeling, aging, afterward). Cactus's own name for its
  optimization option, spelled the British way, appears there too and
  stays.

### M0a, round 3 (on `6bdd9b8`): A SIGN-OFF, B BLOCKED

Both confirmed the round 2 blockers resolved. Reviewer B blocked on two
things, the second of which reviewer A had listed as non-blocking:

1. (B) The second self-test makefile ran none of its checks: `6bdd9b8` gave
   it a first rule (an empty rule for itself), which became the default
   goal. And its match-anything check could not fail, because make ignores
   a failed remake of a makefile. The fragment was sound on every make
   tried; the gate for untried makes was hollow. *Fixed: the script runs
   the goal `all` of each self-test and requires the `.passed` file each
   writes when its checks have all run; the forwarding rule leaves evidence
   that `all` checks; and a unit test runs the real probe step against the
   fragment broken in five ways (on make 4.2.1, 4.3 and 4.4.1).*
2. (B blocking, A non-blocking) A compiler name the recipe's shell would
   resolve to something other than the program on `PATH` — an exported
   function of that name, a `~` entry in `PATH`, a keyword such as `time`
   where `/usr/bin/time` exists — was started directly: a silently
   different compiler for object compiles only. *Fixed: such names go to
   the shell.*

Non-blocking points taken: the rules reader rejects `define NAME` followed
by anything, and the probe declines when the rules file includes other
makefiles; a `build/` that is a link is named by where it leads; a stop
signal that arrives just before a failed direct start is honored instead of
lost; the probe clears an earlier run's event log and self-test results;
command position excludes quoted text and continuation lines; a test pins
the probe step after the clean step; spec wording for compilers behind
another wrapper and for what the count covers.

One more stated limit, added to the list under round 2 (spec §18.4): a
function or alias the shell defines for itself at startup (bash's
`BASH_ENV`, which module systems set) is invisible to the wrapper. One
named like the compiler would run in the recipe and be bypassed by the
wrapper. Exported functions are caught; these cannot be without running
the shell. **This is the second known way, after the two-file stand-down
scan, that the cache could change what gets compiled; both need a setup
nobody is known to have, and both are Max's call.**

### M0b, round 1 (on `f8d632e`): BLOCKED by both

Reviewer A found nine, reviewer B five, ways for two compiles to share a
key and produce different objects, each reproduced with GCC 14.2 or Clang
19.1 through the real wrapper. Record mode's own promises held (nothing
added to the compile; objects identical to a plain build).

1. (both) Whitespace: the preprocessed text collapses spacing, but debug
   information records columns, and `__builtin_COLUMN` and
   `std::source_location` put them into code. (A) Clang's debug information
   also changes with a comment or with text in `#if 0`. *Fixed: the bytes
   of every file read are in the key.*
2. (A) `-C` made a directive behind a same-line comment into text, so the
   header it includes was not read. *Fixed: no `-C`.*
3. (both) Flags admitted by prefix: `-grecord-command-line`,
   `-gembed-source` passed as "debug levels". (A) `-x` after the source was
   keyed as if it applied. *Fixed: levels listed by name; `-x` not keyed.*
4. (both) The path map did not reach everything: sanitizers and Clang's
   OpenMP embed unmapped paths. *Fixed: sanitizers not keyed; Clang with
   OpenMP keeps the installation's paths in the key.*
5. (B; A as non-blocking) The map replaced whole path components, the
   compilers replace string prefixes (`Cactus-libs` beside `Cactus`); (A)
   and not only at the start of a path. *Fixed: maps end in `/`, the key
   applies them as the compiler does.*
6. (both) Inputs in no part of the key: `.incbin`, and a `.gch` beside a
   header. *Fixed: both detected, such compiles not keyed.*
7. (A blocking, B non-blocking) Clang with `-g`: the working directory is
   in the object and was not in the key. *Fixed: keyed with debug
   information, with `PWD`.*
8. (A) GCC's on-disk `specs` file was not part of the compiler's identity.
   *Fixed.*
9. (A blocking, B non-blocking) Relative `PATH` entries: the compiler
   identified could be another file than the one that ran. *Fixed: found
   as `execvp` finds it, and the identified file is the one started.*

Non-blocking points taken: "the last matching map wins" was checked on one
Clang only (now tried per compiler, as part of a trial of the whole map);
Clang's identity ignored the name it is run by and included its install
path, and missed its configuration files; `stepping` and cache sizes in the
platform; a compiler that fails identification was examined again on every
compile (the answer is remembered now); a stop signal during the check
after the compile was waited out (the wrapper now ends at once — and the
first version of that fix hung, because the preprocessor's back end kept
the pipe open; a test with a header that is a FIFO pins it); `cache
report` compared an attempt with itself, skipped unreadable lines silently
and printed half a report before an error; a failed direct start in record
mode left no line in the log; `-g3 -g` was read as level 2; spec §18.5 now
lists every residual it knows instead of naming the environment as the one
weak part; the evidence above was regenerated with one revision.

### M0a, round 4 (on `300fd0b`): SIGN-OFF by both

Both confirmed every round 3 finding resolved, on GNU make 4.0 to 4.4.1,
on the glibc and the static musl build, and (reviewer B) on a real Cactus
tree under make 4.3. Both confirmed the two limits stated for Max are
described accurately, reproduced the `BASH_ENV` one, and know of no third
way the cache could change what gets compiled.

Non-blocking points left open, taken up in M0b (none changes what gets
compiled; see "What M0b is"):

- Under bash the compiler's environment has `_` naming cactup instead of
  the compiler (bash sets `_` for each command it starts). Set it when the
  wrapper resolves the compiler's path, which M0b needs anyway for the
  compiler's identity; until then spec §18.1 rule 3 is off by this one
  variable.
- A `SIGPIPE` ignored on entry reaches the compiler at its default (the
  Rust runtime and `std::process` both touch it). Reachable only by running
  the frozen build script by hand from a parent that ignores `SIGPIPE`.
- Any `PATH` entry beginning with `~` sends every bare compiler name to the
  shell, also under a shell that does not expand `~` and when the entry
  comes after the compiler's directory. Safe, but it costs such a user the
  cache with no reason given. Narrow it to bash-like shells and to entries
  that precede the compiler's directory, and make the debug line and the
  "no compile recorded" line say why.
- The self-test does not try the `CCTK_TARGET` half of the fragment's guard
  on its own (a third run, from `build/` without `CCTK_TARGET`), and a
  machine `make` command carrying `-i` would pass anything.
- A recipe with the compiler on a continuation line right after `;` is not
  wrapped (fail-open; contract A1 says so). Plausible reformatting on the
  build-speed side.
- The `BASH_ENV` limit is really "anything the shell sets up for itself at
  startup that changes how it looks a name up" (also `hash -p`, another
  shell's startup file); say so in spec §18.4.
- `the_selftest_passes_the_real_fragment_and_fails_every_broken_one` runs
  only the `make` on `PATH`; run it over `CACTUP_TEST_MAKES` too.
- `SHELL_WORDS` lacks a few builtins (`bind`, dash's `chdir`, zsh's and
  ksh's). None is a plausible compiler name.

### M0b, round 2 (on `2ffce06`): BLOCKED by both

Both confirmed every round 1 finding resolved, the tests green on glibc
and musl, the real builds' objects identical to a plain build, and the
audit test not vacuous. Each found two new pairs of compiles with one key
and two objects (one of them the same):

1. (both) Clang writes a file name in a line marker as a C string, with
   octal for a tab or a byte outside ASCII; the reader undid only `\\` and
   `\"`, took the misread name for a file that is not there, and keyed it
   as absent — so a header under `bibliothèque/` was not in the key.
   *Fixed: every C escape is undone and an unreadable name is an error; a
   file the preprocessor entered must be readable, or there is no key;
   only a name no marker enters (a `#line`'s) may be absent, and only
   absent.*
2. (A) `-frandom-seed=<path under the tree>` was mapped in the key, but GCC
   records its command line in debug information, unmapped. *Fixed: only
   the values of `-isystem`, `-iquote`, `-idirafter` and `-include` are
   mapped (the audit now tries each); everything else is keyed as
   written.*
3. (B) Flags that reach a compiler from behind its command line never pass
   the reader: a Clang configuration file adding `-fopenmp` or
   `-grecord-command-line`, or including another file; and
   `CCC_OVERRIDE_OPTIONS`. *Fixed: a Clang that reads a configuration file
   and a GCC with a specs file on disk are not cached; the override
   variables make a compile not cached.*

Non-blocking points taken: `-mcpu=native+ext` (the prefix is matched now);
`-imacros` (not keyed: the preprocessor's output does not name the file);
the audit asserted that every compiler relocates (it now requires only
that keys agree exactly where the wrapper says they can, and that some
compile did); the map's flags for the trial come from the code that makes
them for a key; the report's note under "against" said "this build's log";
spec §18.5's list gained the textual `.incbin` check, files that appear
during an attempt, and flags from behind the command line.

Left open, with the reason: (B) each keyed compile reads about 110 files
twice on top of two preprocessor runs, and a per-attempt memo of file
digests by size, change time and inode would save most of that. It would
also trust a file's change time where the check after the compile now
reads its bytes; that trade wants the M0c numbers on a network filesystem
first.

### M0b, round 3 (on `b8ceb51`): A SIGN-OFF, B BLOCKED

Both confirmed the round 2 blockers resolved and accepted the stated
limits (the textual `.incbin` check, steering files that appear during an
attempt, the deferred file-digest memo) as accurate and non-blocking.

1. (B, blocking) Whether Clang reads a configuration file was asked once,
   by a bare `clang --version`. Clang picks the file by target, so
   `i386-pc-linux-gnu-clang.cfg` is read by `-m32` compiles only; and a
   first compile with `CLANG_NO_DEFAULT_CONFIG` set left an answer that
   later compiles without it reused. *Fixed: every compile's own
   preprocessor run is given `-v`, and the driver says whether it read a
   configuration file (Clang) or its built-in specs (GCC); the variable is
   part of what the remembered identity depends on.*

Non-blocking points taken: (A) a header whose own name begins with `<`
was taken for a pseudo-file (the compilers' pseudo names are matched
exactly now); (A) the trial of the map also uses `__builtin_FILE()`; (B)
the configuration-file test skips where a copy of the Clang driver cannot
run. (A, B) Both raised the cost of rejecting every GCC with a specs file
on a Spack-based cluster: see below.

### M0b, round 4 (on `045eb76`): SIGN-OFF by both

Both re-ran their earlier experiments (every hole found in rounds 1 to 3
still closed), confirmed that `-v` leaves the preprocessor's output
unchanged (four drivers, up to eight flag sets each), that a configuration
or specs file appearing during an attempt is caught by the next compile,
that 2.7 MB of preprocessor warnings do not stall the wrapper, that the
tests pass on glibc and musl with three makes, and that the real builds'
objects are byte for byte those of a plain build. Neither found a new pair
of compiles with one key and two objects.

Non-blocking points left open, to take up at the start of the next code
milestone (none can make a key wrong; each costs hits or robustness):

- (both) A GCC that prints its messages in another language never says
  "Using built-in specs." in those words, so every compile is declined,
  with a reason that does not mention the locale. Run the preprocessor
  with English messages if that can be done without changing how it reads
  the source, or say so in the reason and the user docs.
- (both) For Clang, silence on stderr reads as "no configuration file";
  for GCC it does not. Require a line Clang always prints under `-v`
  (`InstalledDir:`), so that a lost answer declines for both.
- (both) A GCC with any `specs` file on disk is not cached (next
  paragraph). Settle before measuring on a cluster with a Spack-built GCC.
- (B) The per-attempt memo of file digests (see round 2).

Known cost of the round 2 fixes, to measure in M0c: a GCC with a `specs`
file on disk is not cached at all. Spack-built GCCs have one (Spack writes
the library search path for `libgcc` into it), and many cluster compilers
are Spack-built. A refinement that stays sound: accept a specs file whose
every section other than the link ones is byte for byte the built-in one
(`gcc -dumpspecs`) and that includes no other file. Not done yet, on
purpose: it is new ground for a review round, and whether it is needed
shows in `cache report` on a real cluster ("reads a specs file").

### M0c (on `5411e68`): BLOCKED by both, on the results document

Both found the code changed since M0b's gate sound (dependency output:
same key and same object with and without the flags, four drivers, every
accepted spelling, and no preprocessor run touching the file; English
messages: the preprocessor's output unchanged across seven locale setups;
reviewer B confirmed with a German GCC message catalog that a translated
GCC is now keyed), and both checked the results' numbers against the logs
and found them right. Both blocked on the same sentence of the summary:
it said a serving cache pays about half the recording cost on a miss. A
miss pays all of it; a hit pays about half and does not compile. *Fixed.*

Also corrected in the results: 275 thorns, not 278; the wall-time
overhead is 6 to 9% from single runs; the dependency-file check was made
with a debug build of the working tree (its hash lists are now kept);
which attempt `smoke` means; what "objects untouched" does and does not
cover; a forced rebuild is in the table and an option list edit is said
to be unmeasured; Fortran's share is given for both builds.

Non-blocking code points taken (B): `-MF` as the value of `-MT`/`-MQ` no
longer counts as naming the file; an empty `LC_ALL` is no `LC_ALL`;
Clang's answer must reach the compiler proper's command line, since it
names a configuration file after `InstalledDir:`; the dependency-file
test also checks that nothing wrote the file after the compile.

### M0c, round 2 (on `13c7471`)

Reviewer B: the results document is right now, the code fixes hold; one
blocking finding, in a test. The assertion added in round 1 (the
dependency file is no newer than the object) assumed the compiler writes
the object last. Clang writes the dependency file last, so the test
failed 3 runs in 40. *Fixed: the time check is for GCC only, and a unit
test pins the preprocessor's command line (both runs are that command)
to carry none of the dependency flags.* Also taken: the compiler is asked
for its identity in English too (B showed the compiler part of the key
depended on the language of the first session to ask); `runs_cc1`'s
comment names both lines that satisfy it; `fresh-env.sh` says it was
written down after the runs.

Reviewer A: SIGN-OFF on `13c7471` (results right, code holds, tests green
on a clean export of the commit), with the same remark on `runs_cc1` and
a note that the dependency-build hash lists do not show their own
provenance (the results now say so).

### M0c, round 3 (on `129ecf7`): SIGN-OFF by both

Both confirmed the dependency-file test stable (60 of 60 runs each, also
under load and on musl; A reproduced the old failure and showed, by
breaking `preprocessor()` in an export, that the new unit test and GCC's
time check both catch a dependency flag reaching the preprocessor), the
refactoring a pure move (A recorded every driver run's arguments and
environment before and after: identical), and the compiler identities
unchanged in an English session and equal in a German one (B, with a
German GCC catalog mounted: the identity that used to differ no longer
does; A has no catalog and could only confirm "no difference").

Non-blocking points left open, for the start of the next code milestone:

- No test pins that the compiler is asked for its identity in English; a
  later edit could drop it unnoticed (A).
- A doubled blank line after `in_english` in `src/objcache/key.rs` (both).
- Carried from M0b's round 4 and still open: the specs-file refinement
  for Spack-built GCCs, and the per-attempt memo of file digests.

`PLAN.md` still lists `-MD` as not cached: it is the plan as approved,
and its first lines say where the work has moved since.

### M1a, round 1 (on `7099249`): BLOCKED by both

Both reviewers ran the suites (green on glibc with three makes and on
musl), checked the pattern against every `make.code.*` file in two real
trees (no false stand-downs), and the `LINK_ONLY` list against GCC 14's
driver strings (right). Blocking:

1. (both) The stand-down pattern wanted a space after `include`,
   `define` and `eval`; make joins `\`-newline into a space first, so
   `include\` + newline + file, and `$(eval\` + newline + …, are real
   directives the pattern missed: a thorn's own recipe silently replaced,
   reproduced on make 4.2.1, 4.3 and 4.4.1. *Fixed: a `\` counts as the
   space; the cases are in the unit test and the under-make test.*
2. (both) Asking the shell read its output from a pipe to the end, so
   something a `BASH_ENV` file started in the background (`sleep 20 &`)
   held up the compile (20 s against 2 ms; forever for a daemon).
   *Fixed: the shell's output goes to a file and only the shell is waited
   for; a test with `sleep 30 &`.*
3. (A) A specs file that leaves sections out was accepted, but GCC does
   not set up built-in sections when it reads a specs file: one with only
   `*link_libgcc:` lost `cc1_cpu` (`-march=native` failed), an empty one
   made GCC write no object, and both were keyed. *Fixed: the file must
   define every section `-dumpspecs` prints.*
4. (A) Nothing tied a key to what cactup does to the compile: the path
   map's flags were not keyed (only "mapped"), and no rule said when the
   key's label changes, though several cactup builds share one store.
   *Fixed: the option and the names are keyed (`PathMap::description`),
   `KEY_LABEL` (now `key-3`) carries the rule, a test pins the key of
   fixed parts; spec §18.5 and `CLAUDE-contract.md` state it.*

Non-blocking points taken:

- (both) An entry whose header failed to parse was removed, so two cactup
  builds could delete each other's good entries. *The blob lengths moved
  to a line of their own; size and checksum are checked before the
  header is read; a whole entry with a header this cactup cannot read is
  `Miss::Foreign` and left alone; a `FORMAT` rule is stated.*
- (both) The specs reader split sections after an empty one differently
  from GCC (which skips blank lines after a name): refused now unless two
  blank lines follow.
- (A) bash and zsh run a function named by a path; `command -v` prints
  the path for it. *The shell is now asked `type` behind a marker line, in
  English, for path names too; a test with a path-named function.*
- (A) A startup file's output without a line end could corrupt the
  answer: the marker. (B) zsh's `.zshenv` files are watched for zsh.
- (B) `load` and `$(guile` stand the fragment down too. (A) `grep`'s own
  messages are discarded.
- Store details: the restore's temporary file is created with `0666` and
  left to the umask and default ACLs (no more reading `Umask:`); sync
  before `chmod 0444`; a non-file at an entry's name is invalid (no
  blocking on a FIFO) and fails a publish; an object that changed while
  it was copied is not published; `Miss::Invalid` says whether it removed
  anything; 1 MiB buffers; a concurrent test with invalidation and
  republication.
- Wording: the log line no longer says "the recipe's shell" twice; the
  docs say "source directory".

Stated as limits rather than fixed (spec §18.4): the ask is a new shell
run with `-c` (`.SHELLFLAGS` is not passed, as for the shell the wrapper
hands a compile to); the remembered answer does not depend on the working
directory (unless `PATH` is relative), on variables other than those
listed, or on what a `BASH_ENV` value expands to. Spec §18.7 now says
that an interrupted restore leaves a `.cactup-` file in `build/`.

### M1a, round 2 (on `916b0cb`): B SIGN-OFF, A BLOCKED

Both re-ran their round-1 reproductions: every blocking finding resolved
(the continued directives on three makes; 8 ms against 20 s with a
background job in `BASH_ENV`; partial and empty specs files refused; the
key's label and description in place). Both found no new defect in the
code and accepted the stated limits.

1. (A, blocking) Tests failed now and then with "Text file busy" (3, 2 and
   1 of 40 runs at 32 threads, two of the tests new in M1a): a test wrote
   an executable and ran it while another thread's fork held the write
   handle. *Fixed: `objcache::make_executable` (and its twin in
   `tests/objcache.rs`) has `install` write the file that runs, so no
   write handle to it is ever in the test process; every test executable
   in the cache's tests and in `src/build/mod.rs` goes through it.*

Non-blocking points taken: (B) the key's label is recorded in an entry's
header, and a whole entry under another label is foreign (for `cache
verify` and `gc` later); (B) a specs file with `#` or a line-final `\` is
refused, since GCC reads such text otherwise than `-dumpspecs` prints it;
(B) the limit of a startup file defining `type` or `printf` is stated;
(A) the decline message for a specs file reads in one sentence; (A) the
background-job test kills its `sleep`.

### M1a, round 3 (on `9d8f62b`): SIGN-OFF by both

A ran the unit-test binary 120 times at 32 threads: no failure in any
cache test (6 in 40 before the fix). B confirmed the label, the specs-text
rule and the stated limit by running them in an export. Both read §18.5
and §18.7 against the code: they match. M1a has passed its gate.

Non-blocking points left open, for the start of M1b:

- (A, B) No test pins the label case of `Miss::Foreign` (B tried it in an
  export: `key-2` and `key-9` both left alone). Add it to
  `a_whole_entry_with_a_header_of_another_cactup_is_left_alone`.
- (B) Adding `label` to the header did not bump `store::FORMAT`. Harmless
  while nothing publishes; from M1b on the rule binds, so the first entry
  published by a release is format 1 as it stands at `9d8f62b`.
- (B) `it_dies_of_the_signal_that_stopped_the_compiler` assumes the test
  runner does not ignore `SIGQUIT` (a background job of a non-interactive
  shell does): clear the disposition or skip.
- (A, outside the cache) `build::tests::source_tree_changes_are_a_rebuild_input`
  failed once in 120 runs with exit 126: inferred to be the same "Text
  file busy" race in product code, `sim::start::write_executable` writing
  the build script and running it at once. Predates M1a; it is master's,
  and worth a fix of its own (retry on `ETXTBSY`, or run a `#!` script
  through its interpreter). Reported to Max, who asked for the fix:
  `script_command` now runs a `#!` script through its interpreter, as
  the kernel would (`<interpreter> [<one argument>] <script>`), so the
  script is only read, never exec'd. Its own commit on this branch, made
  to be cherry-picked to master (`git log --grep 'run a stored script'`).
  Confirmed: `sh -c '<script>'` with a write handle open is exit 126,
  "Text file busy"; the new command runs it; 60 runs of the unit tests at
  32 threads, no failure.

### M1b, round 1 (on `58fba3e`): B BLOCKED; A did not report

Reviewer A was cut off by the account's usage limit before reporting;
a fresh reviewer A takes round 2 with the full brief. Reviewer B checked
the dependency file on a hit (byte for byte, gcc, g++, clang, clang++, six
flag spellings), audit mode's wrong hit, decision 5 across a UTF-8 and a
C session, `-Werror` through the preprocessor run, a stop signal during a
miss; all sound. The tests were green. Blocking:

1. (B) A wrong hit across locales: `-fexec-charset=ASCII//TRANSLIT`
   follows the locale (`cafe` in UTF-8, `caf?` in C), the locale trial
   compiles without charset flags, so the locale left the key; a C session
   was served a UTF-8 session's object, and audit mode called it a wrong
   hit. *Fixed: a compile with `-finput-charset=`/`-fexec-charset=` keeps
   the locale in its key (`Compile::charset`); key label `key-5`; a
   two-session test, which fails on `58fba3e`.*
2. (B) A miss could lose the compiler's messages: the threads passing them
   on were given two seconds after the compiler ended, also while they
   were still writing to a slow terminal (175 KB said, 128 KB arrived).
   *Fixed: reading and writing are separate threads; only the reading
   waits for the streams to close (two seconds, for a process the
   compiler started that keeps one open, which now has its own reason);
   the writing is waited for to the end. A slow-reader test, which loses
   bytes on `58fba3e`.*
3. (B) The dependency file of a hit was mode `0600` (`tempfile`'s), where
   the compiler's is `0666` less the umask; make silently skips a `.d` it
   cannot read, so in a group-shared configuration another user's build
   would lose that object's header dependencies. *Fixed: the temporary
   file is created `0666` and left to the umask and default ACL; the test
   compares modes, and fails on `58fba3e`.*

Non-blocking points taken: the last `-MF` is used, `-MF -` is declined;
audit's second compile is not shown again; a wrong entry is removed and
the fresh object published in its place; the closing line counts audited
hits whose compile now fails; messages are rewritten only where a path
begins (a unit test); spec §18.8 names the temporary files a signal can
leave; tests for a failing compile through the pass-through and for
`relocate = false`; `interpreter_command` reads a `#!` line as the kernel
does (a carriage return stays, a relative interpreter is a path from the
working directory, the first line is read as bytes).

Not taken, for Max (below): the advice for debuggers. A relocated object
records its compile directory as `./configs/@config/scratch` and its
source directories relative to it, so a debugger resolves its sources to
`./configs/@config/scratch/./arrangements/...`; the one-line gdb
`substitute-path` the docs gave probably does not work, and this host has
no debugger to find one that does. The docs now point at
`build-cache-relocate no` for a build meant for debugging.

### M1b, round 2 (on `6a283ef`): BLOCKED by both, one finding each

Both confirmed every round-1 finding resolved (B re-ran its three: the
TRANSLIT sessions now miss, 176774 of 176774 bytes reach a 20 KB/s
reader, the `.d` modes match under umask 002, 022, 077 and a default
ACL). Reviewer A (fresh this round) also checked a symlinked root, the
`.d` file under every flag the key run adds, signals on a serving miss,
and `interpreter_command` against the kernel. Blocking:

1. (A) **A file changed and changed back during a serving miss published
   a wrong object**, which another installation was then served (`v = 2`
   where its own compile gives `v = 1`). Spec §18.5 listed this as a
   limit, which in record mode cost nothing; with a shared store and no
   eviction it is a lasting wrong object. *Fixed: the key also records,
   for each file it read, device, inode, size, modification and change
   time (of the name and of what it leads to; `Read::seen`, not in the
   key), and the check after the compile requires them unchanged. User
   space cannot set a change time back. A unit test rewrites a header and
   puts its bytes back: the check fails.* Left as a stated limit: a
   filesystem with change times coarser than the edits.
2. (B) `an_invalid_entry_replaced_meanwhile_is_not_removed` failed on ext4
   (91 of 100 runs with `TMPDIR` there): ext4 gave the republished entry
   the freed inode. *Fixed: the test keeps the old file alive under
   another name; 30 of 30 on ext4.*

Non-blocking points taken (A): audit verdicts only for compiles whose
inputs held still (`inputs changed` otherwise), a second check after the
second compile, a stop signal during it ends the wrapper by that signal;
audit compares the dependency file a hit would have written
(`wrong dependency file`); paths after a terminal color sequence are
mapped in messages; `-MT -MF` and the like are read as flag and value;
pipes and their threads are made before the compiler starts, and a thread
that cannot be made leaves the streams inherited instead of costing the
compile; outcomes and verdicts are enums (`event::Outcome`,
`event::Audit`); the closing line counts what could not be published and
inputs that changed; `cache report` says "found in the cache"; the
charset test skips on a host without `C.UTF-8`; the signal test runs in
serve mode too and checks nothing was published; `script_command` reads
only the first line; spec and docs wording (identifier, `CCTK_WARN`
names).

Decision 8 (Max, 2026-10-07): absolute placeholders. The root maps to
`/cactup-root/`, the configuration to `/cactup-root/configs/@config/`; key
label `key-6`; spec §18.5, §18.8 and the user docs give `set
substitute-path /cactup-root /path/to/Cactus` (not tried: no debugger on
`plato`).

Not taken, as acceptable to both: messages buffered without limit for a
stalled terminal (compiler messages are small); a path glued to a flag in
a message is not mapped.

### M1b, round 3 (on `efd6994`): BLOCKED by both, one finding each

Both re-ran every earlier reproduction (A: `changeback2.sh` now unstable
and unpublished, a file moved aside and back too; 150 audited compiles
under a flipping header, no false verdict; B: the TRANSLIT sessions, the
slow reader, the `.d` modes, a header flipped while cc1 ran) and found
them resolved. Blocking:

1. (A) The check watched each file's own name only: an include directory
   that is a symlink, switched to another tree and back during the
   compile (as Spack views and `current` links switch), passed it, and the
   wrong object was served to another installation. *Fixed: every entry a
   keyed name resolves through, directories and symlinks, followed as the
   kernel follows them (`key::trail`, memoized per run), by device, inode
   and birth time, a symlink also by change time and target; the file
   itself by `fstat` after the open (B's NFS point). A unit test switches a
   symlinked include directory and back. The test first passed on tmpfs and
   failed on ext4, which gives the new symlink the old inode number at
   once: hence the birth and change times; 30 of 30 on each since.* What
   is left, a directory renamed away and the same one renamed back, is
   Max's decision 9: a stated limit (spec §18.5).
2. (B) Audit mode failed the build and deleted the good object when its
   second compile died by any signal (`kill -9`, an OOM kill, a crash).
   *Fixed: only a stop signal that reached the wrapper (`PENDING`) ends it
   by that signal; otherwise the first object is put back and the verdict
   is `second-compile-failed`. Tested with a second compile that kills
   itself (a debug-only hook, `CACTUP_CC_TEST_SECOND_COMPILER`).*

Non-blocking points taken: (A, B) an entry another build published first
is no longer counted as "could not be published"; (A) the gdb recipe gives
the configuration's rule first (Cactus compiles the copies in the
configuration's `build`), here and in decision 8's wording; (A) the
dependency file of an audited hit whose compiler cannot be started is
removed; (A) the comment on reading a script's first line.

### M1b, round 4 (on `6b8efb1`): SIGN-OFF by both

A re-ran the switched include directory four ways (a new link renamed
over the old, `ln -sfn` in place, `rm` and `ln -s` with the inode number
reused on ext4, a path through `..`): every one fails the check and
publishes nothing, while the unchanged tree's compile still publishes. B
re-ran a second compile killed by `kill -9` (wrapper exits 0, the good
object kept, `second-compile-failed`) and a stop signal to the wrapper
during it (dies by the signal). Both reviewed `trail` (symlink targets,
`..` on the physical path, the 40-link cap, relative names from the
working directory, a memo per run) and found it sound; B counted its
cost: 342 entries looked at for a compile reading 295 files, against 590
stats before, so about even (to be measured on NFS).

The review half of the M1b gate is passed. Non-blocking points left open,
for the start of M1c:

- (A) The audit closing line does not count `second-compile-failed`
  (`cache report` lists it).
- (A) A stop signal that reached the wrapper while audit's second compile
  ended by a status, not by the signal, is lost (GCC's and Clang's drivers
  re-raise it, so this is a corner): check `PENDING` however the second
  compile ended.
- (A, B) On a filesystem with no birth times (NFS, typically), a directory
  replaced by one that reuses its inode number is not seen: add it to spec
  §18.5's limits beside decision 9.

The other half of the gate is Max's: the full audit build in two
installations, Clang included, on `6b8efb1`. *Done (2026-10-07, binary
`~/tmp/build-cache-m1b/cactup-6b8efb1`, store `store-et-4`, results in
`gate-4.out`): the Einstein Toolkit, 3376 compiles each. GCC: serving into
an empty store in `build-cache` published 2782; audit in `build-cache-b`
and in `build-cache` itself checked 2781 each, 0 wrong, 0 failing to
compile, 0 not deterministic, 0 with inputs that changed. Clang: 2782
published, audit in `build-cache-b` checked 2781, 0 of each. Every build
exited 0. M1b has passed its gate.*

### M1c, round 1 (on `3011fff`): BLOCKED by both, on the same two

(The author edited `DECISIONS.md` and `STATUS.md` while the reviewers
worked, against the rule above; documents only, but not again.)

1. (both) `gc` read a use log it could not read as empty, removed the
   entries that log recorded as used, and deleted the log; and the logs
   were written `0600`, so in a store shared with a group every member's
   `gc` would do so with the others' logs (both ran it: a copied store,
   400 entries in a log, `chmod 000`, all 2783 removed). Listing errors
   were swallowed too. *Fixed: logs, the folded log and the size stamp
   are written `0666` less the umask; every directory and every log must
   be read whole, or the walk fails and `gc` removes nothing (a unit test,
   and run on a copy).*
2. (both) The directories of other formats were walked file by file, with
   no progress and no Ctrl-C (A: 400k files, the Ctrl-C seen only at the
   end), by `stats`, `gc`, `verify` and builds. *Fixed: they are named,
   never walked, and `stats` says to remove them by hand; spec §18.7's
   "left for `cache gc`" corrected.*

Non-blocking points taken: a build never walks the store (the size stamp
plus what the build published, an estimate; `stats` and `gc` measure);
`gc` removes an entry only if device, inode, size and modification time
are still what it walked; `verify` counts as it goes and says what it
removed when interrupted, and an entry it could not remove is counted
apart; `stats` works on a store it can only read; `parse_age` cannot
overflow; `--to-size` alone; wording ("objects", "all stored today",
human sizes when interrupted); a unit test of `after_build`.

New in the same commit, by Max's choice (decision 11): the machine's
place for the cache, `[paths] build-cache-home`, between the user's knob
and `$CACTUP_HOME/cache`; set on 19 machines beside their per-user
`simulation-home` on scratch or work; MDB generation 2 (`mdb/GENERATION`,
`GENERATIONS.md` with the overlay recipe), all in one commit as
`CLAUDE.md` asks.

### M1c, round 2 (on `feffae1`): BLOCKED by both, on the same one

1. (both) `build-cache-home` was resolved with the other `[paths]`, all
   or nothing, so a machine whose value named an `@ENV(…)@` unset on the
   host failed every build, simulation and install there, cache off or
   not. *Fixed in `4e51d03`: resolved alone and leniently, a warning and
   the next place.*
2. (B) A use log whose `stat` failed after it was listed was skipped, not
   counted as unreadable. *Fixed in `4e51d03`: it stops the walk too.*

Non-blocking points taken: `stats` and `verify` do not fail on
unreadable logs (only `gc` must).

Then, before round 3, Max moved the cache (decision 12): its default is
`.cactup-build-cache` in the install home, beside the installations, and
every `[paths]` key has a knob of the same name (`build-cache-home`
replaces `build-cache-dir`). The 19 machines' `build-cache-home` and MDB
generation 2 are gone again (`mdb/` is as it was before `feffae1`); the
schema keeps the optional key, which no machine uses.

### M1c, round 3 (on `01bd00a`): SIGN-OFF by both

Both re-ran the round-2 blockers and found them fixed (an unresolvable
machine `build-cache-home` warns and goes on to the install home; an
unreadable or unlistable use log stops `gc`, with nothing removed), and
both confirmed that `mdb/` is unchanged and no generation bump is due.

Non-blocking points taken after the gate: the
`Paths` doc comment names `.cactup-build-cache`; the docs say a store
left behind by a new `install-home` or `build-cache-home` is moved or
removed by hand. Left as they are: the removed `build-cache-dir` knob is taken as a custom
knob like any unknown name (both; it was never released, and Max: "The old
knob name fallback is pointless. None of these changes have ever been
live."); `@SCRATCH_HOME@` read at each submit
(as a machine edit would be); the machine-detection notice in the cache
commands on an unknown host. (B) noted that machines whose `install-home`
is `$HOME` keep the store there: decision 12, Max's.

### M2a, round 1 (on `4fa357e`): BLOCKED by both

Both reproduced wrong hits through the wrapper with legal sources and flags
the Einstein Toolkit does not use; audit mode would have caught each.

1. (both) The dependency run's preprocessor read the source otherwise than
   the compile: `-D` given to it (`-Dmymod=othermod` named another module
   file), `_REENTRANT` (and `_OPENMP`) defined under `-fopenmp` (B: `use
   pm _REENTRANT` in fixed form), a line ending in `\` with blanks after
   it (B: a comment `! see C:\  ` hid the next line's `use`), a lone
   carriage return, trigraphs. *Fixed: `-D`/`-U` are given to no run that
   preprocesses; the preprocessor is run once more with `-E`, and its
   output, line markers aside, must be the source; a `#` line that is not
   a line marker still keeps a source out (it would leave nothing to
   compare). All 594 Einstein Toolkit Fortran build copies pass.*
2. (both) Search order: gfortran looks in the directory of the file it
   reads before any `-I` directory, for module files too, so the
   dependency run found a module file beside the source before the
   working directory's, which the compile finds first. *Fixed: every
   module file the run read must be the first of its name in the
   compile's order (working directory, the compiled file's directory,
   the `-I` directories); checked again after the compile.*
3. (both) An included file found below the working directory
   (`sub/vals.inc`, `../x.inc`) slipped past a check of its parent only.
   *Fixed: none may be found under the working directory, by components,
   as named and as resolved.*

Non-blocking points taken: the copy is checked again after the compile
(and the source with it, both outside the key, so that a build that
records keys as one that serves); record mode writes no copy; the
dependency runs read the source itself; a copied source's included file
that the copy's directory has too keeps it out; a renamed compile whose
messages cannot be passed through the wrapper runs as the recipe gave
it; the fallback homes are written once (`mdb::meta::fallback_home`); the
spec says how module files are restored and what a signal leaves; the
docs and decision 13 say what "In file" messages name. The wrong premise
of the "module file beside the source" decline went with it.

### M2a, round 2 (on `e9b0a33`): A SIGN-OFF, B BLOCKED

Both re-ran every round-1 reproduction: each is now a correct miss or
declined with its reason.

1. (B) An included file named like a module file (`include "cfg.mod"`, a
   text file) was taken by its name for a module file, and so could be
   found in `scratch` by the dependency run, which the compile does not
   search for included files. *Fixed: a module file is one by its name and
   by being gzip-compressed, as gfortran writes them.*
2. (B) For a copied source, a file of an included file's name in
   `.cactup/` (a copy whose build copy is gone) was checked only when the
   included file was found in the source's directory; the compile, which
   looks in `.cactup/` first, read it, and an object of other text was
   published. *Fixed: any file of that name there keeps the compile out, as
   the spec said.*

Non-blocking points taken: the module comment and `key::key`'s argument
(`serving`) say what they now mean; audit mode's second compile, should
its messages not be passable, runs with them discarded rather than as the
recipe's; the spec says `-D`/`-U` are kept from the preprocessing runs
only, that CRLF sources are not cached, and that `.cactup/` is safe to
delete. Left as it is (A): an included file an external library installed
below `scratch` is declined, a lost hit only (the spec says why).

### M2a, round 3 (on `6a55842`): BLOCKED by both, one finding each

1. (A) Round 2's fix decided by content: a `.mod` file that is not
   gzip-compressed was treated as an included file only, and so escaped
   the search-order check; but gfortran reads module files through zlib,
   which takes an uncompressed file as it is, so round 1's module file
   beside the source came back (A served `$0x1` where the compile gives
   `$0x3`). *Fixed: every file named like a module file gets the
   search-order check; one that is not gzip-compressed gets the
   included-file rules as well.*
2. (B) A copied source's `include "../x.inc"` resolves from `.cactup/`,
   which is the source's directory one level down: the compile read
   `build/<Thorn>/x.inc` where the dependency run read the file one level
   up. *Fixed: for a copied source, an included file named with `..`
   keeps the compile out (the dependency run prints the name as written).*

### M2a, round 4 (on `a14f279`): SIGN-OFF by both

Both re-ran every reproduction of the four rounds through the wrapper:
each is a correct miss or declined with its reason, and the cases that
should be cached (an uncompressed module file found only outside
`scratch`, a copied source with a plain include beside it) still are.
Non-blocking (B): the `..` check also fires for an `-I` directory spelled
with `..`, a lost hit only. The Einstein Toolkit gate is run once more on
`a14f279` itself (`gate-4.sh`), the commit the reviews signed.

**The Einstein Toolkit gate on `e9b0a33` (`gate-3.out`) and on `a14f279`
(`gate-4.out`)**, alike: `et.toml` served in `build-cache`, 3376 published;
audited in `build-cache-b` and in `build-cache`, 3375 checked each, 0
wrong; `et-ld.toml` served, 3376 published, audited in `build-cache-b`,
3374 checked, 0 wrong; every build exit 0. **M2a has passed its gate.**

### M3a, round 1 (on `064cb82`): BLOCKED by both

Both re-ran the lookup model against GCC 14, Clang 19 and gfortran 14 and
found it agreed with the compilers in every lookup case they tried (quote
and bracket order, `#include_next` from each kind of file, `-iquote`,
directories in the way, `.gch`, canonical names, skipped includes,
gfortran's include order and pre-include); no Einstein Toolkit compile fell
back. The holes were around it, each a case the second compiler run caught
and the check by lookups did not (all reproduced, A and B):

1. **`__has_include` the byte scan cannot see** (A, B): spliced across a
   line (`__has_\` newline `include`), made by pasting (`__has_ ##
   include`), behind a `??/` trigraph, or behind a C++ digit separator
   that put the comment scan out of step (`1'0, "x'/*"`). Fixed: the scan
   reads files with splices undone; a `??/` falls back; pasting falls back
   when an identifier that begins a watched name (and is not all of it)
   stands anywhere in what the compile reads; numbers are read as numbers.
2. **`#embed` and `__has_embed`** (A, B; B served stale objects end to end
   with Clang 19): no line marker names the embedded file. STATUS said they
   fall back; nothing did. Fixed: both fall back.
3. **An `-I` that is not a directory** (A, B): GCC leaves it out with a
   warning, not in the "nonexistent" lines; if it became a directory the
   compile would search it. Fixed: it is watched with the nonexistent ones,
   all of which must not be directories later (which also stops a dangling
   symlink, and Clang's "nonexistent" regular file, from failing every
   check).
4. **Output parsing** (A): a skipped `#import` was not parsed; a line in a
   raw string that looks like a returning marker could pop the stack. Fixed:
   `#import` falls back; a marker must return to the file the output was
   reading before, by the name it last gave it; each include is tied to the
   include that entered its file (not to a file name).
5. **`__DATE__`/`__TIME__`** (B): the second run failed when the clock's
   second moved on; `HTTPD/Content.c` was published by lookups. Fixed: they
   fall back unless `SOURCE_DATE_EPOCH` is set; `__TIMESTAMP__` always.
6. **gfortran's built-in modules** (B): a `use iso_c_binding` without
   `intrinsic` takes a module file of that name if one appears where the
   compile looks; the dependency run lists none. Fixed: no such file may
   appear where the compile looks for modules.

Non-blocking, also done: the spec and STATUS lists of what falls back
match the code (`__has_include_next` is answered, `#include_next` from the
source is modeled); Clang's `./` for a relative source (undone in round
2) and GCC with
`-ffreestanding` or `-nostdinc` no longer fall back every time; a pass
lists a directory with a name the listing lacks under another case by its
path (casefold directories); the key's `__has_include` answers and the pass
before the compile share one set of listings, and the check after the
compile one more; the log counts lookups by path (`lookups`) and directory
listings (`listings`) apart, for the check's passes only, Fortran's module
order lookups included. Not done: B's suggestion to drop the pass before
the compile (it is what ties a skipped include to the file the run had
entered before it); hits now pay the `__has_include` answers and the scan
(the audit builds' median key: g++ 90 to 109 ms, gcc 16 to 19 ms, on
`afa94e7`, before the faster scan of `b2f7c1d`).

### M3a, round 2 (on `9515bb4`): BLOCKED by both

Both confirmed every round-1 finding fixed in the form reported, and both
found the fixes incomplete or too broad (all reproduced; A and B each
served stale objects end to end):

1. **The pasting rule of round 1 sent nearly every C and C++ compile back to
   the compiler** (A, B): glibc's `sys/cdefs.h` has `__ ## f ## _alias`,
   libstdc++ has `__h` and `__has_` as identifiers. No bound short of a
   preprocessor tells those from a pasted `__has_include`. Asked, Max made
   such a name a stated limit (decision 15); the rule is gone.
2. **The scan still did not read every file as the compilers do** (A, B):
   a backslash then a lone carriage return splices for both compilers, and
   a lone carriage return ends a line; `??=` pastes (`a ??=??= b`) and
   makes `#` under strict ISO modes; in C17 a `'` after a digit begins a
   character literal; `auR"` was taken for a raw string; `#/* c */ embed`
   and `/* c */ #embed` are directives. Fixed: the scan now works on each
   file's code (lines spliced, each comment one blank, literals apart) and
   falls back on whatever two compilers or two language modes could read
   otherwise (a NUL byte, a lone carriage return, any trigraph, a raw
   string, a number with a `'`, C90); `#embed`, the date words and
   `__has_include` are looked for in that code.
3. **Fake line markers** (B): a raw string, a line marker in a source, or a
   macro that expands to `# 1 "file" 2` at the start of a line writes a
   line the tracker takes for the compiler's. Fixed: raw strings, line
   markers in a source, and any `#` that is no directive and no operator of
   a function-like macro fall back.
4. **The same physical file at another place** (A, B): `before_compile`
   took a name to agree with a lookup whenever both led to the same file,
   so a symlink appearing earlier in the search to the found header passed;
   and after the compile only the path strings were compared, so a symlink
   on the way to a system header named by its physical path, turned
   elsewhere during the compile, passed (A served a stale object). Fixed:
   only GCC's own rule is followed, for a header its marker flags as a
   system one and whose physical path is shorter, and after the compile the
   file found must still have that physical path. (Clang's `./` for a
   relative source falls back again; Cactus names sources absolutely.)

Non-blocking, done: the `__has_include` answers' digest has the search list
in it (positions alone did not say which directories; A noted that keys now
differ between search lists, which they must); `Looker`'s doc; the spec's
lists. Noted, not done: gfortran's pre-included header is found by the
driver's own search, which is not repeated (stated in the spec); the key's
own `__has_include` answers are not counted in `lookups`; the listing cost
on a cluster filesystem is for M3b's decision. STATUS's figures of the
smoke and Einstein Toolkit runs above are of `afa94e7`, before the round-1
pasting rule.

## Decisions

All of them, answered, are in `DECISIONS.md`. **Answered by Max on
2026-10-07 (decision 8), kept here for the record:**
should the path map name the tree with absolute placeholders (say
`/cactup-root/` and `/cactup-root/configs/@config/`) instead of `./`?
Then objects would record absolute, if fictitious, paths, and one
`set substitute-path /cactup-root /path/to/Cactus` would let gdb find every
source; with `./`, the paths are relative to a recorded compile directory
and no simple recipe is known. It changes the design choice of 2026-10-01
("objects then name files as `./arrangements/...`"), hence the question. Decision 3's check came
back on 2026-10-02 from qbd: a site-built GCC whose specs file adds an
rpath in a link section and changes nothing else, so the refinement for
specs files is needed and is taken into M1a. Spack-built GCCs were
checked on 2026-10-07 (mike, db1; decision 3): all five accepted.

## What M2a is

gfortran, cached. The facts it rests on were tried on this host's gfortran
14.2 first (2026-10-07), each with a throwaway source:

- Module files are deterministic: gzip with no time, and the source named
  by its file name alone ("created from x.f90", whatever path it was given).
  gfortran leaves a module file alone when it would not change.
- gfortran looks for a module in the working directory first, then in the
  `-I` directories. `-cpp -undef -M` lists every file a compile reads,
  module files by the name they were found under (intrinsic ones by full
  path), include files, the pre-included header, and the module files it
  writes as targets; it writes those module files too (hence `-J` into a
  directory of its own), and `-E` lists no modules. Without `-c` the
  driver reads `libgfortran.spec`, as for a link; `-fsyntax-only` does not,
  and compiles nothing.
- In traditional mode the preprocessor swallows code between a `/*` in a
  Fortran comment and a later `*/`; after `-undef` it still defines names
  that all begin with two underscores. None of the Einstein Toolkit's 594
  Fortran build copies has `/*`, a line-final `\`, a `#` line or such a
  name (they have names with `__` inside, which the preprocessor reads as
  whole names).
- The runtime-error string is the path the compiler was given or the one a
  line marker names, and no flag remaps it (decision 13). A copy under the
  source's own name in a directory of its own, `# 1 "<mapped name>"`
  first, the copy's directory mapped last, the source's directory first in
  `-I`: two trees give byte-identical objects and module files.

The Cactus runs on `85407d6` (binary `~/tmp/build-cache-fortran/cactup-85407d6`):
`smoke.th`, served into a fresh store in `build-cache`, audited in
`build-cache-b` and in `build-cache`, then the same with line directives
(`linedir.toml`): 357 compiles each, 50 of them Fortran, 357 published and
357 checked, 0 wrong, every build exit 0. The Fortran overhead on those
small files: keying 559 ms and checking again 442 ms against 1039 ms of
compiling, all fifty together.

**The first Einstein Toolkit run failed** (`gate.sh`, `gate.out`, on
`85407d6`): served, then audited in `build-cache-b` and in `build-cache`
itself, 63 Fortran compiles were wrong hits in both audits. Each build made
other objects of them, and each audit's two compiles the same: gfortran's
runtime checks write "In file '<the name it was given>', around line N",
and that was the copy in its private, randomly named directory. The small
tests had no runtime checks. Five more compiles were never published:
their source defines a module and uses it, and the dependency run, writing
its module files apart, found the stale module file of that name in
`scratch` (the compile reads the one it has just written), so the check
after the compile failed. And 42 sources were declined for a "name
beginning with two underscores" that was the rest of a name continued on a
fixed-form line. (The two runs with line directives failed in the MPI
library's configure: `linedir.toml` has no MPI settings; `et-ld.toml` is
`et.toml` with line directives on.)

Fixed in `8827465`: the source is named by its path from `scratch`
(`../build/<Thorn>/x.f90`) and copied only when its line markers need
mapping (decision 13, as built); the dependency run runs in an empty
directory of its own with `scratch` first in `-I`; only `__NAME__`-shaped
words keep a source out. `smoke.th` with `et.toml` and with `et-ld.toml`,
served and audited across the two installations on `8827465`: 357 of 357
checked, 0 wrong.

**The Einstein Toolkit again, on `8827465`** (`gate-2.sh`, `gate-2.out`,
stores `store-et-2` and `store-et-ld-2`): 3376 compiles in each build, 594
of them Fortran, every build exit 0.

| Build | Result |
|---|---|
| `et.toml`, served in `build-cache` | 3376 published |
| audited in `build-cache-b` | 3375 checked (594 Fortran), 0 wrong |
| audited in `build-cache` | 3375 checked, 0 wrong |
| `et-ld.toml` (line directives), served in `build-cache` | 3376 published |
| audited in `build-cache-b` | 3374 checked (593 Fortran), 0 wrong |

The misses in the other installation: `HTTPD/Content.c` (as in every
earlier gate), and with line directives `TestLoopControl/TestLoopFortran.F90`,
whose build copy holds the absolute path of its source in string literals
that Cactus's own preprocessing wrote (LoopControl's macros): a different
text in each installation, rightly another key. The cost of Fortran on
real code (the audit in `build-cache-b`): keying 19 s and checking again
19 s, summed over the 594 compiles, against 451 s of compiling.

## What M3a is

Decision 14, the first half: the check after a compile that is to be
stored runs no compiler. Today it runs the key's compiler run again (`-E`
for C and C++, the dependency run for gfortran) and compares; that is
107 s of the 236 s a fresh Einstein Toolkit build pays (`a14f279`, fet
attempt 0006). The check stays as strong: every case the second run
catches must still be caught, or the compile falls back to the second run.

What the second run catches, and what replaces it:

- **A file it read changed.** The files are read again and compared with
  what was keyed (bytes, and the `seen` digest: identity, size, times, and
  every entry the name resolves through), as now. No compiler needed.
- **A lookup that would now go elsewhere**: a file that appeared where the
  compiler looks before the place it found one (a header earlier in the
  search path, a module file in the working directory), a directory that
  appeared or vanished in the path, a `.gch` beside a header. cactup
  repeats each lookup itself and requires the file the key's run entered.
  - C and C++: the key's `-E` run is given `-dI`, which prints each
    `#include`/`#include_next` as written (a macro-named one expanded)
    just before the marker that enters the file; one skipped by its
    guard or `#pragma once` is printed with no entering marker. The
    search list is the one the run's `-v` printed (quote, then bracket
    directories), with the directories it ignored as nonexistent, which
    must still not exist. Quote includes look first in the including
    file's directory; `#include_next` after the directory the including
    file was found in. A directory where a header could be is skipped
    (GCC and Clang, tried); any `.gch` at a place looked at sends the
    compile to the fallback.
  - gfortran: modules in the compile's order (`module_dirs`, as the key
    checks now); included files from each file's own `INCLUDE` lines, in
    the source's directory and then the `-I` directories (tried: the
    working directory is not searched). Any entry at a place looked at
    first counts as found: gfortran does not skip a directory there (it
    hangs on one, tried).
- **`__has_include`**: no output says what the key's run was answered.
  Each literal `__has_include`/`__has_include_next` in a file read is
  answered by cactup at key time over the same search lists, and the
  answers (by mapped name) join the key (label bump); the check after the
  compile asks again and must get the same. An answer that changed
  between the key's run and cactup's asking makes a key no other compile
  arrives at (its text says one thing and its answers another), so it
  serves nothing wrong. libstdc++'s `c++config.h` and glibc's `unistd.h`
  path have them, so nearly every compile does.
- **Anything this does not model** goes to the second compiler run, as
  today; the spec (§18.5, record mode) has the list as built.

**As built** (`52af3c8`, `3b34cc8`, `afa94e7`): `src/objcache/search.rs`
for C and C++, `Fortran::before_compile` for gfortran; the key's label is
`key-7` (the `-dI` lines are in the text, the `__has_include` answers in the
files part). What the smoke builds (`smoke.th`, served in `build-cache` and
audited in `build-cache-b`, GCC, line directives, Clang; script and logs in
`~/tmp/build-cache-overhead/`) found on the way, each now modeled:

- GCC names a system header by its physical path where that is shorter
  (`-fcanonical-system-headers`): Debian's `x86_64-linux-gnu/asm` is a
  symlink, so `asm/errno.h` is entered as `/usr/lib/linux/uapi/x86/asm/...`.
  A lookup is taken to agree when the file's physical path is the name.
- GCC's `limits.h` includes `"syslimits.h"` beside itself, which does
  `#include_next <limits.h>`: GCC goes on from the start of the list for a
  file found beside its includer (Clang searches as for a plain include).
- libstdc++ mentions `__has_include` in a comment (`#endif //
  __has_include`), glibc in a comment over two lines; Clang's own headers
  ask `__has_include_next`. Comments are found by reading the file as the
  compilers do, erring toward code; `__has_include` is answered for every
  directory of the search list and every directory of a file read, which
  answers `__has_include_next` too.
- Looking at every place a compiler tries took a median 4965 system calls
  for a small C file: each directory is now listed once per pass instead.

On `afa94e7` every check of the smoke builds was made by lookups (0 by a
compiler run), 0 wrong in every audit. Per compile, the check now costs a
median 7 ms for C (the key 18 ms), 31 ms for C++ (key 105 ms), nothing to
speak of for gfortran; most of it is reading every file again (110 files
for C, 330 for C++), which stays: it is what catches a file rewritten
within the change-time tick of the key's reading.

**The Einstein Toolkit on `afa94e7`** (`gate.sh`, logs and reports
`*-afa94e7.*` in `~/tmp/build-cache-overhead/`): `et.toml` served in
`build-cache`, 3376 published, audited in `build-cache-b`, 3375 checked, 0
wrong; `et-ld.toml` served, 3376 published, audited, 3374 checked, 0 wrong;
`clang.toml` served, 3376 published, audited, 3375 checked, 0 wrong. Every
check of the three serve builds was made by lookups. The cost, summed over
the all-miss `et.toml` build (fet attempts 0006 on `a14f279`, 0008 on
`afa94e7`, 0009 on `b2f7c1d`; one run each, the host busy with other work
during 0008 and 0009, as the compile times show):

| | `a14f279` | `afa94e7` | `b2f7c1d` |
|---|---|---|---|
| Compiling | 1296 s | 1398 s | 1391 s |
| Key | 112 s | 145 s | 129 s |
| Check after the compile | 107 s | 51 s | 57 s |
| C: key, check | 37 s, 36 s | 50 s, 27 s | 43 s, 31 s |
| C++: key, check | 53 s, 52 s | 71 s, 23 s | 62 s, 26 s |
| gfortran: key, check | 22 s, 19 s | 24 s, 0 s | 23 s, 0 s |

(Storing, 17 s on `a14f279`, took 60 to 175 s on the later runs: a few
publishes of 1 to 15 s each, on a host that had just rebooted and was
writing a lot; the publishing code did not change, and `fetld` on
`afa94e7` stored in 16 s.) What the key gained on `afa94e7` was the scan of
every file read for `__has_include`, 13 ms on a CarpetLib compile (5.8 MB),
now 1.1 ms (`b2f7c1d`, memchr). `46f4c8c` halves the pass before the
compile (a name searched along the same directories is found once per
pass): on real compiles, run in place, the check now takes 1.7 ms for a
bindings file (29 files read), 3.5 ms for a thorn's C source (115), 11 ms
for `CarpetLib/dh.cc` (384), most of it reading the files again. The
key's `-dI` costs the preprocessor nothing (timed).

The lookups a check makes are counted and timed in `events.jsonl`, and
that cost on the Einstein Toolkit (here, and on a cluster's filesystem) is
what Max decides M3b on. Gate: the review pair, and the Einstein Toolkit
served and audited across both installations with GCC, Clang and
gfortran, 0 wrong, with the fallback count reported.

## Next step

M3a (below and decision 14). Then Max decides on M3b from M3a's
measured lookup cost; after that, the CUDA compilers (decision 2), with
audit mode to prove them; the narrower key of decision 4 can be revisited
with audit mode too.
