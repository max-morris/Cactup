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
- While developing on this shared instance, give a build the cache's mode
  and store per command (`-K build-cache=off`, `-K build-cache=record`,
  `-K build-cache-home=<a directory of your own>`), not with `cactup knob`:
  since decision 34 a build with no knob set serves into the default store,
  and the instance's database is shared with other sessions and with
  installed cactup builds. (Users of a released cactup set the knob
  normally.)
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
| M3a | The check after a gfortran compile runs no compiler: the files read again and the places it looks looked at by cactup (decisions 14, 16; C and C++ on the branch `feature/build-cache-m3a-cxx`) | **signed off** at `6ba57f8` (round 12); inherited holes closed after it (the compiler identity watches where it searched), **signed off** at `95f7ec6` (seven reviews), its follow-ups at `05c67a3` and `e29e593` |
| M3b | Keys from the compile's own dependency list, no preprocessor run on a miss or a hit (decision 14) | **only if Max decides so**; it would need for C and C++ the `__has_include` knowledge M3a's C and C++ half could not make sound (decision 16) |
| M4 | CUDA: `nvcc` (and NVIDIA's other compilers as far as they go) keyed, served and audited like GCC and Clang (decision 2) | **passed the gate** at `b652348` (nine review rounds): decisions 24–31; design note `M4-CUDA.md`; qbd: audits all `same`, a served build's GPU test suite all passed; the two-thornlist benchmark 350 of 389 served, 1:07 against 8:30 |
| M5 | `cactup cache doctor`: the cache's rules applied to a configuration's build environment without compiling (path-variable refusals, compilers and the programs they run, refused flags and settings), each finding traced to the user's shell, the machine's `meta.toml` or the optionlist, with what to change; every cache-on build ends with one line: objects reused of all, and the misses by kind (built for the first time, sources changed, kept out by something `cache doctor` or `cache report` explains), a kind with a count of 0 left out (Max, 2026-10-13; "sources changed" needs a per-unit index beside the store); a CI check of every MDB machine's environment setup (each path variable unset, empty and set) for empty or relative entries. The checks sit behind a small seam (named sets of checks returning findings: severity, what, where from, fix), so a later general `cactup doctor`, out of scope here, can run them with others; and where whole files stay keyed (decision 27) | part 1 (the build's line, the unit records, `cache doctor` reading a build) **passed the gate** at `ad236d2` (nine review rounds); part 2 (the environment checks, the MDB check) **in review**; then the merge to master (decision 27) |
| M4b | Cray compilers: C, C++ and Fortran through `cc`, `CC`, `ftn`, seen through to the compiler they run, with the module environment that shapes them; Cray's `CC` as `nvcc`'s host compiler (decision 23) | follow-up after the merge (decision 27); no Cray machine to probe yet |
| M5b | Take on the cases where whole files stay keyed (decision 27): Clang's file checksums, `-g3`, preprocessor warnings, redefinitions | follow-up after the merge (decision 27) |
| M6 | HIP and other non-NVIDIA GPU compiles (`hipcc`, AMD's `amdclang`/`amdclang++`, and what else the MDB's GPU machines use) | follow-up after the merge (decision 27) |
| later | the narrower key revisited with audit mode | not started |

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
pasting rule. Reading each file as code cost 41 ms on the
437 headers CarpetLib's `dh.cc` reads; `9efbb9f` reads as code only a file
with something to look for (116 of them), from one byte that matters to the
next: 7 ms, on a host loaded by other work. The smoke builds on `87bde6c`
made every check by lookups, 0 wrong.

### M3a, round 3 (on `503638c`): BLOCKED by both

Both confirmed the model of the compilers' search again (every lookup case
they worked through held), and found the scan, and the edges of two
round-2 fixes, still open (all reproduced end to end, stale objects
served):

1. **The scan's reading of comments and literals** (B): where a number with
   a digit separator ends (`e+1'2`, `a.1'2`), a header name with `/*` in it,
   Clang reading `#warning` text raw; each hid code. Three rounds of this
   class: the scan no longer tells code from comments and literals at all.
   What it looks for counts wherever it stands (a misreading can only cost a
   needless fallback, never hide anything), and a use is taken for a
   comment only where its own line proves one. Measured on the 464 headers
   two real compiles read: no needless fallback.
2. **A `<name>` the compilers expand macros in** (A): in a macro, or in a
   macro's argument, `__has_include(<x.h>)` asks for the macro-expanded
   name (`linux` is predefined in GNU modes). Fixed: the key's run is given
   `-dD`, so every macro as the compiler read it is in the output: such a
   name is followed only while no word in it is a macro (the compilers'
   own included); an alias of `__has_include` and a `#` in an object-like
   macro are read there exactly. The definitions are in the text too (GCC's
   markers around them would differ otherwise): an unused `-D` now changes
   the key, which Cactus, giving every compile of a configuration the same
   ones, does not feel.
3. **An `-I` that is not a directory** (A, B): `-w`, colors and line
   wrapping hide GCC's warning. Fixed: every directory the compile names and
   neither list has is watched, from the command line.
4. **A line marker with a comment after its `#`** (A, B), past the
   pre-check of `9efbb9f`: gone with that pre-check; such a marker falls
   back.
5. **The physical name before the compile** (A, B): a symlink to the found
   header appearing in an earlier `-I` directory after the key's run was
   taken for it. Fixed: only where cactup itself found the file in a system
   directory (a bracket one not given by `-I`); and the entries its path
   resolves through are watched (A's N1: a symlink turned and turned back).

Also: `__has_include_next__` (GCC 9) is recognized; `__DATE__` and
`__TIME__` fall back whatever `SOURCE_DATE_EPOCH` says (older Clang ignores
it). A's remark that the lookups before the compile could run while the
compiler does is left for later.

### M3a, round 4 (on `5bbd724`): BLOCKED by both

Both re-ran every finding of rounds 1–3 in its reported form: all fixed.
New, each reproduced end to end with a stale object served:

1. **Comment proofs too loose** (A, B): a `*/` later on the line was taken
   for the end of a comment with a `/*` or `//` between; a `//` or `*/`
   inside a header name (`<a//b.h>`, `<a*/b.h>`) counted as a comment mark.
   Fixed: nothing on the way may open a comment, and no quote, `<` or `>`
   may stand where it could hide a comment mark.
2. **A `<name>` in a macro made of its parameters** (A, B): `#define
   HAVE(h) __has_include(<h>)`, and `__VA_ARGS__`; also the compilers'
   dynamic macros (`__LINE__`, `__COUNTER__`, `__FILE__`, …), which `-dD`
   does not print (A). Fixed: such a name falls back.
3. **Search directories from elsewhere** (A, B): `CPATH`,
   `C_INCLUDE_PATH`, `CPLUS_INCLUDE_PATH`, and `-iprefix`/`-iwithprefix`/
   `-iwithprefixbefore`, `-isysroot`. Fixed: the variables' entries are
   watched as the flags' are (by the language); the flags fall back. Also
   (A) `-Iinc/` is printed with its `/`: given and printed names are
   compared with and without it.
4. **Performance** (A, B): OpenMPI's `mpi_portable_platform.h` has
   `(#include "omp.h" not acceptabe)` in a comment, which the rule against a
   `#` passed to a macro sent to the compiler: 209 of 2782 C and C++ checks
   of the gate on `5bbd724` (every Carpet compile), 41 of its 56 s of C++
   checking. Fixed: the rule now asks what a forged marker needs after its
   `#` (a number, blanks, a quote, on its line or the next).

Measured by A on real compiles (quiet moments): the check after the compile
takes 10.5 ms for C++ (108 ms as a second compiler run), 2 ms for C (18
ms); the key grew 15 ms and 1.5 ms (the scan, `-dD`, the answers). The
gate's 15.2% of compile time (16.9% on `a14f279`) carried the 209
fallbacks and a host at load 30.

### M3a, round 5 (on `14b144e`): A SIGN-OFF, B BLOCKED

A re-ran every finding of rounds 1–4 (all fixed), found the model sound and
no needless fallback (the Einstein Toolkit checks by lookups but for a
trigraph source and `__DATE__`), and signed off. B, the same re-runs fixed,
found four more ways to a stale object, each reproduced end to end:

1. **Comment marks sharing a character** (`/**/*`, `*/**/`, `/**//`): the
   proofs took the substrings for separate marks. Fixed: no proof on such a
   line.
2. **A built-in function-like macro in an expanded `<name>`**
   (`__has_attribute(packed)`), which `-dD` does not show. Fixed: any
   reserved word (`__x`, `_X`) in such a name falls back.
3. **A name taken as written right on an `#if` line that the compilers
   expand**: a macro making the `(` (`#define CALL ID(`), or a comment
   between a macro's name and its `(`. Fixed: a name counts as written so
   only with nothing before it on the line but `defined` and its operand,
   numbers, operators and `__has_include`s, and no comment (glibc's `# if
   __has_include (<linux/…>)` qualifies, as it must: `linux` is a macro).
4. **A UTF-8 byte order mark** before `#embed` or a line marker. Fixed: a
   leading one is a blank.

Also from B: the physical-path work (`canonical`, the resolved entries) is
counted in `lookups`; a name with blanks at either end falls back. Open for
M3b's decision on a cluster: on NFS, directory listings and `open()` can be
answered from different client caches, so a change made by another host
during a compile might show to one and not the other (to measure, or to
state).

### M3a, round 6 (on `d76f6a0`): BLOCKED by both; M3a narrowed

Both re-ran rounds 1–5 (fixed) and found three more ways each to a stale
object, all deliberate constructions: a macro dropping a `*/` argument
(`DROP(*/)`) past the comment proof; UTF-8 identifiers (GCC 10+, Clang)
past every ASCII word rule (`é(…)` making a call, `__has_include_x`, a
UTF-8 parameter); blanks or a comment inside an expanded `<name>`; `g++`
compiling a `.c` file reading `CPLUS_INCLUDE_PATH`. And the cost: 16.8% to
14.4% overall, C not at all, hits dearer (decision 16). Max narrowed M3a to
gfortran: `feature/build-cache` takes back `a14f279`'s C and C++ path
(`key.rs` as at `0270b7d`, plus the hooks of the Fortran check), and the C
and C++ work stays on `feature/build-cache-m3a-cxx` (`d76f6a0`) with these
findings open. Next: the review gate on the narrowed milestone.

### M3a narrowed, round 7 (on `e67b55d`): BLOCKED by both

Both found the include search model, submodules, the copy (decision 13)
and the built-in modules sound, and the C and C++ path the same as at
`0270b7d`; the Fortran check cost 9–18 ms summed over a whole Einstein
Toolkit build (19 s at `a14f279`), with no fallback. Three findings, the
same from both (all reproduced; B served stale objects end to end, 0 of
50 at `a14f279`):

1. **Module directories resolved when the key was made**: an `-I`
   directory behind a symlink turned elsewhere led the compile to another
   module file, and the order check (and the built-in places) still looked
   along the old target. Fixed: the `-I` directories as the compile names
   them, resolved when looked at; the first module file found is compared
   with the one read by its physical path.
2. **The driver's own searches**: its `specs` file, pre-included header and
   intrinsic modules' directory, found along its prefixes (`LIBRARY_PATH`
   among them), were caught by a second dependency run and not repeated.
   Fixed (B's proposal): after the compile, the driver alone (`-###`, no
   compiler run, about 1 ms) must read the same `specs` file or none and give
   `f951` the same pre-included header and intrinsic directory; both read
   from `f951`'s own command line (the first `-fpre-include=` of the `-v`
   output could be one in `COLLECT_GCC_OPTIONS`).
3. **A file included that is also the pre-included header** was never
   placed. Fixed: placed like any other where it can be.

Also: the intrinsic modules' directory is the last place an included file
can come from (`include 'omp_lib.h'` no longer falls back); the spec says
when the module order is checked; code tidied (one `empty`).

### M3a narrowed, round 8 (on `52f4dfd`): A SIGN-OFF, B BLOCKED

Both re-ran round 7's harnesses end to end (0 stale where `e67b55d` still
served some) and found the search model sound (B traced gfortran's file
calls one by one). The Einstein Toolkit gate on `52f4dfd`: 594 of 594
Fortran checks by lookups, 1.6 s in all (19 s at `a14f279`).

B blocked on one finding, inherited from M2a (`a14f279` publishes the same):
where the compile reads a copy, a fallback to the dependency run is blind to
the copy's directory, so a file there (`.cactup/sub/j.inc`, an
`iso_c_binding.mod`) was compiled in and published. Fixed: where a copy is
compiled, a place taken before the compile, or an included file that cannot
be placed, fails the check instead of falling back. Also fixed, A's
non-blocking findings: the key's run and the check now both ask the driver
by `-###` (`-v` and `-###` quote differently, so a path with a blank or `$`
would never have matched); an entry of `LIBRARY_PATH`, `COMPILER_PATH` or
`GCC_EXEC_PREFIX` that is no absolute path keeps the compile out (inherited:
the driver searched a relative prefix from another directory than the
compile's; A served a stale `specs` that way on `a14f279` too); without the
map, the source is watched by the name the compile reads it by (inherited:
a symlink on the way turned and back); the `-###` child is registered for a
stop signal. B's: an `-I` directory given twice is no place before itself;
test docs; the spec's "left" window.

### M3a narrowed, round 9 (on `da35df8`): BLOCKED by both

Both blocked on the same finding, made by round 8's fix: the key's driver
findings came from a `-###` run after the dependency run, so a `specs` file
or prefix that changed between the two had the key read the old
pre-included header and the check compare against the new one, which the
compile also read. Fixed: the dependency run's own `-v` is read too, and
the check fails unless what its driver gave the compiler proper (now also
which compiler proper) is what the `-###` run, made in the compile's own
working directory, says the compile's will; relative driver prefixes then
lead where the compile's do, and the blanket refusal of them is gone.

A blocked on two more. An included file named by an absolute path that is
not there is found under a directory as `<dir>//<name>` (tried), and the
absolute name itself, where gfortran looks first, was no watched place:
it is now. And the way the source's name leads could change while the key
read it (a symlink on the way turned and back between the read and the
dependency run; B too, non-blocking): the way is now pinned before the
source is read and must be the same when the key is made, the name the
compile gives must reach the file read, and a written copy must hold the
text keyed.

### M3a narrowed, round 10 (on `6849a3a`): A SIGN-OFF; B did not report

A re-ran round 9's three blocking findings end to end (all caught, 0 stale
served) and found no new hole. B ended on an API error before reporting. The
Einstein Toolkit gate on `6849a3a`: 0 wrong in all three audits; the
Fortran check 1.3 s over 594 compiles (median 2 ms, all by lookups; 18.6 s
at `a14f279`), the Fortran key 28 s (22 s at `a14f279`). A's non-blocking
findings, taken: the driver's `-###` ran in every key though only the check
uses it (about 1.4 ms on a hit too): it now runs right before the compile;
and the driver's findings were compared as strings, so a relative prefix
with `..` could name two compilers proper alike from the two directories:
any relative name there now fails the check. A test of the fallback to the
dependency run (with the driver compared) was added.

### M3a narrowed, round 11 (on `dd02e31`): BLOCKED by both, on the same one

