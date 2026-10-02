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
| M1a | Store: publish, restore, invalidate; the `build-cache-dir` knob; thorn stand-down on `include`/`define`/`eval`; the shell asked what the compiler's name resolves to | **go-ahead given**; not started (`HANDOFF-M1.md`) |
| M1b | Serving, the locale trial, dependency file on a hit, audit mode, two-installation audit build | not started |
| M1c | `cache stats/gc/verify`, size notice, contract into `CLAUDE.md` | not started |
| after M1 | gfortran, then the CUDA compilers; the narrower key revisited with audit mode | not started |

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

## Decisions

All of them, answered, are in `DECISIONS.md`. Decision 3's check came
back on 2026-10-02 from qbd: a site-built GCC whose specs file adds an
rpath in a link section and changes nothing else, so the refinement for
specs files is needed and is taken into M1a. A Spack-built GCC is still
unchecked.

## Next step

M1a, as `HANDOFF-M1.md` lays it out.
