# Build cache: status

Read this first when picking the work up. The plan is `PLAN.md` in this
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
- Every milestone ends at a review gate: two independent harsh reviewer
  agents, same brief, full milestone diff. Fix or answer every blocking
  finding and re-review until both sign off in the same round. Record the
  verdicts below.
- Re-read the contract file at every milestone, keep the cache-side
  sections current, and refresh the snapshot.
- Test the make-facing parts against more than one GNU make:
  `CACTUP_TEST_MAKES=/path/to/make-4.2.1:/path/to/make-4.3 cargo test
  --test objcache` (build them from ftp.gnu.org; 3.82 does not run on a
  current glibc).
- `CLAUDE.md` is untracked and shared by every session in the repository:
  do not edit it from this branch. Its text waits in `CLAUDE-contract.md`.
- Commits carry no AI attribution. Never run `cargo fmt`.

## Milestones

| Milestone | Scope | State |
|---|---|---|
| M0a | Wrapper dispatch, fail-open paths, panic hook, probe and `inject.mk`, per-build config, knob | **passed the gate** at `300fd0b` (four review rounds) |
| M0b | Argument parser, platform/identity/environment digests, key, richer `events.jsonl`, `cache report` | implemented; in review (round 1) |
| M0c | Measurements in `~/cacti/build-cache`, written results | not started |
| M1a | Store: publish, restore, invalidate; the `build-cache-dir` knob | not started |
| M1b | Serving, double check, audit mode, two-installation audit build | not started |
| M1c | `cache stats/gc/verify`, size notice, contract into `CLAUDE.md` | not started |

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
- `src/objcache/identity.rs`: which compiler, by content (driver, GCC's
  back ends and assembler, the shared libraries they load), remembered per
  attempt.
- `src/objcache/platform.rs`: machine, universe, environment-setup digest
  (frozen by `prepare`), and the compiling host's architecture, processor
  kinds and OS release.
- `src/objcache/environment.rs`: the allowlisted environment.
- `src/objcache/key.rs`: the path map, the preprocessor run, the five-part
  key.
- `src/objcache/event.rs`: the event log's format.
- `src/commands/cache.rs`, `CacheCommand` in `src/args.rs`: `cactup cache
  report`.
- The points carried over from M0a's round 4 (all but the `SIGPIPE` one,
  which is documented in spec §18.4 as a limit): the compiler's `_`, the
  narrower `~` rule with a reason on record, the self-test's third run and
  its `&&` chains, a compiler on a continuation line, the `BASH_ENV`
  wording, the self-test test over `CACTUP_TEST_MAKES`, more shell words.

First numbers (2026-10-01, `plato`, GCC 14.2, make 4.4.1, an *unoptimized*
cactup, `-j 8`; M0c is where these get measured properly):

- `smoke` (25 thorns): 357 compiles, 307 keyed (all C and C++); the 50
  Fortran compiles are 2% of the compile time. All 357 objects are byte for
  byte those of a build without the cache.
- `smoke2`, the same sources under another configuration name, against
  `smoke`: **307 of 307 keyed compiles would be served** (98% of the
  compile time).
- `ext`, which adds HDF5 and two thorns that use it and builds from another
  optionlist file, against `smoke`: 301 of 334 (90%); of the 33 misses, 27
  are files `smoke` does not have and 6 differ in their preprocessed text.
- Cost: keying summed to about 30% of the compile time and checking again
  to about 25%; wall clock went from about 16 s to about 39 s for `smoke`.
  The debug build of cactup is a large part of that (it reads 170 MB of
  preprocessed text line by line), and the second preprocessor run is the
  obvious thing to make cheaper. A release build has not been measured.

"Would be served" rests on the path mapping of spec §18.5 producing the
same object, which only audit mode (M1b) can show per compiler.

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
  changed (Modeling, aging, afterward). `OPTIMISE` there is Cactus's own
  option name and stays.

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

### M0b, round 1: pending

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

## Decisions waiting for Max

Neither blocks M0b; both are about what the cache may do in a setup nobody
is known to have. They are the two known ways it could change what gets
compiled.

1. **A thorn's compile recipe defined indirectly** (in a file its
   `make.code.deps` includes, or under a computed name) is not seen by the
   stand-down scan, and Cactus's stock recipe would run in its place. No
   Einstein Toolkit thorn defines a compile recipe. Accept as a stated
   limit, or have the probe do more (for example, decline for the whole
   build if any thorn make fragment has an `include`)?
2. **Functions or aliases a shell defines for itself at startup**
   (`BASH_ENV`) are invisible to the wrapper; one named like the compiler
   would be bypassed. Closing it means sending every compile to the shell
   wherever `BASH_ENV` is set, which module systems do, so the cache would
   be off on most clusters. Accept as a stated limit?

## Next step

Take M0b through its review gate. Then M0c: a release build of cactup; a
second installation (`cactup install`, alias `build-cache-b`, same release)
for cross-installation numbers; the full Einstein Toolkit thornlist; line
directives on and off; edit-and-revert of a thorn; a fresh login session;
wall-clock overhead against an unwrapped build. Write the results up for
Max before any M1 work: nothing is stored or served until he has seen them.