Both re-ran rounds 7–10's findings (0 stale served; B's timing batches 40
and 120 runs each) and found the search model matching gfortran in every
case tried. Both blocked on one finding that round 9 had opened: with the
refusal of relative driver prefixes gone, an empty `LIBRARY_PATH` entry (as
`export LIBRARY_PATH=$LIBRARY_PATH:/x` leaves when it was unset) led the
key's driver, in its own directory, elsewhere than the compile's; the
comparison of the two runs only before a compile, so a hit served a stale
object (A: a fake `omp_lib.mod` in `configs/<conf>/lib/finclude`; B: a
`specs` file and a pre-included header under the working directory; audit
mode called it "inputs changed", not a wrong hit). Inherited from M2a
(`a14f279` serves it too), closed at `da35df8`, reopened at `6849a3a`.
Fixed: the refusal is back in the key (no cost on a hit), and it now also
covers a compiler named by a relative path and, for one named without a
`/`, an entry of `PATH` that is no absolute path. Also: a driver answer
naming no compiler proper fails the check; spec wording on when the driver
is asked.

Left, non-blocking: an absolute include found directly falls back to the
dependency run (a copy's compile is then not stored), sound and rare in
Cactus; record mode keeps a copy's refusals for a source with line markers;
`checked_by_compiler` also names cases where nothing ran; an assembler found
through a relative `COMPILER_PATH` or `PATH` entry (all languages, inherited
from M1, not reproduced).

### M3a narrowed, round 12 (on `6ba57f8`): SIGN-OFF by both

Both re-ran every earlier blocking finding (0 stale served; B 30 runs a
case, submodules too) and checked the search model against gfortran 14.2
once more. The Einstein Toolkit gate on `6ba57f8`: 594 of 594 Fortran
checks by lookups, no fallback, 1.9 s in all (median 3 ms; about 19 s at
`a14f279`), 0 wrong in audit. **M3a signs off here.**

Two holes inherited from earlier milestones, reproduced end to end with a
wrong object served, and as open at `a14f279` (never caught by a second
dependency run, so not blocking M3a), closed right after it in one change
reviewed on its own:
- B: a place taken right before the compile sent the check to the
  dependency run; a file put there, read by the compile and gone again
  before that run, was stored. Such a place now fails the check.
- A (inherited from M1, all languages): with `.` in `PATH`, an assembler
  that appeared in the working directory after the build's compiler
  identity was worked out assembled objects stored under the real one's
  identity. A relative or empty entry of `PATH`, `LIBRARY_PATH`,
  `COMPILER_PATH` or `GCC_EXEC_PREFIX`, or a compiler named by a relative
  path, now keeps every compile out of the cache (it was Fortran only).

Left, non-blocking: an absolute include found directly falls back to the
dependency run; a driver passing two `-fintrinsic-modules-path` (only from
modified language specs) would be read as one; `lookups` counts gzip module
inputs never looked up; `checked_by_compiler` names cases where nothing ran.

### After M3a, review 1 (on `ebaee25`): BLOCKED by both, on the same three

Both found the two holes closed only in the shape reproduced, and three
ways (all open at `6ba57f8` and `a14f279` too) to a wrong object served:
- A Fortran source that also includes a file by an absolute name: the
  first file that could not be placed sent the check to the dependency
  run before any place was looked at, so a file appearing and gone again
  was stored. Fixed: every place is looked at first (one taken fails the
  check), and the fallback looks at them again after the compile.
- An assembler (or `f951`, `cc1`, by `COMPILER_PATH`) appearing in an
  absolute directory searched before the one found, after the build's
  compiler identity was remembered (the round-12 case was about timing,
  not the relative entry). Fixed: the identity is remembered with every
  place passed over before each program (`-print-search-dirs`, then
  `PATH` for the assembler) and library (the places glibc's loader says it
  tried, a missing directory on its search path, its cache file); each
  must hold nothing (or what it held) for the identity to be reused. About
  15 empty places and 30–40 files per compiler on `plato`; key time
  unchanged in the smoke builds.
- A library loaded from the working directory through an empty
  `LD_LIBRARY_PATH` entry (as cactup's own docs' `export` idiom leaves
  one) was dropped from the identity. Fixed: those places are looked at
  from each compile's own directory, and a library found by a relative
  name rules the compiler out.

With places watched, the refusals were narrowed (A, B): relative `PATH`
entries and a relatively named compiler keep only Fortran out (its
dependency run runs elsewhere), `LIBRARY_PATH` only Fortran (it finds a
`specs` file, which C and C++ compiles report each time);
`COMPILER_PATH`, `GCC_EXEC_PREFIX`, a library preloaded by a relative
path and `LD_AUDIT` keep every compile out. The refusal names an empty
entry as the working directory.

Left, non-blocking: a place empty right before the compile, taken during
it and empty again after (§18.10's stated limit); the reason a whole build
was refused is only in `cactup cache report` and `events.jsonl`.

### After M3a, review 2 (on `431d392`): BLOCKED by both

Both re-ran every earlier reproduction (all closed) and confirmed the
loader model against what `LD_DEBUG=libs` prints (the cache hit has its
own try line; a missing directory is listed only once). Found, all run
end to end with a wrong object served, all but one open at `6ba57f8` too:
- Both: the identity was checked only when the key was made; an assembler
  appearing (or replaced, as an upgrade does) while a compile ran
  assembled what was stored. Fixed: the identity is worked out again after
  the compile, before storing, and must be the same (cheap while what it
  watches holds; only a miss pays).
- A (B non-blocking): a program the driver runs found through a relative
  `PATH` entry was remembered by its absolute path and reused from other
  directories; reopened by review 1's narrowing. Fixed: it rules the
  compiler out, as a library found so does.
- A: a link at the place found, turned elsewhere, was not seen (only the
  physical file was watched). Fixed: the names as found are watched too
  (the driver, its programs, the libraries).
- A, B: the loader stops trying an entry that is no directory; with
  `LD_DEBUG_OUTPUT` set the loader's report went to files (and those were
  left behind); a library preloaded by an absolute path that is not there
  is loaded once it is. Fixed: such an entry is watched, the variable is
  removed for the probe, and missing preloads are watched.
- B (regression): a GCC built `--with-as` names an assembler outside its
  search directories, and was refused; accepted again, with nothing before
  it to watch.

Also: the watched places are deduplicated (A measured 610 with 25 extra
`PATH` and 15 `LD_LIBRARY_PATH` directories, about 2 ms a compile on a
local disk; half after deduplication); "not an absolute path". The
compiler part of every key changed with review 1 (the loader and preloaded
libraries are hashed now), so entries stored by earlier binaries are
misses. Left, stated in the spec: what appears during a compile and is
gone after, `/etc/ld.so.preload`, `GLIBC_TUNABLES`, `GCONV_PATH`, a Clang
configuration file added mid-attempt.

### After M3a, review 3 (on `6d4de25`): BLOCKED by both

Both re-ran every earlier reproduction (B: 31 cases; all serve a wrong
object at `6ba57f8`, none at `6d4de25`). The Einstein Toolkit gate on
`6d4de25`: 3376 published, no place taken, no refusal; key and check time
within the host's noise. Found, all inherited and run end to end with a
wrong object served:
- Both: a program the driver runs that is a script (a wrapper around
  `as`) was hashed by its own bytes; what it runs was not followed. Now
  ruled out, as a driver that is a script is.
- A: `-m32` and `-mx32` (accepted as machine options) have the driver look
  for its programs in other directories than the identity asked about.
  Now the flags `-print-multi-lib` lists keep the compile out.
- A: the identity's own search along `PATH` took a file the system would
  not run (mode, `noexec`), where `execvp` goes on to the next. Now each
  candidate is started to see; a program whose libraries cannot be listed
  fails closed.
- B: a library its program names by a relative path with a `/` is opened
  unsearched, and was dropped. Now it rules the compiler out.

Also: a place that appeared between the driver's or loader's search and
the identity's look is caught by asking both again after looking (B); a
preloaded library found nowhere and a missing loader cache are watched
(A, B); the spec's Clang sentence was wrong (each compile reports a
configuration file; A); the cost on a network filesystem without negative
lookup caching is stated (B).

Left, non-blocking: a refusal made in one directory (a helper found there
by a relative name) holds for the attempt in other directories (lost hits
only); Fortran with an empty `LIBRARY_PATH` entry, or `.` in `PATH` with
`gfortran` named bare, is still refused (B suggests the dependency run
could be given the identified path as its name instead).

### After M3a, review 4 (on `67b1014`): A SIGN-OFF, B BLOCKED

The first attempt of both reviewers ended on a network error and was run
again. A re-ran every earlier reproduction (all closed at `67b1014`, all
open at `6ba57f8`) and signed off; a safety classifier stopped one of A's
responses, after which A ran no new attack programs and judged by reading
(stated in A's report). B re-ran all 31 (closed; one more, an `as` whose
ELF interpreter is missing, closed in passing) and blocked on three, all
inherited and run end to end with a wrong object served:
- A module file put right before the compile at a place the compile looks
  first, gone after it: module files had only the order check, made when
  the key is made and after the compile. Fixed: the places before each
  module file are looked at right before the compile, as an included
  file's are.
- A library preloaded through `$LIB` was watched under the literal name.
  Fixed: such an `LD_PRELOAD` keeps every compile out.
- A driver behind a link turned to a hard-linked copy of its tree (one
  with another `as`): every watched file looked the same. Fixed: the
  driver's physical path is remembered and compared.
Also: `/etc/ld.so.preload` watched and `GLIBC_TUNABLES` among the
variables that have the identity worked out again (B); the spec's `execvp`
wording; multilib aliases and a `noexec` remount stated as limits.

Gate on `67b1014`: 0 wrong in all three audits; no refusal, no fallback,
no place taken; the Fortran check 2.3 s summed (median 3–4 ms).
Non-blocking, for landing: bump `KEY_LABEL` (entries stored by the
review binaries since `970f26a` share `key-6` while the identity got
stricter); the build's summary line does not count refused compiles; the
MDB's `${LIBRARY_PATH+$LIBRARY_PATH:}` leaves an empty entry when the
variable is set but empty, which keeps Fortran out (A).

### After M3a, review 5 (on `9fd566d`): BLOCKED by both

Both re-ran every earlier reproduction (all serve a wrong object at
`6ba57f8`, none at `9fd566d`). Found, all inherited and run end to end
with a wrong object served:
- Both: a helper reached through a link to a hard-linked copy of its tree
  takes its `$ORIGIN` (and so its libraries) from the copy, while every
  watched file looks the same; review 4's fix was for the driver only.
  Fixed: every watched file is also watched by where its name leads now.
- A: an ELF wrapper around the assembler passed review 3's "no script"
  rule. Fixed: a program the driver runs must say in its own bytes what
  it is (GNU's assembler or C, C++, Fortran compiler proper).
- A: the identity's places were not looked at right before the compile; an
  assembler put there during the key's preprocessor run that deleted
  itself after assembling was stored. Fixed: the identity is worked out
  again right before the compile, and the directories that hold what it
  watches must look the same after the compile as before it (also closes
  the stated "during the compile" limit for absolute places; A showed it
  deterministic). Relative places lead into the working directory, which
  the compile writes: Max decided (2026-10-11) that a relative or empty
  entry of `PATH` or `LD_LIBRARY_PATH` keeps every compile out, and that
  the MDB idioms that leave one are fixed.
Also: a program whose loader is not glibc's is ruled out (A, run with a
musl `as`); `$LIB`, `$PLATFORM`, `$ORIGIN` in `LD_PRELOAD` are expanded as
the program's own loader says (`--list-diagnostics`) instead of refused
(Max: the refusal was too broad); a compile that converts character sets
while `GCONV_PATH` is set is refused (B ran it: a wrong object served).

### After M3a, review 6 (on `b011058`): BLOCKED by both

A safety classifier stopped both reviewers partway (and, once, the
author): after that they built no new attack programs and judged by
reading; B re-ran review 5's 31 reproductions (all closed at `b011058`, all
open at `6ba57f8`), A ran none. Found:
- Both, run end to end with a wrong object served: a link on the way to a
  watched place (a `PATH` or `LD_LIBRARY_PATH` entry) turned to another
  tree while the compile ran and turned back before the check left every
  stamped directory looking the same through it. Fixed: every entry on
  the way to each watched place is stamped too (`trail`).
- B, run: `LOCPATH` (and the system's converter modules) not keyed for a
  compile that converts character sets. Fixed: every such compile is
  refused (Cactus asks for none).
- B, inferred: a hit is served on the key's preprocessor runs with no
  check after them. Fixed: the identity and the stamps are checked again
  at the end of the key.
- A, inferred: a `specs` file the compile's driver finds only while it
  runs. Fixed: the identity watches the places a GCC looks for one (its
  library prefixes), and `LIBRARY_PATH` has the identity worked out again.
Also: a `PATH` candidate is only started, not waited for (A); the docs
taught the faulty `LD_LIBRARY_PATH` idiom (B), now fixed with a note on
why; the `CPATH` idioms of generic, mike, et-juphub and sunrise (A); a
test whose copied `clang` sat in `/tmp`, whose missing `../lib` put `/tmp`
under watch, moved under `target/` (Max made that edit); the character-set
test rewritten (it had skipped itself: `-o -` writes a file named `-`).
Cost noted: a missing place has its nearest existing ancestor watched, so
a missing `~/bin` on `PATH` puts the home directory under watch (lost
stores only). Open for Max: whether reviews go on hunting with new
attack programs, which the classifier now stops.

### After M3a, review 7 (on `95f7ec6`): SIGN-OFF by both

Within decision 19's scope. A re-ran 37 earlier reproductions, B 31 plus
19 others: every one serves or stores a wrong object at `6ba57f8`, none at
`95f7ec6` (one timed swap of a module file, out of scope, still publishes:
§18.10's stated limit). The Einstein Toolkit gate on `95f7ec6`: 0 wrong in
the audit, 594 of 594 Fortran checks by lookups, no refusal. Non-blocking,
for the follow-up: a `PATH` entry whose name is not UTF-8 leaves the memo
unwritten, and then the stamps are empty both times and pass (fails open;
also a hit costs about 600 ms); `LD_LIBRARY_PATH` split on `:` only (the
loader splits on `;` too); NFS attribute caching (a reinstall by rename on
another host, seen by `stat` up to a minute late on a hit; inferred);
hits with a long `PATH` cost about twice what they did at `b011058` (the
memo parsed and `PATH` searched four times); a `TMPDIR` that is the
nearest existing ancestor of a missing place refuses every Fortran compile
(the key's own preprocessor run writes there), with a misleading reason;
refusals broader than needed (`PATH=$PATH:.`, `LD_LIBRARY_PATH=""`, an
empty entry after every library is found); spec text (the cost sentence,
a second `specs` place, §18.8 on character sets); `KEY_LABEL` to bump at
landing.

### After M3a, follow-up to review 7 (decision 20)

Review 7's non-blocking findings, taken: the compiler identity's watched
places are kept in memory by the compile that identified it (`Watch`) and
checked from there (the memo is read once a compile; a memo that could not
be written, as with a `PATH` entry whose name is not UTF-8, no longer
leaves the stamps empty and passing: such a name is refused at once);
watched files are opened, not only `stat`ed (NFS attribute caching); a
place passed over that is not there is looked at by its own name only
(decision 20: no busy ancestor such as `$HOME` or `TMPDIR` under watch);
a stamp mismatch names the directory that changed; the refusal of relative
`PATH` and `LD_LIBRARY_PATH` entries is judged by the searches themselves
(the loader's `;` included; an entry after what is found keeps nothing
out); the `specs` file GCC reads from its machine's directory is watched;
spec text (the cost sentence, §18.8 on character sets); `KEY_LABEL` is
`key-7`.

### After M3a, review 8 (on `05c67a3`): SIGN-OFF by both

Both re-ran every earlier reproduction at `95f7ec6` and `05c67a3`: the
same outcomes but for reason texts and the refusals decision 20 meant to
lift (`TMPDIR`, a busy home directory now cached), and one reproduction
reopened by design (a self-deleting assembler, appearing and gone within
one compile: decision 20's limit, out of scope by decision 19). Both ran
in-scope cases with real tools (a `current` link turned to a version with
another `as`; an `as` installed into an empty `PATH` directory during a
long compile): seen. Gate on `05c67a3`: 3376 published, none refused, none
unstable, 594 Fortran checks by lookups. Taken from the non-blocking
findings right after: a relative `specs` place (from `LIBRARY_PATH`) for C
and C++ is a search through the working directory, ruling the compiler out
(A; it had been dropped); the `specs` place in a relocated GCC's configured
prefix (B); a bare Fortran driver's own `PATH` walk judged by where a
relative entry stands (both); the loader's search-path lines no longer
over-refuse relative entries it never tries (A); only files and
directories are opened (a FIFO would block); wording; the spec's cost
sentence (about 30 opens a check, 1–2 ms a hit, 4–6 ms with a long
`PATH`). Left: a stamp mismatch after the compile shows as `stable:false`
without a reason (for `cache doctor`, M4); `PATH` is searched three times
a hit.

### After M3a, review 9 (on `e29e593`): SIGN-OFF by both

A short review of the fixes after review 8. Both re-ran every earlier
reproduction at `05c67a3` and `e29e593` (same outcomes, reason texts
aside) and checked each change directly: a relative `LIBRARY_PATH` entry
now keeps C and C++ out (GCC reads `./specs` through it); a bare
`gfortran` with `.` after its place on `PATH` is cached, before it refused;
the relocated GCC's `specs` place is watched; untried relative loader
entries keep nothing out; a FIFO named `as` early on `PATH`, which hung
the compile at `05c67a3`, does not. Taken right after, from the
non-blocking findings: the reason names the variable whose entry the
search went through (it said `PATH` or `LD_LIBRARY_PATH` for a
`LIBRARY_PATH` entry); the configured machine directory from `--libdir`,
`--exec-prefix`, `--prefix` and autoconf's defaults; one open without
waiting (`O_NONBLOCK`) in place of a `stat` and an open; tests for both
new behaviors; spec and comments. For Max: the `LIBRARY_PATH` refusal now
keeps C and C++ out under `export LIBRARY_PATH=/x/lib:$LIBRARY_PATH` with
the variable unset (only a `specs` file appearing and gone within one
compile in the build's own directory is guarded; B).

### After M3a, review 10 (on `86a840c`): SIGN-OFF by both; landed

A quick review of the last fixes before landing (decision 21). Both re-ran
review 9's set at `e29e593` and `86a840c`: the same outcomes, the reason
now naming the right variable in every case, no FIFO hang, 0 objects
published with a site `specs` file added mid-build. Gate on `e29e593`: 0
wrong in all three audits. The branch then landed on master, and the
contract went into `CLAUDE.md` (decision 10). Follow-ups, non-blocking,
for M4's start: the test of the configured machine directory does not
test a relocated GCC (it passes with the code it should test removed; use
`gcc_under`); `configured_libdir` parses the configure line loosely (the
first occurrence where configure honors the last; `--prefix /x`, quoted
values); the reason's variable is set as a side effect of each search (a
relative RPATH try would be blamed on `LD_LIBRARY_PATH`: pass it to
`passed` instead); the spec omits the `/usr/local` default.

### M4, round 1 (on `4a14cba`): BLOCKED by both

Both on plato (tests) and qbd (Slurm jobs on `single`, in
`/work/mmorris/cactup-m4/review-a/` and `review-b/`). Blocking, all fixed
in `4a25fd3`:
- (B) `PTXAS_FLAGS`/`CUDAFE_FLAGS`, which `nvcc.profile` passes on, changed
  the object, not the key (a stale object served). Now the plan is made at
  key time and keyed: every step's words and the environment but `PATH`
  and `LD_LIBRARY_PATH`, mapped (`PathMap::in_text`).
- (B, A's N2) the last host compile reads CUDA headers no pass reads
  (`crt/host_runtime.h`, `fatbinary_section.h`, …): a header update was
  served stale. The trial now records what that step reads (`-MD`), the
  identity hashes and watches it (`host_reads`), and each compile's last
  step must read nothing else the key does not cover, or it is not stored.
- (A) the host compiler was found along `nvcc`'s `PATH` without watching
  what it passed, and the steps ran by name: a `g++` 13.2 put ahead of 11.4
  was stored under 11.4's key. Now found with the places watched, and each
  step runs the file its identities found (`Plan::resolve`; a program
  neither knows: not cached).
- (A) `-lineinfo` and `-G` objects name the real source path: 99 wrong in
  an audit across configurations. No path map for them now (round 2: not
  cached at all).
- (A) `-arch=native`: not cached now.
- (A, B) `NVCC_PREPEND_FLAGS`/`NVCC_APPEND_FLAGS` bypassed the reader (an
  options file changed under one key): not cached now.
- (B) host flags given through `nvcc` skipped the GCC reader's notes
  (charset not refused; a Clang host's OpenMP): now noted. (`-include`
  cannot get past the CUDA reader at all.)
- (B) a step killed by a signal: `nvcc` says `nvcc error   : 'cicc' died
  due to signal 9 (Kill signal)` and exits 9 (seen; its table has 4, 7, 8,
  9, 11); the replay does so too. (A) `nvcc`'s own warnings (the dry run's
  other lines) were lost: said first now.
- (A, B) the spec: §18.11 written, §18.4 and the status line fixed, the
  user docs too.
Non-blocking, done: `shell_words` follows `sh` (a `$NAME` `nvcc` does not
set from the environment, an empty unquoted expansion no word, backslashes
in double quotes; more shell than that: not cached); the dry run's values
untrimmed (`_SPACE_` is one blank). Left: the trial uses `nvcc`'s default
host compiler (where `nvcc` rejects that one, no CUDA compile is cached);
the host compiler's libraries are identified in the wrapper's environment,
not in `nvcc`'s (CUDA's `lib` first); the plan's dry run is in `key_ms`
now. On `4a25fd3`, qbd, fresh `store7`: serve `m4a`, audit `m4b` all 404
`same` (99 `nvcc`), serve `m4c` all 404 served, linked, 1:05. Cost
(reviewers, from the events): keying a CUDA compile 4.5 s
median against an 18.7 s compile, the check after it 2 s; about 12 % on a
miss; a served build 1:39 against 8:56.

### M4, round 2 (on `4a25fd3`): BLOCKED by both, one finding each

Both re-ran their round-1 reproductions on `4a25fd3`: all fixed (another
`g++` ahead on `PATH` runs nothing new and is not stored; `$HOME` in a
flag; `nvcc`'s warnings byte for byte; failing steps' exit codes and
leftovers; `-arch=native` and `NVCC_APPEND_FLAGS` not cached;
`PTXAS_FLAGS` keyed; a killed `cicc` as `nvcc`; an updated CUDA header a
new identity). B ran served objects with given IDs on an A100, linked with
one `nvcc` made itself: correct. Blocking, fixed in the commit after this
record:
- (B) the last step finds CUDA's `math_constants.h`, `crt/host_runtime.h`,
  … by searching the compile's own `-I` directories first, and no pass
  makes those lookups: a thorn header of that name, added after the object
  was stored, was served around (`nvcc` itself fails, or reads it). Now
  each place searched ahead of such a header (the generated files'
  directory, `-iquote`, `-I`, `-isystem`, `-idirafter`; `.gch` beside
  each) must be empty at key time and stay so (`Plan::ahead`; not cached
  otherwise).
- (A) `-G`/`-lineinfo` record each source's modification time in the
  device line table, and Cactus copies sources anew before each compile: a
  rebuild was served objects with the old times. Not cached now.
Non-blocking, done: the variables `nvcc`'s programs read keyed (`EDG_…`,
`NVVM…`, `OCG_FLAGS`, `NVASM_FLAGS`, `LLVMC_FLAGS` and others); an unquoted
`$NAME` whose value has blanks or a pattern is not repeated (the shell
would split it); the spec's word on stop signals made exact (`nvcc` reports
a step's death by `SIGINT` and exits 2; the wrapper ends by the signal, as
for any compile); doc comment, duplicate list, `use` order, two clippy
lints. Key label `key-10`. Left: MDB machines without
`--objdir-as-tempdir` (et-cuda, qbc, db1, slurmjupyter) are never cached
(coverage, for M5's doctor to point at).

### M4, round 3 (on `c3b03d7`): BLOCKED by both

Round 2's fixes held (a thorn header ahead of CUDA's, `-G`/`-lineinfo`,
another `g++` on `PATH`, `CPATH` shadows; served objects with given IDs
ran right on an A100, linked with one `nvcc` made). Blocking, fixed in
`feature/build-cache-next` (decisions 28, 29):
- (A, B) decision 26 served stale messages: a dropped definition can draw
  a warning about itself (a stray quote, `/*` in a comment, `unix`
  redefined, `#pragma GCC poison`), and the compiler's messages can quote
  or name one (a `-Wsign-compare` range over it; "did you mean"). Now no
  line is dropped where the key's preprocessor run diagnoses anything, and
  a hit with messages must match the files whole (decision 29); the
  macros `nvcc` gives its passes count as given.
- (B) a line that only looks like a definition, inside a block comment
  whose end is followed by code: the scanner now follows comments and raw
  strings.
- (B) `Plan::ahead` named a header by the first directory above it: with
  `-I<CUDA root>` a thorn's `math_constants.h` went unchecked. Now every
  name below every directory searched.
- (B) `GCC_EXTRA_DIAGNOSTIC_OUTPUT` (and `GCC_URLS`, `TERM_URLS`, `COLUMNS`)
  keyed.
- (A, B) the bench audit's wrong hit: the build on qbd358, whose file
  system stalled, stored it (B re-ran the thornlist cleanly: byte for byte
  the audit's object). Decision 28: each stored compile is held to what its
  key read.
Non-blocking, done: the spec's word on stop signals (a step may end with
a status of its own). Left: `read_files` holds a unit's files in memory at
once (about 20 MB for a CarpetX CUDA unit); the C/C++ own-reads check
canonicalizes only where names differ.

### M4, round 4 (on `5e31789`): BLOCKED by both

Round 3's fixes held. Blocking, fixed (decision 30 and the commit after
this record):
- (A, B) a CUDA unit whose passes print anything was never stored: the
  check ran the passes without `-dU` once the key had stopped narrowing
  (CarpetX's `#warning "TODO"` units, about 250 s each build). `-dU` is
  now decided once.
- (A) an entry with messages shut every other configuration out of its key
  (`SaidElsewhere`, then "another build published it first"): the
  prolongation units, a rebuild's critical path. Now such entries have a
  key of their own (decision 30), and only messages that speak of macros
  or suggest a name need the files whole.
- (A) Clang lists what `__has_include` found in `-MD`; the markers do not:
  any Clang unit including `<memory>` was never stored (and a header under
  two spellings). Now the compile's list is compared with the key run's
  own `-MD` list.
- (A, B) a C++14 digit separator (`1'024`) read as a character constant
  hid a `/*`: a stale object. Fixed in the scanner; and (B) a definition
  line between statements decides `-Wmisleading-indentation`: files with
  code are keyed whole now (decision 30).
- (B) `#pragma GCC dependency` makes messages depend on when files changed
  (inherited since M1b): not cached now.
- (B) `-MD -MF <object>.cactup-reads` could pass the longest name a
  directory takes: short names of the wrapper's own now (for `nvcc`'s
  steps too).
Non-blocking, done: CUDA's own include directory given with `-I` no longer
counts as ahead of itself; doc comments put back; the `-MF` refusal
worded. Left: the contract file's line on `-MMD` (updated with this
record); decision 28 cannot see a `__has_include` branch that includes
nothing different.

### M4, round 5 (on `6a27a41`): A BLOCKED; B hung, sent again on `94d856c`

A found the object and exit status exact, and blocked on four ways for a
served object's messages to differ from the compile's own. Handled in
`94d856c`:
- (A) a "did you mean" naming an unused macro of the current headers, and
  messages in a language other than English, which `speak_of_definitions`
  cannot read: decision 31, the knob `build-cache-preserve-exact-warnings`
  (`false` by default: best-effort warnings under the narrowed key; `true`
  keys whole files).
- (A) whether a warning exists at all must stay exact, since `-Werror`
  turns it into a failed compile: an include directory that is missing
  (gfortran warns by default) is now keyed by its existence, and
  `#pragma GCC dependency`, which `-E` drops (round 4's check never
  fired), is found by scanning the files read. Key label `key-15`.
B's round-5 run hung on one tool call 16 minutes in and reported nothing.
It was stopped after three hours and sent again, now on `94d856c`.

### M4, round 6 (on `94d856c`): BLOCKED by both

Both re-ran their earlier reproductions (all fixed) and found the object
right in every case. Each found ways for a warning to exist in the compile
and not in a hit, or the reverse; with `-Werror` that is a compile that
should fail and does not. One of them reached an exit status without
`-Werror`: GCC before 13 ignores `#pragma GCC diagnostic` under `-E`, so a
narrowed key served an object where the compile fails (A, on qbd's GCC
11.4). Fixed in `24c7479` and the commit after this record:
- (A, B) `#pragma GCC dependency` behind `_Pragma`, a macro, a comment, no
  blank or a continued line; (B) `__has_include` deciding only a
  `#warning` (inherited); (A, B) `#pragma GCC poison` of a name defined
  only in a header the key narrows; (A) a `#line` naming a file that does
  not exist, under which a narrowed header's warning was said. The key's
  preprocessor run prints each of these as the compile does: what it says
  of the text (its diagnostics, mapped; gfortran's message lines) is now
  in the text part, for C, C++, `nvcc`'s passes and gfortran (whose check
  after the compile then runs the dependency run again). A diagnostic that
  names no existing file keeps every file whole.
- (A, B) an include directory that is a file (GCC always warns): its kind
  is keyed.
- (A) the knob never reached a C/C++ key: a `true` build could share a key
  with a `false` build's narrowed entry. Keyed now; and (B) at `true` the
  locale too, since the messages' language and quotes follow it.
Also (B): the spec's knob list and frozen settings name the knob. Left:
`true`/`false` where `build-cache-relocate` takes `yes`/`no` (Max's words);
older clippy lints. Key label `key-16`.

### M4, round 7 (on `6ff97d4`): BLOCKED by both

Both re-ran round 6's reproductions on plato and qbd: all fixed (every
`#pragma GCC dependency` spelling, `__has_include` deciding a `#warning`,
poison, `#line`, an include directory that is a file, the knob). The
benchmark on `6ff97d4` (`bench6`): 350 of 389 served (80 of 96 `nvcc`) in
1:07, audit 389 the same; a CarpetX-style `#warning "TODO"` unit shared
across installations. Blocking, fixed in `a0a697f`:
- (A, B) `-fdiagnostics-color=always` (Clang's `-fcolor-diagnostics`)
  splits the words a diagnostic is read by, so the key heard nothing said
  of the unit's own source and round 6's holes came back. Escape sequences
  are now stripped first (gfortran's lines too).
- (A) GCC before 13 ignores `#pragma GCC diagnostic` under `-E`: with one
  in the text, a narrowed key served an object where a C compile fails on
  a warning the pragma made an error (GCC 11.4 on qbd; RHEL 9's system GCC
  is 11). Where the text has one and the run names no GCC 13 or later,
  nothing is narrowed.
- (A) at `true`, the locale's variables are keyed but not what they
  resolve to on the host (which locales and message catalogs are
  installed). The key now has how the compiler (for CUDA, the host
  compiler) words a small warning in the compile's environment.
Non-blocking, done: (A, B) curly quotes are read as plain ones, so a
Fortran unit that warns keys alike under `LANG=C` and a UTF-8 locale;
`f951: Warning:` lines read; a stale comment. Left: (A, inferred) a file
touched between the key's `-E` and the compile: the check after a C or C++
compile runs `-E` again and compares what it said. Key label `key-17`.

### M4, round 8 (on `a0a697f`): BLOCKED by both

Both re-ran round 7's reproductions: all fixed (colors under GCC, Clang and
`nvcc -Xcompiler`; GCC 11.4's ignored `#pragma GCC diagnostic`; the locale
at `true`; Fortran shared across locales). A tried round 7's G4 for real (a
dependency file touched at random moments, 30 runs): the runs that landed
in the window were not published. Blocking, fixed in `a9c0dba` and the
commit after this record:
- (B) **a stale object** in C: after `#pragma GCC optimize` (and `target`,
  on GCC 11) the C compile redefines `__OPTIMIZE__`, `__FAST_MATH__`,
  `__AVX2__` for the rest of the unit and `-E` does not, so `-dU` missed a
  macro the compile used and its definition was left out (plato's GCC 14,
  qbd's GCC 11.4). (A) And GCC 13 and later follow `#pragma GCC
  diagnostic` under `-E` only for an option the preprocessor knows
  (`-Wcomment`, not `-Wall`), so round 7's version gate did not hold. Now a
  C unit whose text has a `#pragma GCC` other than those the preprocessor
  alone handles (`system_header`, `poison`, `warning`, `error`,
  `dependency`) or that change nothing later lines read (`ivdep`, `unroll`,
  `novector`, `visibility`) is not narrowed, whatever the GCC.
- (A, B) round 7's rule cost nearly every C++ and CUDA unit its narrowing
  on GCC 11: libstdc++'s headers and `cuda_runtime.h` carry `#pragma GCC
  diagnostic`. `bench7` (`a0a697f`): 275 of 389 served (5 of 96 `nvcc`),
  9:28 against 9:05 cache off. For C++ the compile and `-E` agree (B: 17
  cases, `g++ -E` and `g++ -c` agree on every one; `gcc` differs in 11),
  and `nvcc`'s compile is made of its own `-E` passes: the rule is for C
  only.
Key label `key-19`.

### M4, round 9 (on `b652348`): SIGN-OFF by both; M4 passes its gate

Both re-ran every earlier reproduction they had: all fixed, or right as
before. A on plato and qbd: H1 (`-Wall` under `#pragma GCC diagnostic`, its
alias, push/pop, `-Werror`) a miss matching gcc; the new rule tried on GCC
8.5, 11.4, 13.2 and 14.2 (C++: the compile never names a header `-E` does
not, 2,280 to 3,800 cases each; every predefined macro alike after
`optimize`, `target` and their attributes; the compile expands exactly the
pragma arguments `-E` does); end to end with g++ 11.4 and 14 and `nvcc`
with g++ 11.4, every served object the compiler's. B on plato: 20 spellings
of `#pragma GCC optimize` in C each a new key, 426 C and 487 C++ predefined
macros compared after nine pragma sets (C++: none differ), 32 diagnostic
pragma runs with exit status and warnings matching. (B could not reach qbd
this round; A's qbd jobs 1092559 and 1092603 cover GCC 11.4 and `nvcc`.)
The benchmark on `b652348` (`bench8`): 350 of 389 served (80 of 96
`nvcc`), as at `6ff97d4`; audit 388 checked, 0 wrong. Non-blocking, done:
the comment on `key::Output`; the reason given for C++ (the compile and
`-E` agree, tried, rather than a mechanism: on GCC 14 both act on `target`
as they read). Left: two older clippy errors (`from_mode(0)` in
`tests/objcache.rs` and `upkeep.rs`) and older lints. **M4 signs off at
`b652348`.**

### M5 part 1, round 1 (on `8fe00ab`): BLOCKED by both

Both found nothing that changes what is served in the whole-key lookup
and `read_files` (a hit under the key with the files whole needs the same
plain key and the same files whole). Blocking, fixed in the commit after
this record:
- (B) `key::FrontEnd` let a `#pragma GCC diagnostic` region closed in one
  file through, but a `warning` or `error` in it turns the option on for
  the whole unit, and the `pop` does not undo that (`-Wpedantic` turned on
  in one header warns of a GNU variadic definition in another, on GCC 14;
  on GCC 8 to 12, whose `-E` ignores the pragma, a narrowed key would serve
  that unit; inferred, no such GCC on plato). Only `ignored` is excused in
  a region now. Key label `key-21`.
- (A, B) audit-mode hits and recording builds were counted "kept out"
  (`cache doctor` on Max's `et`, an audit build: "3376 kept out"). A hit is
  reused in audit mode too; a keyed compile of a recording build is no
  serving miss.
- (A, B) an entry the store has but could not use (damaged, unreadable,
  another cactup's) was "new", and its reason overwritten by "another
  build published it first": now its own kind (`Miss::Unusable`), counted
  as kept out, with the reason kept.
- (A, B) a narrowed file that differed was said to differ in "a definition
  the unit uses": it can be any line the key keeps (a comment, another
  directive). Said so now.
- (A) records of another key label made every miss "with other sources":
  a record carries its label, and another label is "built otherwise".
- (A) records were not in the store's size: counted now (`stats`, the size
  stamp, `gc --to-size` with each entry's record).
Non-blocking, done: records are flat files `units/<ab>/<unit>.<key>` (no
directory per object); `gc` removes a record only if its entry is still
not there then (one published during the walk keeps it); only plain files
are read as records; the doctor's messages name `serve`, its `-G` hint says
what to change; wording of `gc` and `stats`; a doc comment put back; the
kept definitions deduplicated. Left: records are 18% of the entries' bytes
on `smoke` (4.7 of 26 MB; about 130 bytes a file read); the time a miss
spends on records is in no event field.

### M5 part 1, round 2 (on `8890389`): BLOCKED by both

Both re-ran round 1's findings: all fixed. Both found nothing that changes
what is served (A, B: GCC's `handle_pragma_diagnostic` passes `imply = kind
!= DK_IGNORED` from GCC 8 to 14, so an `ignored` region turns nothing on;
B ran nested regions, a stray pop, `_Pragma` and a `#line` re-sync in
gnu11 and gnu89). Both ran real builds (outside the sandbox: a build locks
in `~/.cactup`): two builds publishing into one store at once, `gc
--to-size` removing exactly the records of the entries it removed.
Blocking, fixed in the commit after this record:
- (A) the doctor made one "kept out" group per compile when the reason
  names a path (an entry that cannot be read; a temporary file that cannot
  be made): reasons are grouped with paths left out; and a failed publish
  no longer hides the read error before it.
- (B) a header one key took whole and another narrowed was said to differ
  in "a line the key keeps", though its bytes were the same: a record
  notes whether each file was narrowed, and such a file is said to be
  taken so.
- (B) a serving build whose every compile was kept out was called "only
  recorded": the doctor goes by the mode frozen for the attempt.
Non-blocking, done: a hit whose dependency file could not be put in place
is kept out; `gc` counts records' bytes in what it removed; `stats` shows
the records' bytes per machine; the help text and docs name every kind of
"built otherwise" and "kept out"; the doctor's no-narrowing heading;
"narrowed" now also says the unit may use other definitions of the file.

Compression (Max: "see if compression helps without killing performance"):
records are zlib-compressed by `zlib-rs` 0.6 (pure Rust, already in the
tree through gix; the MUSL binary stays static) and keep 16 hex digits of
each file's digest (a record is a report, never a key). On `smoke` (363
records): 4.66 MB plain, 2.8x at best by compression alone (zstd level 9;
the 64-digit digests barely compress), 6x with the shorter digests and
deflate level 6: 0.78 MB, 3% of the entries' 26.2 MB. Per record 0.06 ms
to compress, 0.015 ms to read back (`opt-level = "z"`). Publishing, timed
over alternating real builds of 357 compiles: median 3 to 4 ms and 90th
percentile 5 to 7 ms with and without it (two earlier runs at 17 to 18 ms
were the host's, the plain binary did the same then). zstd's C library
would give the same size and faster reading, at the cost of a C build for
the aarch64 MUSL target.

### M5 part 1, round 3 (on `a81fb74`): A SIGN-OFF, B BLOCKED

Both re-ran rounds 1 and 2 (fixed) and found nothing that changes what is
served (records are read only after the hit or miss, and after the
compile; `FrontEnd` through the wrapper against gcc 14 alone, exit status,
messages and object alike in every case). Records: damaged ones (cut
short, a byte flipped, round 2's plain JSON, empty, a 60 MB length, a
directory, a FIFO) passed over by compiles, `stats` and `gc`; 8 builds
publishing one unit at once, 20 rounds, 20 entries and 20 records; `gc
--to-size` removed exactly the records of the entries it removed; 0.8 MB of
records against 26 MB of entries on `smoke`. Cost (B, local disk): a miss
reading 32 records of a 1000-file unit (15 KB each) pays about 13 ms
(0.4 ms a record); a record of a `smoke` unit 0.015 ms.

B blocked on one finding, fixed in the commit after this record: a file one
key narrowed and another took whole was said to be so, then given a cause
("a line the key keeps differs …") that contradicted it. Now only how this
key took it ("narrowed here", "whole: … here"), and the test asserts the
whole line. Also: a file counts as narrowed only when a definition was left
out of it; a record is not synced (one cut short reads as none); "kept out"
in the docs covers a store the build cannot write in, and the doctor has a
hint for it; `gc`'s dry run counts records' bytes as the real run does; each
grouped reason shows the first in full (A: the path the user needs); the
"another version wrote it" message names the right mode; "new" says a store
filled by an older cactup has no records. Left: records of round 2's
format (this branch only) cannot be read and keep a new record from being
written until `gc`; the time a miss spends on records is in no event field.

### M5 part 1, round 4 (on `a5685b1`): BLOCKED by both

Both re-ran rounds 1 to 3 (all as fixed; real builds 0 of 357, 357 of 357,
336 of 342; publishing median 2 to 4 ms) and found nothing that changes what
is served (B: `FrontEnd` through the wrapper against gcc 14 alone, every
case alike, with and without `-Werror`). Blocking, all in the miss report,
fixed in the commit after this record:
- (A, B) round 3's "narrowed only when a definition was left out" made a
  file narrowed with nothing to leave out look taken whole: an ordinary
  comment edit was "not known to differ", and a header the unit came to
  use all of was "narrowed here". A record keeps both facts now (narrowed,
  and a definition left out); only a file one key took whole and the other
  left definitions out of is "not known to differ"; a file with nothing
  left out is said to be whole ("the unit uses every definition in it").
- (B) a miss's list of differing files was cut to 8 silently, sorted by
  name, so the configuration's own headers (`/cactup-root/configs/…`) and
  system headers, those M5b is to see, fell off: 64 now, with a count of
  the rest, which the doctor says.
Also: a Fortran unit's source is named by its file (it was "source" for
all); the doctor's no-narrowing heading no longer speaks of thorn lists;
the "cannot write in the cache" hint matches only a failure to create
there; `gc`'s interrupt message counts records; a doc comment on records.
Left: the doctor says nothing of audit verdicts (`cache report` does).

### M5 part 1, round 5 (on `d2d684e`): BLOCKED by both

Both re-ran rounds 1 to 4 (all as fixed; real builds as before; A: 132
compiles through the wrapper against gcc 14 alone, 57 of them hits, every
one alike) and found nothing that changes what is served. Blocking, all in
the kind of miss or what it says, fixed in the commit after this record:
- (A) a miss whose own entry was passed over for messages about other
  files whole (decision 30) was "new": its own record was left out of
  those read. It is read in that case, and the miss is "with other
  sources: the files whole …".
- (A) the kind went by what the store held after the compile, so a build
  publishing meanwhile changed it: records are read when the miss is
  found, before the compile.
- (B) a file with nothing left out because every definition the unit does
  not use was kept anyway (defined twice, given, reserved) was said to be
  used whole by the unit: it says which were kept and why now.
Also: hints for a full file system; "new" in the docs says a cache filled by
an older cactup has no records; the doctor says when records could not be
written. B's run filled the home file system (debug builds and a test
store); 148 GB of reviewers' cargo output from passed milestones, in 80
directories, was cleaned (`cargo clean --target-dir`), their scripts and
logs kept.

### M5 part 1, round 6 (on `97f80a7`): A SIGN-OFF, B BLOCKED

Both re-ran rounds 1 to 5 (fixed; A: 132 compiles against gcc 14 alone, 57
hits, none differing; real builds 0, 357 and 336 of 342 reused; B: two
builds into one store, 357 entries and 357 records) and found nothing that
changes what is served. B blocked on two, fixed in the commit after this
record:
- the miss whose closest record left definitions out of a file this key
  left nothing out of (its unused ones kept: defined twice, reserved)
  did not name those kept definitions: it does now;
- the doctor ranked "with other sources" by the whole line, so one header
  taken in several ways (units keeping other definitions of it) was split
  over lines and ranked below a smaller one: it tallies by file now, the
  ways it was taken under it.
Also (A): the spec says a record may outlive an entry `verify` or audit
mode removed, until `gc`; hints for a full quota and a full file system
(either the cache's or the build's); a record that could not be written is
said beside the earlier reason, not over it, and counted by one constant.
Both reviewers cleaned their cargo output (the home file system has 579 GB
free).

### M5 part 1, round 7 (on `7766abb`): BLOCKED by both

Both re-ran rounds 1 to 6 (fixed; A: 132 and 64 more compiles through the
wrapper against gcc 14 alone, none differing; real builds as before) and
found nothing that changes what is served. Blocking, fixed in the commit
after this record:
- (A) a hit from the key with the files whole (decision 30) was logged as a
  use of the plain key, which has no entry: `cache gc --unused-for` and
  `--to-size` took the entry in use for unused and removed it (reproduced
  with real builds: the next build compiled `OverloadComm.c` anew). Since
  M4, and every such unit since part 1's lookup. The event names the key it
  was served from (`served`), and the use log records it.
- (B) a Fortran source was named by its file name alone, so the doctor
  took same-named sources of several thorns (`Basegrid.F90` in four of the
  Einstein Toolkit's) for one file: by its mapped path now.
Also: the quota hint names either file system; the plural wording of the
doctor's note on records not written; a type alias for the tally; the
spec's use-log path (`v<N>`).

### M5 part 1, round 8 (on `399cf37`): A SIGN-OFF, B BLOCKED

A re-ran rounds 1 to 7 (fixed), 48 more `FrontEnd` compiles against gcc 14
alone (244 in all, none differing), and round 7's `gc` finding on real
builds both ways: with the store aged 40 days by hand, and with an hour of
real time between filling the store and building again (`gc --unused-for
1h` now removes nothing in use; the hits from the key with the files whole
name it as `served`, and the use logs list it). B could not run real builds
this round (`~/.cactup` read-only in its sandbox, running outside refused)
and worked with test trees of real gcc 14 compiles; it blocked on one,
fixed in the commit after this record: a miss read its own record under
the key with the files whole as another entry's, when that entry had been
removed (`cache verify`) and its record was left until `gc`: "with other
sources" for an unchanged compile. Both of a compile's own keys are passed
over now (but the plain one where its entry was passed over for messages
about other files whole: that entry is the closest, decision 30). Also: the Fortran source's path worked out once per miss; a hit
whose dependency file could not be put in place no longer names the entry
it came from; `gc --dry-run`'s wording.

### M5 part 1, round 9 (on `ad236d2`): SIGN-OFF by both; part 1 passes its gate

A short round on the small diff (Max: the gate was taking long; the bar
unchanged). Both re-ran their fast reproductions of rounds 1 to 8 (all
fixed or as before; `FrontEnd` against gcc 14 alone in 244 compiles, none
differing; 8 builds publishing one unit at once, every entry with its
record and every record with its entry), the unit and integration tests and
the MUSL build, and found nothing blocking. B's round-8 finding reproduced
with real gcc and `cache verify` (A's `e22`): an unchanged compile after its
entry under the key with the files whole was removed is now "new". Left,
non-blocking: two object names with one key share one entry and one record
(the second's miss reads "new"); the doctor says nothing of audit
verdicts; the time a miss spends on records is in no event field; the
build's line does not say when it could not read lines of the log. **M5
part 1 signs off at `ad236d2`.**

### M5 part 2, round 1 (on `3ff11ce`): BLOCKED by both

Both confirmed nothing changes a build or what is served (the wrapper's
functions only made visible; the probe keys in record mode in a temporary
tree). Both could not build configurations (`~/.cactup` read-only in their
sandboxes) and made CACTUP_HOMEs of their own with copies of `m5a`'s
attempt. The same blocking findings from both, fixed in the commit after
this record:
- Fortran 77 (`F77`, `F77FLAGS`, `COMPILE_F77`) was not checked.
- A setup that prints on stdout broke the check (the answer shares the
  pipe): the setup's stdout goes to stderr, its stdin is `/dev/null`, and
  it runs under `set -e`, as the build script runs it.
- The green all-clear was given where nothing was checked: a kind of its
  own ("not checked") now, and the all-clear only without it.
- The MDB check passed over setups that stop early (dash ends at a
  sourced file that is not there: omnia, db1's `host` universe): run in
  bash, which goes on, with a line at the end that must be reached (an
  `exit` fails the test); a printed `NAME=value` line is no value.
- Ctrl-C was not honored while the setup ran for the path variables; no
  progress line was ever drawn: a `progress::Line` on its layout's clock.
- Findings traced to the wrong place: a probe's reason (`-march=native`)
  to "the build environment", now to the flag's variable; a flag in two
  languages' variables merged into one (now one finding per variable);
  what the wrapper makes of the compiler's text before any key (a command
  only a shell runs, `ccache gcc`, a name the shell has a meaning of its
  own for) was not applied, and is.
Also: shell versus setup decided by whether the setup changed the value;
flags split as the shell splits them (`-DNAME="a b"`); a default flag
traced within its language; `make`'s values read by `$(info)`; a note where
the option list's values stand in for `make`'s; `LD_PRELOAD` by its own
rule, `LD_AUDIT` traced; the Fortran "no file is narrowed" note dropped
(the cache's own limit, M5b's); `REFUSED` tied to the lists it repeats by
a test; the knob readers reused.

### M5 part 2, round 2 (on `3ac6cde`): BLOCKED by both

Both re-ran round 1 (F77, a printing setup, the all-clear, Ctrl-C, the MDB
check's end marker: fixed; tracing right in every case run but one) and
confirmed nothing changes a build or what is served. Blocking, fixed in the
commit after this record:
- (A) an empty setup, or one of comments, was a syntax error inside the
  group round 1 put it in: every configuration of the seven MDB machines
  without a build setup was "not checked". A `:` opens the group now.
- (A) a setup that fails at once was reported as a broken pipe: the
  request is written from a thread of its own, and the setup's own message
  is the finding.
- (A) the setup run for the path variables flagged a library `LD_PRELOAD`
  names without a `/` (which the loader searches for): one rule for both.
- (B) no progress line was drawn (prodash draws a line that changed): the
  seconds so far count on it.
- (B) a flag in the compiler variable itself (`CC = gcc --coverage`) was
  checked with every flag variable and traced to them: checked once, as
  the compiler variable's.
- (B) a setup that ends the shell with status 0 (`exit 0`, a `return`)
  was taken for an answer this cactup cannot read: a line the shell writes
  once the setup is through tells them apart.
Also: a finding that names its variable is kept when merged (`CCC_OVERRIDE_
OPTIONS` once, a relative `COMPILER_PATH` once); a process the setup leaves
running in the background no longer holds the doctor (the pipes are read
for a moment after the setup ends), and Ctrl-C kills the setup's process
group; a fix line for `ccache gcc`; a setup that could not run is traced
to the machine; a configuration not configured yet says the option list
stands in. Left: a variable a module sets (stubbed in the setup's run for
the path variables) can make an entry look fragile there; a thorn's own
flags (`make.code.defn`) are not seen.

### M5 part 2, round 3 (on `770193e`): BLOCKED by both

Both re-ran rounds 1 and 2 (an empty setup, a failing one, `exit 0`,
`LD_PRELOAD` by name, a flag in `CC`, progress counting, Ctrl-C in every
phase leaving nothing behind: fixed) and confirmed nothing changes a build
or what is served. Blocking, fixed in the commit after this record:
- (A) a setup leaving a process in the background that holds the pipes
  (`sleep 15 &`) was "not checked": the reader gave up and threw away what
  it had. The setup's output goes to files now, and only the shell is
  waited for (as the wrapper's own shell lookup does); an answer that
  parses is the proof the setup ran through (`exec 2>/dev/null`).
- (A) the setup's run for the path variables split every variable on
  blanks (`/opt/My Tools/bin` flagged): only `LD_PRELOAD`'s.
- (B) a compiler text only a shell runs (`CC = LANG=C gcc`) was given a
  fix that named a word of it as a flag: a hint of its own.
Also, at Max's word ("seems worth fixing"): the flags a thorn adds for its
own compiles in its `make.code.defn` (and its bindings' make files) are
checked, read as Cactus's `make.subdir` reads them for each of its source
directories, traced to the thorn's file; a flag set for one target only is
said not to be checked. The setup runs four times: said in the spec.

### M5 part 2, round 4 (on `e31f029`): BLOCKED by both

Both re-ran rounds 1 to 3 (all fixed: a background process, a blank in a
path, `CC = LANG=C gcc`, Ctrl-C in every phase) and confirmed nothing
changes a build or what is served. All blocking findings were in the new
check of thorns' flags, each reproduced with planted thorns and Cactus's
own `make.subdir`:
- (A, B) flags a thorn's compiles get were missed: in `make.code.deps`
  (AHFinderDirect's umfpack sets `CPPFLAGS` there; ML_ADMConstraints'
  one-target line is there), through a capability its bindings include
  (AMReX's makes `CXX` `nvcc`), through `CXX_OPTIMISE_FLAGS` and the like;
  and its compiler was never evaluated.
- (A) the directories were a walk, not the thorn's `SUBDIRS`: a linked
  source directory (GRHayLib's `Reconstruction`) missed, a directory no
  thorn compiles reported.
- (A) a thorn's flags went through the reader alone, never a key
  (`-march=native` on a host of mixed processors).
- (B) the configuration's flags were split at blanks and the thorn's as
  the shell does: a quoted option-list flag made every thorn that names a
  flag variable "keep the cache out".
Fixed in the commit after this record, by A's design: each directory a
thorn compiles (`src`, its `SUBDIRS` as make reads them) is read by `make`
through Cactus's own `make.subdir` with a goal that prints the compilers
and flags and no source; where a language differs from the
configuration's, it is checked again with `check_language` (probe key and
all), and what that finds beyond the configuration's check is the
thorn's, traced to the file that has the flag (the directory's
`make.code.defn` or `make.code.deps`, the thorn's bindings, a capability
they include). A directory make cannot read is "not checked". The Einstein
Toolkit's `et` (334 directories): 1.5 s for the whole doctor. Also: the
help, docs and spec say thorns' files are read with make and their
`$(shell)` runs; the "not a plain command" hint names every language's
compiler.

### M5 part 2, round 5 (on `56e4d0f`): BLOCKED by both

Both re-ran rounds 1 to 4 with their planted thorns (`make.code.deps`,
capabilities, `*_OPTIMISE_FLAGS`, linked `SUBDIRS`, a directory not
compiled, `-march=native` by key, AMReX's `CXX = $(CUCC)`, quoted
option-list flags: all right now) and confirmed nothing changes a build or
what is served. Blocking, both raised by both, fixed in the commit after
this record:
- flags set for some objects only (`a.cc.o: CXXFLAGS += …`, `%.cc.o: …`;
  ML_ADMConstraints has one) were passed over without a word (round 3's
  note had gone with the rework): each object's values are asked of `make
  -n` with a rule of its own, and checked.
- a thorn's finding was given the option list's fix, and one the probe's
  key found named no file: a fix of its own for a thorn's flag, and the
  flag's variable found among the thorn's values.
Also: a file that names the flag only in a comment is not taken for the
one that sets it; a language listed once in a merged finding; a directory
the fragment would stand down for (an `include`, a `define`) is said to
keep the cache out; a language checked again with the same values is
checked once (60 thorns with one capability's flag: once). `et`: 2.0 s.

### M5 part 2, round 6 (on `da23d88`): BLOCKED by both

Both re-ran rounds 1 to 5 (all right) and confirmed nothing changes a
build or what is served. Blocking, raised by both: an object's values were
asked of make outside the context the build's sub-make gives them (`SRCS=`
on the command line emptied `$(OBJS)`; nothing set for the goal reached
the objects; `SRCS` read from `make.code.defn` alone missed sources the
configuration chooses; an explicit rule's `$*` is not the stem), and only
assignments to the compile variables themselves were looked for (missed:
`CXX_OPTIMISE_FLAGS`, a variable a flag variable refers to, `private`). B:
`make -n` ran a prerequisite's `+` and `$(MAKE)` lines. Fixed in the commit
after this record by running `make.subdir` as the build's sub-make runs
it, for its goal, with `make -n`, the compile recipes redefined to print
each object's values, Cactus's dependency-file rules canceled, and the
recipe lines `make -n` runs regardless sent to a shell that runs none of
them (it runs `$(shell)`, read through a pipe): every directory, every
object, no trigger. Also: one finding with the objects that get it (no
repeat per object; a flag one object takes away is the others'); a fix
for where the flag's file comes from (the thorn's, its configuration's,
a capability's); the stand-down names its line; short make runs are no
longer rounded up to the 50 ms poll. `et`: 1.9 s.

### M5 part 2, round 7 (on `da03847`): BLOCKED by both

Both re-ran rounds 1 to 6 and compared every object's values on `et` (302
directories, 2043 objects) with real `make -n` of the build's sub-make:
equal. Nothing changes a build or what is served; no `+` or `$(MAKE)` line
ran, under make 4.2.1, 4.3 and 4.4.1 and with `GNUMAKEFLAGS` set.
Blocking, fixed in the commit after this record:
- (both) `$(shell)` ran in `/bin/sh` (dash on plato), not the
  configuration's `SHELL` (bash): bash syntax missed a flag or named a
  wrong one. `.SHELLFLAGS` now names `$(SHELL)`, and the recipe shell runs
  `$(shell)` in it.
- (A) an existing, unreadable `make.code.deps` gave an all-clear where the
  fragment stands down (grep's status 2): the doctor asks grep as the
  fragment does, and says the file cannot be read.
- (B) Ctrl-C left the per-directory makes and their `$(shell)` running,
  and `__cc-doctor`'s temporary directories behind: the makes stay in the
  setup's process group, which is sent a TERM first (`__cc-doctor` takes
  it as an interrupt and cleans up), then a KILL 0.3 s later. A run with a
  20 s `$(shell)` stops in 0.12 s, nothing left.
- (B) a configuration not yet configured got an all-clear though no
  thorn was checked: "not checked" now.
Also: `USESTHORNS` and `SRCDIR` (`src/.`) as the build passes them; a
thorn with its own `Makefile` "not checked"; capabilities "not checked"
while the bindings are not written; one capability's flag one finding for
all the thorns that require it; a note carries no fix; a flag of a
variable the option list could set is not said to be "not the option
list's". Not taken: the language checks run one at a time (the probe's key
moves the process's directory), so 300 thorns with 300 different sets of
flags take about 13 s, with progress.

### M5 part 2, round 8 (on `4a7ef30`): BLOCKED by both

Both re-ran rounds 1 to 7 (all as recorded) and compared each object's
values with the build's own (`make.thornlib`'s sub-makes under `make -n`):
equal on `et`, `m5a`, `m5b` and the planted thorns. Blocking, both raised
by both, fixed in the commit after this record:
- a link to nothing as `make.code.deps` was said to keep the cache out:
  round 7 kept it by `symlink_metadata`, where the fragment's `[ -e ]`
  follows the link (and wraps). Now `Path::exists`, as `[ -e ]`.
- Ctrl-C during the language checks left `__cc-doctor`'s temporary
  directory (it polled the flag only among the thorns, and was killed):
  it looks between languages and between a language's compiles, and gives
  no answer once interrupted (no partial one that looks whole); the KILL
  follows the TERM after 0.5 s. Stopped at 0.3 s and 0.8 s: 0.3 s and
  0.2 s, nothing left.
Also: every flag of a variable the cache refuses is said (it said the
first); each thorn's bindings (`bindings/build/<thorn>`) are checked as
its directories are; a thorn's unreadable `src/make.code.defn` is "not
checked"; the bindings-not-written finding is the configuration's; the
compiler's name is checked in the configuration's `SHELL`; grouped
findings say "thorns". Not taken: the doctor's makes use `make` from
`PATH` (every MDB machine has plain `make`).

### M5 part 2, round 9 (on `5ff56f9`): BLOCKED by both

Both re-ran rounds 1 to 8 (all as recorded; the bindings compared with the
build's own makes: equal). Round 8's "nothing left" held at the delays
tried, not across a run. Blocking, fixed in the commit after this record:
- (both) Ctrl-C while a compiler was being identified still left
  `__cc-doctor`'s temporary directory (identification runs several
  compiles and does not look for an interrupt; the KILL came first, the
  more so on a busy host): `cache doctor` now owns that directory (the
  checks' `TMPDIR`) and removes it once they are gone, however they
  ended. A sweep of Ctrl-C from 0.05 s to 1.35 s: stopped in 0.27 s at
  most, nothing left.
- (B) the search for every refused flag read a refused flag's value as a
  flag (`-mllvm -inline-threshold=500`), and for nvcc left `-Xcompiler`
  reading the next flag (a false "leave --objdir-as-tempdir out"); (A) a
  refused flag with a comma (`-Wp,-D_FORTIFY_SOURCE=3`,
  `-fsanitize=address,undefined`, as distribution CFLAGS have them) ended
  it. The flag is now taken from the reason's own text; a value is taken
  with the flags the reader knows to take one
  (`compile::NOT_CACHED_WITH_VALUE`, read by nothing else); the search
  stops where the next word may be an unknown flag's value, and after an
  nvcc host flag (`doctor::every_reason`, tested with the real reader for
  gcc and nvcc).
Also: a British spelling ("canceled" now); a grouped finding names
six thorns before "and N more"; bindings are not checked for a thorn the
doctor does not build-check.

### M5 part 2, round 10 (on `cfdc986`): BLOCKED by both

Both re-ran rounds 1 to 9 (all as recorded; Ctrl-C swept across whole
runs, idle and on a host with a load of 25 to 43: at most 0.69 s in a
debug build, nothing left; each object's values equal to the build's own
on `et`, `m5a`, `m5b`). Blocking, fixed in the commit after this record:
- (B) a setup that sets a `TMPDIR` where no file can be made got an
  all-clear, where the wrapper keeps every compile out (its key's list of
  what a compile reads, the shell's lookup of the compiler); round 9's
  own `TMPDIR` for the checks hid it. The shell now hands the setup's
  `TMPDIR` to the checks (by `env`, which a `readonly TMPDIR` does not
  stop), which make a file there; the finding is traced to the setup or
  the user's shell, with its fix.
- (A) the fix for a refused flag that takes a value named the flag alone
  ("leave -mllvm out"), which does not compile: it names the value too.
Also: a word after a flag the reader does not know is said, as what may be
that flag's value (round 9 dropped it: `-fipa-pta -fno-tree-pre` said one);
a two-word reason (`-Xptxas -O3`) does not stop the search; nvcc's joined
form is named once; a user's `TMPDIR` with no room for `cache doctor`'s
own files falls back on `/tmp` (it said "could not be checked"); a
temporary directory the checks cannot make is "not checked" (it skipped
every check). A grouped finding names five thorns before "and N more"
(all, up to six). Not taken: the setup runs four times (the checks, and
three runs for its path entries), so a 30 s setup takes two minutes, with
progress.

### M5 part 2, round 11 (on `efd7087`): BLOCKED by A; B did not report

B stopped without a report when the session ended overnight (it left
nothing running; its output was cleared); round 12 asks it for rounds 10
and 11 both. A re-ran rounds 1 to 10 (all as recorded; round 10's `TMPDIR`
finding checked against the wrapper itself: true) and found, blocking and
older than round 10:
- one flag the reader refuses hid every reason the probe's key would
  give after it (`-march=native` on a host of unlike processors, beside
  `-fipa-pta`), and nothing said so: the key's error was the reason
  already said. The probe's key is now made without the flags already
  said (`doctor::every_reason` gives what is left), so the key's own
  reasons are said beside them.
Also: the build's `TMPDIR` is read by the checks themselves, as exported
(a setup's unexported `TMPDIR` is not the build's; no `env`, which a
`PATH` without it would break); a flag given twice is said once; no
"value of" note after a flag with a value joined to it (`-wd810`,
`-L/opt/lib`); every value of a flag given twice with one (`-mllvm a`
and `-mllvm b`) is named in the fix; "TMPDIR is not set" gets a fix of
its own; a temporary directory `cache doctor` cannot make names where it
looked.

### M5 part 2, round 12 (on `a9170fa`): BLOCKED by both

Both re-ran rounds 1 to 11 (all as recorded; B judged round 10 too; Ctrl-C
swept to 10 s, idle and at a load of 40: at most 0.67 s, nothing left;
each object's values equal to the build's own on `et`). Blocking, fixed in
the commit after this record:
- (both) round 11's key without the flags already said kept the
  `-Xcompiler` of a refused nvcc host flag, which then read the next word
  or the probe's `-c` (a false "leave -c out"): it goes with its flag.
- (both) a reason about no flag ("there is more than one input file",
  from `-L /opt/lib`), held back once a flag was taken out, came from the
  key traced to "the build environment" with no fix: it is said with its
  variable, names the word read as an input and the flag before it, takes
  them out and reads on (`-fipa-pta`, `-march=native` after them are
  said), with a fix.
- (B) a relative `TMPDIR` (a quoted `~/tmp`) keeps every compile out (the
  compiles run in `configs/<name>/scratch`) and was not looked at: it is
  checked there.
Also: "leave it out" for an unknown flag no longer says "of the option
list" (it may be Cactus's default); "TMPDIR is not set" with no room in
`/tmp` is the host's; no room for `cache doctor`'s own files is said of
this host, not of the setup.

### M5 part 2, round 13 (on `0fa9e69`): BLOCKED by both

Both re-ran rounds 1 to 12 (all as recorded; round 12's relative `TMPDIR`
checked against the wrapper run in a `scratch`: agrees). Blocking, both
raised by both, in round 12's second-input reading, fixed in the commit
after this record:
- it took the first word without a dash for the second input, and the
  word before it out with it: a value the reader takes apart from its
  flag (`-I /dir`, `-isystem /dir`, `-D X`, `--param`, nvcc's
  `--gpu-architecture sm_80`, `-ccbin g++`) was named, and a flag before
  the word (`-march=native`, `--objdir-as-tempdir`) dropped. The word is
  now the one without which the reader no longer says "there is more
  than one input file", taken out alone; a flag before it the reader does
  not know is said in its turn.
- its fix said the cache reads such a value joined to its flag (`-L/dir`),
  which it does not: the fix is to leave the word out with its flag.
Also: a flag the reader reads out of a longer word (`-o` of
`-override-limits`) is named by that word in the fix; the fix for an
unknown flag with a word after it that is no flag names both
(`-L /opt/lib`); a variable with shell syntax the recipe's shell expands
first (a command substitution) is "not checked" instead of read word by
word.

### M5 part 2, round 14 (on `c4d6420`): BLOCKED by both

Both re-ran rounds 1 to 13 (all as recorded). Blocking, both raised by
both, in round 13's second-input search, fixed in the commit after this
record:
- the word whose taking out stops "there is more than one input file"
  is not always the second input: after a value the reader takes apart
  from its flag (`-D NDEBUG libm.a`, `-I /dir x.o`, nvcc's `-ccbin g++
  x`), taking the value out lets the flag take the stray word, so the
  value was named (and the stray word, left for the probe's key, gave a
  false "the preprocessor failed").
- with two stray words no one word stops it, so nothing was named and the
  search ended: on frontera, graham and the Intel Fortran lists most
  findings went unsaid.
The reader is now asked directly: without the probe's source, of each
longer start of what is left (the compile's own words first, so a start
ending in a flag that wants a value has nothing after it); the first
start that has an input ends at the second input. Two are found one after
the other, and what follows is read on (frontera: every flag said).
Also: the fix for a refused flag that takes no value names it alone
(`--coverage`, not the stray word after it); an unknown flag's fix names
its value word too (`-L /usr/lib and -L /usr/lib64`); a flag read out of a
longer word is named whole at thorn level too; a "value of" note only
where the variable has the two words side by side; the second-input fix
only where a word is named.

### M5 part 2, round 15 (on `d9f93a1`): BLOCKED by both

Both re-ran rounds 1 to 14 (all as recorded; B read all 870 flag
variables of the MDB's option lists with the real reader: what is left
parses, every word named is in its variable). Blocking, fixed in the
commit after this record:
- (A) the prefix reading gave the reader its own `-o probe.o`, so a start
  ending in a word it reads `-o` out of (`-openmp`, `-override-limits`:
  fuchs, tianhe1a) was "-o is given more than once", taken for an input
  and named as one; and that reason, naming `-o`, which is no word of
  the flags, ended the search (tianhe1a: one finding of a dozen). The
  prefixes are read with `-c` alone (an input shows as any answer but
  "there is no input file"), and such a word is named whole, taken out,
  and the rest read on.
- (B, there since round 2) each variable was read on its own, so what
  nvcc's reader says of the compile as a whole failed on the part that
  lacked another's word: a bare `CUCC = nvcc` (qbd, delta, athena,
  saturn, generic) got a false "without --objdir-as-tempdir", graham's
  `CXX = nvcc -x cu` a false "not CUDA". The compiler's words and every
  flag variable are read as one, as the recipe has them; each reason is
  the variable's that has the word it is about, a reason about the whole
  compile the language's flags'.
Also: a two-word reason (`-Xptxas -O3`) takes out only that pair, not
`-Xptxas -v`; the search's guard is 64 reasons (an Intel list has more
than 16); a test's script is made executable as the others are (it
failed once with "Text file busy").

### M5 part 2, round 16 (on `379bfc6`): stopped; the first reason only

B re-ran rounds 1 to 15 (all as recorded; round 15's nvcc fix checked
against all 165 compiles of the MDB's option lists) and blocked on one
finding of the every-flag search: a refused flag in two variables of one
compile was said once, of the first (`CPPFLAGS = -pg`, `CFLAGS = … -pg`).
A was stopped when Max decided (decision 32) that the doctor says the
reader's first reason for now, so that the feature merges sooner. The
commit after this record does that: `doctor::first_reason` (the reader's
first reason, with the word it takes for a second input or reads `-o` out
of, as rounds 13 to 15 found them), given to every variable with the word
(the two-variable case is said of both), a "not checked" note for what
comes after, and no probe key past it. Kept from the search: the compile's
words read whole (round 15; nvcc's false "without --objdir-as-tempdir"
since round 2), the fix texts (`shown_for`), shell syntax "not checked".

**Every flag, on `feature/doctor-every-flag` (at `379bfc6`), for after
the merge:** open there are round 16's B1 (a word in two variables: say
it of each, or take out one occurrence at a time), and its notes: a fix
line naming the first `-o` word where two are in the flags; a stale
comment over the reading; a host compiler's reason now ends the search
for all the variables read together; `reason_subject` of a word inside a
joined value (`--compiler-options=-pg`) falls back to the flags; nvcc's
`-m 64` (a gap of the wrapper's reader, M4: `-m` unknown, `64` an input).

### M5 part 2, round 17 (on `7cbe92e`): BLOCKED by both

Both re-ran rounds 1 to 16 (every first reason checked against the
wrapper's own, by hand and on every MDB option list's compiles: all agree;
only by decision 32 do the outputs differ). Blocking, fixed in the commit
after this record:
- (B, and A's N1) a second input's word was given to every variable with
  that word, also one where it is a flag's value (`-isystem /dir` in
  CPPFLAGS, a stray `/dir` in CFLAGS), whose fix would drop a needed
  directory: a reason about one occurrence (a second input, a second
  `-o`) goes to the variable at its position (`first_reason` gives it);
  a refused flag still to every variable with it.
- (A) with the first reason only, a thorn that takes the configuration's
  first refused flag out (`$(filter-out -flto%,$(CFLAGS))`) showed the
  configuration's next one as the thorn's own: a thorn's reason about a
  word the configuration's own compile has too is dropped.
- (A, older) a CPPFLAGS finding was traced to `C_OPENMP_FLAGS` (the
  prefix was the language's): it is the variable's (`CPP_OPENMP_FLAGS`).
Also: the "only the first" note is said once for the run, of the cache's
reader, with what to do (it was one for each thorn's directory, of "the
build environment"); stale docs (the module's, `reason_subject`'s link,
"Next step").

### M5 part 2, round 18 (on `b672235`): BLOCKED by both

Both re-ran rounds 1 to 17 (every first reason checked against the
wrapper's, on every MDB option list too: all agree). Blocking, fixed in the
commit after this record:
- (both) round 17's filter dropped a thorn's reason whenever its word was
  anywhere in the configuration's flags, even where the configuration's
  check had read them all (a thorn's stray `/usr/local/include` beside
  the configuration's `-isystem /usr/local/include`): kept out, nothing
  said. It applies only where the configuration's check stopped at a
  reason.
- (B) round 17 gave a word read as `-o` to the first variable with it
  only; every copy of it is read so (tianhe1a's `CPP_` and `C_OPENMP_FLAGS
  = -openmp`): of every variable with it again.
- (A, older) the trace admitted CPPFLAGS for every variable, and it sorts
  first: a CXXFLAGS or `C_*_FLAGS` finding was traced to CPPFLAGS where
  it had the same word (whose fix drops a needed directory). CPPFLAGS
  only for a finding of no variable.
Also: the "from" of a second `-o` or second input looks for the word it is
about (`-openmp`, not `-o`), so it names `CPP_OPENMP_FLAGS` and the like;
the spec says the unknown-flag fix again (`-L /opt/lib`).

### M5 part 2, round 19 (on `120ebf8`): BLOCKED by both

Both re-ran rounds 1 to 18 (all as recorded; each word named checked to be
a true cause with the real reader). Blocking, fixed in the commit after
this record, all in where a finding is traced and what its fix names:
- (B) a word read as `-o` given to a variable whose first `-o` word is
  another (`-override-limits` beside `-openmp`, tianhe1a's shape) had
  that other word in its fix: the fix names the word the reason names.
- (B, older) the trace named an option list variable whose switch is off
  (`C_DEBUG_FLAGS` with `DEBUG = no`, where `C_PROFILE_FLAGS` gives the
  flag): a `*_DEBUG/OPTIMISE/PROFILE/WARN/OPENMP_FLAGS` whose
  `CCTK_*_MODE` is not `yes` is passed over.
- (A, older) a second input was traced to the first variable (by name)
  with its word, which may hold it as a flag's value (`-isystem /dir` in
  CFLAGS, a stray `/dir` in `C_OPTIMISE_FLAGS`, omnia's shape): to one
  where it stands alone (not after `-I`, `-D`, `-isystem` and the reader's
  other flags with a value apart, `compile::WITH_VALUE`).

### M5 part 2, round 20 (on `5b9e237`): BLOCKED by both

Both re-ran rounds 1 to 19 (all as recorded). Blocking, both raised by
both, in round 19's switch filter, fixed in the commit after this record:
`*_NO_OPTIMISE_FLAGS` ends in `_OPTIMISE_FLAGS`, so it was judged by
`CCTK_OPTIMISE_MODE` the wrong way round (Cactus gives it where that is
not `yes`): with OPTIMISE off a flag of it was traced to "the option list
or Cactus's defaults, CFLAGS", with it on to the variable the compiles do
not get. It is on exactly where optimizing is off now. And the switches
are make's own (`CCTK_*_MODE` evaluated with the compilers and flags):
Cactus writes `CCTK_WARN_MODE` indented in a conditional, which the line
reader of `make.config.defn` never saw, so `*_WARN_FLAGS` was never
passed over.

### M5 part 2, round 21 (on `8eba40d`): BLOCKED by both

Both re-ran rounds 1 to 20 (all as recorded; round 20's switches checked
against make itself, `WARN` from the shell too). Blocking, both raised by
both, older than this round, fixed in the commit after this record: where
the configuration is not configured (a first build queued, or one that
stopped in configure), or make cannot read it, only the option list's
base variables were checked, never its `C_OPTIMISE_FLAGS` and the other
variables its switches add (OPTIMISE is on by default: where most option
lists keep their flags; holodeck's `-xHost`), and the note said the
option list's flags were what was checked. It is "not checked" now, with
what is checked and what is not. (Composing them as configure would was
left: it takes configure's defaults, more to get wrong.)

### M5 part 2, round 22 (on `05b9b98`): BLOCKED by both

Both re-ran rounds 1 to 21 (all as recorded). Blocking, both raised by
both, older than this round, fixed in the commit after this record:
- Cactus's configure sets `F77`, `F77FLAGS` and every `F77_*_FLAGS` from
  F90's, whatever the option list says (checked by running configure).
  The doctor traced a Fortran 77 finding to "Cactus's default F77FLAGS
  … the option list can set F77FLAGS itself" (it cannot: it is
  overwritten), and where not configured checked the option list's own
  F77 and F77FLAGS (false findings; a real one only said of Fortran). Now
  a Fortran 77 finding is traced to its F90 variable and said with
  Fortran's, and where the option list is read, Fortran 77 gets F90's.
- a configured configuration whose newest attempt configures it again
  from another option list (a `build submit` queued, a configure that
  failed) was checked with the old configure's flags (a false all-clear,
  or a fix aimed at a flag that is going). What configure last ran with
  is Cactus's own `config-info`; where the attempt's option list is
  other, it is as not configured, said so. (Where there is no
  `config-info`, the option list installed with the configuration, or the
  last successful attempt's.)
Also: the `id` of a build attempt is read outside tests now.

### M5 part 2, round 23 (on `7723bbf`): BLOCKED by both

Both re-ran rounds 1 to 22, and planted configurations as a real build
leaves them (Cactus's own configure run). Blocking, fixed in the commit
after this record:
- (both; round 22's) the reconfigure check compared Cactus's
  `config-info` with the option list as written, but Cactus writes it
  after its own parse (comments out, `${WORD}` expanded, a user's
  `~/.cactus/config` added): 21 of the MDB's 47 option lists would have
  been said to configure again, and checked from the option list only.
  The check compares cactup's own records, as written: the attempt's
  option list with the one installed with the configuration when an
  attempt succeeded (or the last successful attempt's).
- (both) a configure that failed records its new options in
  `config-info` (`CONFIG-STATUS : 1`) and leaves the old
  `make.config.defn`: the doctor checked the old flags. A failed last
  configure is now as not configured, said so.
- (B) where the option list is read, it was read as written (sciama's
  `CC = ${CC}` a false "not a plain command"): read as Cactus's
  `setup_configuration.pl` reads it (`#` comments, `keyword value` too,
  the first setting standing), `${WORD}` expanded from the build's
  environment and `$$` a `$` where the checks run.

### M5 part 2, round 24: stopped; the doctor asks, decision 33

Round 24 (on `acb88dc`) was stopped when Max decided the doctor should
ask the build's tools rather than model Cactus. The commit after this
record: `doctor::sources` traces a finding's word by emptying each
variable of `make.config.defn` that has it on make's command line and
seeing whether a copy leaves the compile's variable (no prefixes,
switches, `NO_OPTIMISE`, F77-from-F90 or value-flag rules in `trace`
any more; the option list's where its value has the word, else "as
configure wrote it"); where there is no configure to ask, no compiler or
flag is checked, said so (no option-list parsing or `${WORD}` expansion,
no F77 copy). Checked against the reviewers' planted real-configure
layouts of rounds 17 to 23 (switches on and off, `NO_OPTIMISE`, WARN off,
F77 overwritten, stray words beside `-isystem`, `-openmp` in two
variables, queued and failed reconfigures): each traced where make says;
`et`, `m5a`, `m5b` clean. 188 fewer lines in `doctor.rs`, 146 more.

**Working practice (Max, 2026-10-15):** before a fix goes back to the
gate, it is checked against the real tools it concerns (Cactus's
configure, make, the wrapper, the MDB's option lists, configurations
planted as a real build leaves them), not unit tests only.

### M5 part 2, round 24 on the redesign (on `af96c1f`): BLOCKED by both

Both re-ran rounds 1 to 23 (each traced where make says, real-configure
plants too; doctor and wrapper agree on every first reason). Blocking,
fixed in the commit after this record, nothing of Cactus modeled in any:
- (both) a flag a variable has in its own text and gets from another
  too (`-flto` in CFLAGS and `C_OPTIMISE_FLAGS`) was traced to the other
  only: where copies are left with the sources found all emptied, the
  variable itself is named too (not for a word read as a second input,
  which is one copy).
- (B) "the option list" was said of a variable whose option-list value
  had the word, which configure need not have used (F77FLAGS: configure
  writes F90's): the trace says the variable as configure wrote it, and
  that the option list has it the same only where it does.
- (B) where no compiler is checked (no configure to ask), what keeps
  every compile out whatever the compiler (a relative `COMPILER_PATH`,
  `GCC_EXEC_PREFIX`) was only "worth knowing": it is checked there too.
Also: make's own reason where it cannot read `make.config.defn`, traced
to the configuration, and that the thorns are not checked then either;
the help, user docs and spec no longer speak of "Cactus's defaults".

### M5 part 2, round 25 (on `8b84941`): BLOCKED by both

Both re-ran rounds 1 to 24 (all as recorded; the wrapper agrees on every
first reason, MDB lists too). Blocking, fixed in the commit after this
record, by asking make and cactup's own records:
- (A) a word read as a second input that two variables have (`-L
  /usr/lib` in CFLAGS and `C_OPTIMISE_FLAGS`) was traced by which
  variable's emptying lowers the copies, which cannot tell one copy from
  the other: the check now carries where the copy is (`Raw::at`), and
  `source_of_copy` gives each candidate's value back to make between two
  marker words to see which variable that place comes from.
- (A, round 24's) a `make` the setup's `PATH` does not have was traced to
  the configuration's file: to the setup, with "put make on its PATH".
- (B, round 23's) an attempt that had run (configure done, a compile
  failed) was said to "configure again when it runs", its `-flto` unsaid:
  an attempt configures again only where no configure has run since it
  was made (Cactus's `config-info` not newer than its option list); a
  failed configure of it is said as such.
Also: B's two-stray case (a word a value in one variable, stray in
another) is traced to the stray one only now; stale comment and docs.

### M5 part 2, round 26 (on `87b3339`): BLOCKED by both

Both re-ran rounds 1 to 25 (`source_of_copy` right on every real-configure
plant). Blocking, both raised by both, the same root: which option list
the configuration was configured from was guessed (the installed list,
then `config-info`'s time), and every attempt configures with its own
before it compiles, an older queued one possibly after a newer was made.
False then: a reverted change after a compile failed (an all-clear, or
a flag the queued attempt will not have), an unchanged resubmit said to
configure again, an attempt that had ended said to configure "when it
runs", a late configure by an older attempt taken for the newest's. Fixed
in the commit after this record by a record of cactup's own: each build
script touches its attempt's `configured` stamp right after its configure
step succeeds (`BuildAttempt::configured_stamp_path`; nothing on the build
path reads it, nothing compiled changes); the configuration was last
configured from the option list of the attempt with the newest stamp
(where none, the one installed with it). Checked on the reviewers' four
scenarios with stamps as a build leaves them: each right. Also: the
compiler variable's place counts its name (`source_of_copy`).

### M5 part 2, round 27 (on `2cd6d8b`): BLOCKED by both

Both re-ran rounds 1 to 26 and planted with stamps as a build leaves them
(again, revert, resubmit, late, late2, cfgfail, first, firstfail, oldmix:
each right). Blocking (B), older than round 26, fixed in the commit after
this record: after a `--virtual-executable` build the doctor took that
attempt's setup, which is empty (it neither configures nor compiles): it
checked in the user's own shell (a false all-clear; a false "not on
PATH"). It takes the newest attempt that builds. Blocking (A): a
configure that left no stamp (an older cactup's build, a queued job whose
script was frozen before the stamp) was invisible, and the option list
of the newest stamped or installed attempt taken for the one configured
from: now, where Cactus's `config-info` (every configure writes it,
before the stamp) is newer than the newest stamp (else than the installed
option list of the newest successful attempt), what it was configured
from is said not to be known, and nothing checked. And (A) the same
option list configures differently in another setup (Cactus expands
`${WORD}` at configure time; sciama's `CC = ${CC}`): the stamped attempt's
setup and universe are compared too. Also: the stamp is a
byte written (`echo >`), so its time is the file server's where the step
runs again (CLAUDE.md, locking); the newest stamp is found by the stamps'
times alone, one attempt opened; a stale comment. Known and left: what
the user's own shell gives a configure (a `${WORD}` from it) is recorded
nowhere; the doctor runs in the user's shell now, and reads the
configuration's flags as configure wrote them.

### M5 part 2, round 28 (on `1a5a166`): BLOCKED by both

Both re-ran rounds 1 to 27. Blocking, both raised by both: Cactus's CST,
in every build's own `make` step, rewrites `config-info` after the stamp
wherever the setup sets an option a thorn declares (`MPI_DIR`: mel5's
setup; `HDF5_DIR`, AMReX on omnia), so round 27's "a configure that
recorded nothing" was said of every such build, and nothing checked; and
after `cactup build prune` the same. Rounds 23 to 28 all found what
reconstructing who configured last, from what, gets wrong; the commit
after this record drops that: the compilers and flags are checked only
where what is configured is provably the newest building attempt's own
configure (its stamp, with `make.config.defn`, which only configure
writes, not newer; before the stamp, it succeeded and its option list is
the installed one, `make.config.defn` not newer than that); otherwise
"not checked", said why. A queued or resubmitted attempt is checked once
it has configured. Every planted scenario of rounds 23 to 28 (CST
rewrite, prune, hand configure, late configure, revert, resubmit,
virtual, failed, not configured) says true things; `et`, `m5a`, `m5b`
clean.

### M5 part 2, round 29 (on `8617302`): BLOCKED by both

A re-ran rounds 1 to 28 (round 28's blockers fixed; the gate's
assumptions checked in Cactus: only configure writes `make.config.defn`,
through a temporary file in `config-data` and `mv`; a failed `-config`
exits 2, so `set -e` leaves no stamp). Blocking, fixed in the commit
after this record: the reason for "not checked" where the newest
attempt has no stamp listed causes ("queued, or ended before or in its
configure") that are not the only ones (an older cactup's build that
configured and failed in a compile; a build running): it says what the
record shows, that the attempt left no record that it configured the
configuration as it is now (B raised the same: a pre-stamp build that
configured and failed, `m5pb25-ranfail`). Also: help and user docs say
when the compilers and flags are checked (B: they had round 27's gate,
and called a failed last configure unchecked, where the newest build's
stamp vouches for what it left); the stamp step ends `|| true`.

### M5 part 2, round 30 (on `7ff5cb2`): B signed off; BLOCKED by A

B: sign-off (262 plants replayed, real configurations, Ctrl-C, wrapper
runs: all as recorded or right). A, with a real `cactup build`
interrupted in its realclean step: the "no record" reason offered only
"queued or running" or "an older cactup's", which the record ruled out
(it ended, its script has the stamp step). Fixed in the commit after
this record: the reason is the record's: not ended; ended without
recording that its configure succeeded; its script, an older
cactup's, records none; or its script is not there to tell. Also (both): help and user docs state the rule
(checked where what is configured now is the newest build's own
configure) rather than a list of cases; the setup check's "did not run
to its end" names an unset variable under `set -u` too.

### M5 part 2, round 31 (on `13944e1`): BLOCKED by both

Both, the same finding: an attempt that recorded no end was said to be
"queued, or running before that"; a build killed in its configure
(SIGHUP, a second Ctrl-C, the OOM killer, a job cancelled with the
scheduler's own command) records no end either, and `build show` calls
it STALE (A: a real build hung up in its realclean; B: planted). Fixed
in the commit after this record: the doctor guesses nothing of what such
an attempt is doing ("it has recorded neither an end nor that its
configure step succeeded"), and the reason goes after "so what it
compiles with is not known", in one pair of parentheses. Not part 2 (A;
on master since `9f9dc8d`), a follow-up after the merge:
`guard_no_live_attempt` (`src/commands/build.rs`) takes a finished
foreground attempt's job id, the pid of the cactup that ran it, as live
when that pid is reused, so `cactup build` refuses and `-f` runs the
stop command on an unrelated process group (in the sandbox's pid
namespace it stopped itself); `build list` shows such an attempt
RUNNING. It should check the recorded outcome and `submitted` first.

### M5 part 2, round 32 (on `92c3089`): PASSED, both signed off

A: sign-off (220 plants replayed, equal to round 31's but for its
rewording; real builds hung up in realclean and in configure, a double
Ctrl-C in CST, a hand realclean after a stamped build, the doctor run
during a build; Ctrl-C; every MDB option list against the wrapper; direct
wrapper runs). B: sign-off (271 plants replayed, likewise; real builds
killed by SIGHUP and SIGINT in configure; Ctrl-C; wrapper runs). Neither
blocks, both for after the merge: the `guard_no_live_attempt` pid reuse
of round 31 (seen again by both: `-f` stopped A's harness twice); and
(B, unreachable: Cactus's make fails on such a path first) the doctor's
look for the stamp step in a build script takes the stamp path raw,
where the script has it through `sh_quote`, so a `'` in the path would
read as an older cactup's script. `feature/build-cache` merges to master
next (decision 27).

### M5 part 2 meets master: round 33

Pushing master was refused: master had moved (PRs 2 to 5: the db1 and
rostam MDB entries, test selection, untracked thorn edits). Merged into
`feature/build-cache` (`4203c0b`, no conflicts); the MDB check then
failed on rostam's `arm-conda` universe, whose build setup makes a conda
environment on its spark node (`mkdir -p /work/$USER/spark-deps || exit
1`, micromamba fetched), so it stops early anywhere else. Neither side
had met the other: the check is on this branch only, rostam on master
only. It also showed a fault in the doctor: it ran the setup in an
environment of nothing (`USER` unset), so on rostam it would have aimed at
`/work//spark-deps`, said the setup stopped, or made an environment
there. Max chose to fix the doctor with a short review round before the
push. The fix: `fragile_entries` runs the setup as the build does (the
Cactus tree, the user's environment but for the path variables) for
the doctor; the MDB check keeps an environment of nothing but them. A
setup that stops before its end is checked as far as it ran (a trap on
the shell's exit writes the values to a file of the doctor's, past any
redirection or `cd` of the setup's), and the stop said (the doctor: "not
checked" for after it; the MDB check: named in its output, not failed);
one that ends the shell its own way (its own exit trap, `exec`) is an
error, as before. The real configurations `m5a`, `m5b` and `et` read the
same as before it, byte for byte.

Round 33 (on `efa55c3`), A: BLOCKED. The doctor ran the setup in
`bash -c` with the values file as an argument, not as the build does: a
relative entry of the user's own `PATH` was laid to the setup "with PATH
unset"; `BASH_ENV` was read (the build's `/bin/sh` does not); the setup
saw an argument (conda's `activate` passes its own on, and failed). Also:
only the first way a setup stopped was said; the spec claimed more than
the code. Fixed in the commit after this record: run as the build script
does (`/bin/sh`, `set -e`, no arguments; the values file named in the
environment, taken out before the setup runs); an entry of `PATH` the
user's own has is not the setup's, and `PATH` is said "as it is in your
environment"; every way it stopped is said. A's plants (67) and the real
configurations read as before but for the findings above.

B: BLOCKED, the same two (`BASH_ENV`, `SHELLOPTS`; the argument), and:
a setup with a syntax error was taken for one that stopped (the trap
ran before any of it), so the MDB check passed it; a setup's own trap on
exit was dropped once it was through (`trap - EXIT`), leaving its
temporary files; the MDB check's naming of a stopped setup never showed
in CI (a passing test's output is not shown). Fixed in the same commit:
the shell reads the setup with `-n` first, and one it cannot read is an
error; the doctor's trap writes only while a flag says the setup is not
through, so a trap of the setup's own is left to run; a setup that stops
in the MDB check must be in the test's `STOPS_HERE` with why (rostam's
`arm-conda`), and one there that no longer stops fails too. Checked by
re-running both reviewers' reproductions, a syntax error put into mike's
setup (the test fails, naming it), and both reviewers' plants.

Round 34 (on `3d87ace`): BLOCKED by both. B: the setup check
`exec`ed the checks, which drops a trap the setup set on its exit (the
build runs it): a setup's lock removed by its trap was left, so the
path-variable runs that follow said it stopped, and every build after
failed on the lock (older than this diff, made false by it). A: the
doctor's run stubbed `module`, where the build (bash as `/bin/sh`, as on
RHEL) runs the user's exported one: a `module load` that sets
`CUDA_HOME` gave a false entry. Both: the spec still said "run in bash,
module commands doing nothing, a missing sourced file passed over" of
the doctor's run. Fixed in the commit after this record: the checks run
as a child of the setup's shell, which then ends with their status,
anything after (the setup's trap) on stderr; the doctor's run has no
stubs (the MDB check keeps them: no module system there); spec, the
function's comment and the user docs (the setup runs, a few times, with
whatever it does) say so. Not blocking, for after the merge: the MDB
check runs rostam's provisioning on a host where `/work` can be written
(`/work//spark-deps`, network); a setup that closes stderr is said not
to run through (the doctor's own line fails under `set -e`).

Round 35 (on `b6a974b`): PASSED, both signed off. A: round 34's two
findings fixed (an exported `module` under bash run as `sh`: the
doctor's values equal the build's in every state; a setup's lock removed
by its own trap); 13 new setups compared with the build under dash and
bash as `sh`; every earlier plant, the real configurations and Ctrl-C as
before. B: likewise (the lock, the own trap, the rostam-like setup
provisioning once, where the build would; new probes; the merge is
exactly `git merge-tree 2e1fa14 ba0b0ec`). For after the merge, none
blocking:
- The setup is read as one block, the build reads it line by line: an
  alias it defines and then uses is "not found" in the doctor (older
  than round 33; no MDB setup uses one).
- The spec's paragraph on the MDB's setups should say the check still
  makes `module` and `ml` do nothing.
- `cache doctor --help` should say the doctor runs the machine's setup
  (a few times, with whatever it does), as the user docs now do.
- On a host where `/bin/sh` is bash, the unset and empty runs keep the
  user's module bookkeeping while the path variables are emptied: a
  `module load` of a loaded module does nothing there (can only miss).
- The MDB check can run rostam's provisioning (`/work//spark-deps`,
  network) where `/work` can be written, and real Lmod where
  `/usr/share/lmod` is (`x86-modules` sources it).
- The doctor runs the attempt's frozen setup: an older configuration's
  frozen rostam `arm-conda` package list would re-install itself into the
  shared environment.
- A setup that closes stderr is said not to run through.
- `guard_no_live_attempt` and pid reuse (round 31).
- The doctor's look for the stamp step takes the stamp path raw (round
  32).

### After the merge: round 1

On 2026-10-16, master at `2b7bfbd` (CI passed, published and deployed),
Max: "Fix 1, 3-4" of round 35's list, shelve the every-flag search, make
the case for or against an MDB extension for item 2 (a setup that
provisions on its own machine, rostam's `arm-conda`), then M4b, M5b, M6;
and the cache on by default (decision 34). Three commits on `2b7bfbd`:

- `0784992`: the pid reuse of round 31. A foreground build attempt's
  job id, and an interactive test run's, is the pid of the cactup that
  ran it; only a submitted attempt with no recorded outcome, or a
  submitted test run not finished, is asked of the scheduler now, and
  `running.lock` answers for the rest (`build list`/`show`/`prune`, the
  guard before a build, `test list`/`show`/`stop`/`delete`, the guard
  before a test run). An interactive test run's pid is stopped only while
  its lock is held. Sims are left as they are: on the generic machine a
  sim's job is a pid by design (said to Max, not decided).
- `669dcbc`: the setup is read as one block (round 35's first item) and a
  setup closing stderr is said not to run through. The doctor writes its
  script as the build script is written, the setup at its top level, and
  runs it from a file (dash and bash in POSIX mode both run an alias the
  setup defines in the lines after; both say "not found" inside a block,
  tried). The request, the answer and the mark of the setup's end are
  files in the doctor's work directory. `m5a`, `m5b`, `et`: the doctor's
  output is byte for byte master's.
- `e2a95f3`: decision 34. Unit tests' builds keep the cache out whatever
  the real global DB says (they read it: nothing points `CACTUP_HOME`
  elsewhere for them).

Round 1 (on `0430bac`): BLOCKED by both. A: (1) a submitted test run
on the generic machine has a pid for its job too, and `test stop` set
no `finished`, so a stopped run's reused pid read as RUNNING and `test
delete -f` ran `pkill -g` on it (ran); (2) the contract with the
build-speed work still said the cache is off by default, and their
timing builds would serve from it. B: (1) `test stop`/`delete` handed a
running interactive run's pid to the scheduler: on Slurm `squeue` knows
no pid, so `stop` said "not in the queue" and `delete` trashed the live
run (ran; master the same); (2) across hosts the lock's start time is not
checked, so another host's pid went to the local `pkill -g` (ran); (3)
the contract, as A's (2). Non-blocking from B: the doctor took a setup's
own failing exit trap (`trap false EXIT`) for failed checks; text still
saying to turn the cache on (this file's rules, the help's `knob`
example, the spec's D15 row); a submitted test run on the generic
machine that ended without recording it still has its pid asked (as the
generic machine's sims: put to Max); a native blocking submit's late job
id (as on master). Fixed in `d9c8da1` and the commit after this record:
`test stop` records `finished`; an interactive run's pid never goes to
the scheduler (`stop` says to interrupt it where it runs, as `build stop`
does; `delete` refuses while its lock is held, `-f` too); the live
contract and its snapshot say the default changes and that timing builds
pass `-K build-cache=off`; the doctor takes a whole reply whatever the
shell's status after it; the texts.

Round 2 (on `468e97a`): BLOCKED by both. A: (1) `test delete` (no
`-f`) trashed a live submitted run that another host runs on a
`ps`-style scheduler (its pid unknown here, its lock held; ran); (2) on
such a scheduler another host's pid still reaches this host's `ps` and
`pkill -g` (ran). B: (1) `test stop` recorded `finished` though no stop
was sent: a failed `squeue` reads as "not in the queue", and the live
job was then taken for ended for good (ran, new in `d9c8da1`). Not
blocking: a compute-side `test run` read its record before taking the
lock and could write it back without the job id (A); `delete` took no
lock (B); the contract's record of decision 28 and of master, and what a
build with the cache on gets (A, B); the user docs on `test stop` (A);
the spec named only the generic machine where ten have a `ps`-style
scheduler (B). Fixed in the commit after this record: `stop` records
`finished` only once it sent the stop; a submitted run whose lock is
held but whose job the scheduler here does not have is said to run
where it cannot see, and is neither stopped nor deleted (`-f` neither);
`delete` holds the test lock while it looks; `run_compute` reads the
record again under the lock; the contract, the docs, the spec. A's (2)
(another host's pid on a `ps`-style scheduler) is the generic machine's
pid-as-job design, put to Max with its sims; the spec says it.

Round 3 (on `f146bd7`): BLOCKED by both, the same finding: on a batch
scheduler a run whose lock is held though the scheduler does not list
its job was said to run "where this one cannot see it (another host?)";
Slurm sees every node, so it is a query that failed or a run that ended
without letting go of its lock (ran, both, with a fake `squeue` that
answers as Slurm does). Fixed in the commit after this record: `stop` and
`delete` say only that (another host, a query that failed, or a lock
that expires within 15 minutes) and leave the run; the spec likewise.
Not blocking: a failed `squeue -j` reads as "not in the queue" (Slurm
exits 1 for both), so `delete` trashes a queued run on a failed query
and `stop` says it is not in the queue (as on master and for sims: put to
Max with the `ps`-style schedulers); "stopped job" was logged when no
stop was sent (fixed); `delete`'s checks had no test (now
`clear_to_delete`, tested); the comment on `delete`'s lock overstated
what it covers (a foreground run's start, a window of milliseconds, is
not; fixed); the spec now says a new run's `-f` also stops a `ps`-style
job; the docs on `--force`.

Round 4 (on `7bbda47`): BLOCKED by both. Both: a job the scheduler
lists in a state no pattern matches (`JobStatus::Error`: Torque's exiting
`E`, Slurm's suspended `S`) was said not to be listed (ran, with each
machine's own patterns and a fake `qstat`/`squeue` printing as the real
ones do). A: the sibling message `without_job` still said "another
host?" of a stopped Slurm job whose lock lingers, and "is running in the
foreground" of a foreground run on another host killed without letting
go of its lock (ran). B, not blocking: the spec said every `ps`-style
`stop` stops a process group (five of them `kill` one process). Fixed in
the commit after this record: a listed job, in whatever state, is the
scheduler's (`stop` stops it; `delete` refuses it without `-f`, stops it
with); `without_job` names the lock that outlives its run, and drops
"another host?"; the spec.

Round 5 (on `6601c23`): BLOCKED by both, the same finding: round 4
took every job listed in an unread state (`JobStatus::Error`) for live,
but Slurm and Torque list an ended job for a while (`CD`, `F`, `CA`,
Torque's `C`): `delete` said a completed job "has a live job", and `stop`
and `delete -f` failed on Torque's `qdel` refusing it (ran, with fake
`squeue`/`qstat`/`qdel` answering as Torque 6 and Slurm do). Fixed in the
commit after this record: such a job is its own case (suspended,
exiting or ended); `stop` asks the scheduler to stop it and records the
end only if it took the stop, saying one it refuses may have ended;
`delete` refuses it without `-f`, and with `-f` stops it or, where the
scheduler refuses, goes ahead unless the run's lock is held; a failed
stop command is named. Not blocking: the guard before a new run counts
only R, Q and H as live (as master and sims).

Round 6 (on `9377920`): BLOCKED by both. A: Slurm's `scancel` takes an
ended job without a word (exit 0), so `stop` said "it took the stop" of a
job that had ended (ran with a fake `scancel` as Slurm's source has it).
B: on Torque a completed job (`C`), which `qdel` refuses, kept `delete`
refusing, its advice (`test stop` first) never unblocking it (ran; master
trashed it). Not blocking: the refusal of `delete -f` with the lock held
did not say the lock expires; the docs on `--force`; the lines' first
letter; a suspended job whose lock lapsed and whose stop fails at that
moment is taken as ended (needs a coincidence). Fixed in the commit after
this record: `stop` says only that it asked the scheduler; where the
scheduler refuses and nothing of the run holds its lock, `stop` takes the
run as ended and records it, so `delete` goes ahead; the texts.

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

(Narrowed to gfortran by decision 16: what follows on C and C++ is the
history of the branch `feature/build-cache-m3a-cxx`.)

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
  ask `__has_include_next`. (Comments were then found by reading the file
  as the compilers do; round 3 gave that up.) `__has_include` is answered for every
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

M5 part 2 passed its gate at `92c3089` (round 32), and with it M5;
master had moved, and the merge passed rounds 33 to 35 (at `b6a974b`);
master is `2b7bfbd` (decision 27), released. Now "After the merge: round
1" (above): items 1, 3 and 4 of round 35's list and decision 34, in
review; item 2 (the MDB check running a setup's provisioning) waits on
Max's choice between the proposals put to him. Then M4b (Cray), M5b (the
whole-file cases), M6 (HIP). The every-flag search on
`feature/doctor-every-flag` is shelved (Max, 2026-10-16).

M5 (`cactup cache doctor`) in two parts, each through the review gate.
Part 1 passed its gate at `ad236d2` (spec §18.12):

- Every serving build ends with one line from `cactup __cc-summary`:
  `350 of 389 objects reused; 39 compiled: 22 new, 9 with other sources,
  5 built otherwise, 3 kept out (see cactup cache doctor)`, a kind with a
  count of 0 left out. "Built otherwise" (another compiler, other flags or
  environment) is a fourth kind beside Max's three, so that those misses
  are not called "sources changed"; "failed" counts failed compiles.
- The unit records: `<store>/v4/<machine>/units/<ab>/<unit digest>.<key>`,
  one per published entry, zlib-compressed JSON (the key's label and parts,
  each file's mapped name, 16 hex digits of its digest, whether it was
  narrowed), written as entries are. A miss reads its object's newest 32
  and logs its kind and what differs from the closest (files, with how the
  key took each: decision 27; or the key's parts). `cache gc` removes the
  records of entries that are gone; `cache stats` counts them.
- `cactup cache doctor [<config>] [--attempt N]`: the line, the compiles
  kept out by reason (with what would let them in, where known), the misses
  with other sources by file, those built otherwise by part, and the
  compiles of which no file was narrowed, by reason. Reads the event log
  only.

Found by trying it on plato (`~/tmp/build-cache-m5/run.sh`: smoke.th served
as `m5a`, then two thorns fewer as `m5b`, then `m5a` again):
- Round 8's rule (no narrowing for a C unit with a `#pragma GCC` its front
  end acts on) kept every C unit whole on Debian 13: glibc's
  `bits/stdlib-bsearch.h`, read by every unit with `<stdlib.h>`, has a
  `#pragma GCC diagnostic push`/`ignored`/`pop`. `m5b` was served 236 of
  342. A `diagnostic` region that opens and closes in one file, with no
  other file entered in between, reaches no file the key may narrow (the
  file it is in has code and is keyed whole): such a region no longer
  counts (`key::FrontEnd`). `m5b` now 336 of 342; the 6 left are the
  bindings sources, which list the thorns. (Round 1 then excused only an
  `ignored` region: key label `key-21`.)
- From M4 (decision 30): an entry whose messages speak of definitions is
  stored only under the key with its files whole, and a lookup went there
  only after finding another entry under the plain key: such a unit was
  never served, not even to the build that stored it (`OverloadComm.c`).
  A narrowed key now looks there when the plain key has nothing.

Part 2 is in review (spec §18.12, "The build environment", "The MDB's
setups"; `src/objcache/doctor.rs`): `cache doctor` first runs the
attempt's frozen environment setup in a shell, which starts `cactup
__cc-doctor` with the compilers and flags `make` gives the configuration
(through `config-data/make.config.defn`); that process applies the
wrapper's own functions in the build's environment (the refused
variables, the driver's own searches, the compiler's identity, the
compile's words read whole and the reader's first reason (decision 32),
a whole key of a probe compile per language where there is none) and answers in JSON; `cache doctor` traces each finding to the
user's shell, the machine's setup, or the variable of `make.config.defn`
make says gives the flag (decision 33), with what to change. Named sets of checks (`doctor::CheckSet`). A unit
test runs every MDB machine's build setup in every universe with each
path variable unset, empty and set: all clean today (they use the
`${X:+…}` idiom). On plato's `m5a`: nothing keeps the cache out;
`CCC_OVERRIDE_OPTIONS`, a relative `COMPILER_PATH` and an empty
`LIBRARY_PATH` entry planted in the shell are each found and traced to
it. Then the merge of
`feature/build-cache` to master (decision 27), and the follow-ups M4b Cray,
M5b, M6 HIP. Max's warnings replayed by condition (decision 31) is on hold.

qbd test instance: `/work/mmorris/cactup-m4/` (its own `CACTUP_HOME`,
installation `m4` at `/ddnB/work/mmorris/m4`, builds to `single` by the
MDB's default; `pair.sh` serve-audit-serve, `bench*.sh` the two-thornlist
benchmark (three thorns apart each way; `bench8.sh` on `b652348`),
`night.sh` the GPU test suites; at most one ssh session to qbd at a time,
its login limit holds a refused login for an hour; `ssh` from plato needs
the sandbox off, which cannot resolve qbd's names). The benchmark's history:
whole files 184 of 389 served, 8:47 against 8:28 cache off; decision 29 336
(8:16); decision 30 346 (3:05); from `0164287` on, 350 of 389 (80 of 96
`nvcc`), 1:07 to 1:39 against 8:28 to 9:05, audits 0 wrong; the served
build's GPU test suite on gpu2 passed 67 of 67.
