# Interface contract: cactup build cache <-> Cactus build-speed work

Two workstreams change how Cactus gets built, and Max wants them to stay
mutually compatible:

- **Cache side**: a shared compiler-output cache built into cactup.
- **Speed side**: speed improvements to the Cactus build system.

This file is the single source of truth for what each side depends on and
what each side is changing. It is deliberately independent of any Claude
session, account, or memory: sessions and accounts rotate, so neither side
may assume the other remembers anything that is not written here.

## How to use this file

1. Read the whole file before changing anything listed in it.
2. Parties are named by role ("cache side", "speed side"), never by session
   name. Whoever currently holds a role edits that role's sections.
3. Edit only your own side's sections. To object to, or ask about, the other
   side's entry, add a dated item under "Open questions"; do not rewrite it.
4. Before changing anything the other side lists as a dependency, add an
   entry to your "Planned changes" section first, with enough detail for
   the other side to adapt (the new recipe text, the new file layout).
   Move it to "Landed changes" when it lands, with the commit.
5. Every edit also gets one line in the change log at the bottom:
   `YYYY-MM-DD  <role>  <what changed>`.
6. Each side re-reads this file at every milestone of its own work, and
   whenever a new session picks the work up.
7. Neither side writes in the other's working tree:
   cache side works in `~/cacti/build-cache` (and a sibling install it
   creates for cross-installation tests); speed side works in
   `~/cacti/speedup-build`. To test against the other side's changes, check
   its branch out in your own tree.

## Where each side's durable state lives

- **Cache side**: cactup repo (`/home/max/src/Cactup`), branch
  `feature/build-cache`, worktree `.claude/worktrees/build-cache`. Plan:
  `design/build-cache/PLAN.md`; current status and next step:
  `design/build-cache/STATUS.md` (both on that branch).
- **Speed side**: (speed side: please fill in the repo, branch, worktree,
  and any plan or status file.)

## What the cache side is building

cactup interposes a wrapper on each Cactus object compile. Eventually the
wrapper computes a key from the compiler's own preprocessor output, the
compiler's identity, its arguments, the platform, and selected environment
variables; on a hit it restores the object instead of compiling, on a miss
it runs the compiler unchanged and stores the result. What exists today
(milestone M0a) only runs the compiler unchanged and logs one line per
compile.

The wrapper is fail-open by design. If the make system does not look the
way it expects, a probe step turns the cache off for that build and prints
one line saying why. So a speed-side change can at worst disable caching;
it cannot break a build. Keeping the two compatible means keeping the cache
*working*, not merely harmless.

How it gets in front of the compiler: a makefile fragment, read through the
`MAKEFILES` environment variable for the `make <config>` step only, that
redefines Cactus's compile recipes inside Cactus's object sub-makes:

```make
override define COMPILE_C
<the body copied from this configuration's make.config.rules, with $(CC)
 replaced by: '<cactup>' __cc '<conf>' '$(CC)' '$(SHELL)'>
endef
```

No compiler variable is touched and no recipe's environment changes.

## Cache-side dependencies on the Cactus make system

Baseline: flesh `lib/make` at commit `ae66cd2` (Cactus 4.20.0). "M0a" marks
what the code on `feature/build-cache` relies on today; "later" marks what
the planned keying and serving will rely on.

- **A1 (M0a). The compile recipes are canned sequences** named `COMPILE_C`,
  `COMPILE_CXX`, `COMPILE_CU`, `COMPILE_F77`, `COMPILE_F`, `COMPILE_F90`,
  each defined in `configs/<name>/config-data/make.config.rules` exactly
  once, unconditionally (not inside `ifeq`/`ifdef`), as a line `define NAME`
  with nothing after the name (no `=`, no comment), then `endef`, with
  nothing else assigning to it and no nested `define`. The rules file must
  not `include` other makefiles (the probe then declines for the whole
  build: it cannot see what they define). Each
  body contains exactly one reference to its compiler variable, spelled
  `$(CC)`, `$(CXX)`, `$(CUCC)`, `$(F77)`, `$(F90)`, `$(F90)` respectively,
  **in command position** (at the start of a recipe line that does not
  continue the one above, or right after `;`, `&&` or `||`, outside quotes
  and backticks) and as a word of its own. The probe copies the body
  and replaces that one reference. A recipe that does not fit is left
  unwrapped (that language is then not cached): so `cd x ; $(CC) ...`
  works, `cd x ; $(LAUNCHER) $(CC) ...` builds fine but uncached.
- **A2 (M0a). Object compiles run in a sub-make that** (a) is given
  `CCTK_TARGET=...` on its command line (`make.thornlib` does this for
  `make.subdir`), (b) runs with its working directory under
  `configs/<name>/build/`, (c) is given `SRCDIR=<the thorn's source
  directory>`, and (d) is started by a make that itself read `MAKEFILES`
  (so the variable is still in its environment). The fragment acts only
  when (a) and (b) hold, and reads `$(SRCDIR)/make.code.defn` and
  `$(SRCDIR)/make.code.deps` to stand down for a thorn that mentions
  `COMPILE_` in them.
- **A3 (M0a). The object rules' recipes use those canned sequences**
  (`$(COMPILE_C)` and so on), `make.config.rules` is included after the
  fragment is read, and the only file read after `make.config.rules` that
  may redefine a `COMPILE_*` sequence is the thorn's own `make.code.deps`
  (directly: a redefinition in a file it includes, or under a computed
  name, is not noticed and loses to the fragment's `override`).
- **A4 (M0a). `MAKEFILES` is honored** by every make in the chain from
  `make <config>` down to the object sub-makes, and none of Cactus's
  makefiles reads `MAKEFILE_LIST` (the fragment removes itself from it).
- **A5 (M0a). GNU make.** Checked on 4.2.1, 4.3 and 4.4.1 (real Cactus
  trees on 4.3 and 4.4.1); a self-test in the build script decides for
  anything else. `/proc/self/status` must be readable where compiles run.
- **A6 (later). Compile recipe shape.** One source file per compiler
  invocation, with `-c` and `-o <absolute object path>`, run from cwd
  `$(TOP)/scratch`, on the processed copy in `$(TOP)/build/<Thorn>/...`.
- **A7 (later). Dependency files** come from a separate `$(CC) -E -M ...`
  run (`C_DEPEND` and friends), not from the compile itself.
- **A8 (later). Fortran module files** all land flat in `$(TOP)/scratch`,
  the compile cwd, and a thorn that produces a module is built before a
  thorn that uses it (today through `USESTHORNS`).

No longer a dependency (the first design needed them, review replaced it):
object target names, `make.config.defn`, exported compiler variables.

Changes the cache side can adapt to if told in advance (please add a
"Planned changes" entry rather than avoiding them):

- Merging dependency generation into the compile (`-MD`/`-MMD -MF ...`).
  The copied recipe carries it along automatically; the cache would later
  store the depfile as an extra output and rewrite its paths on restore.
- Precompiled headers, unity or batched compiles.
- A different compile cwd or build-directory layout; dropping the processed
  source copy.
- Replacing recursive make (non-recursive make, ninja, anything else), or
  moving object compiles out of `make.subdir`.
- Renaming or restructuring the `COMPILE_*` sequences, or compiling without
  them.
- Changing how `C_LINE_DIRECTIVES` / `F_LINE_DIRECTIVES` work.
- New default compiler flags (`-pipe`, `-ffile-prefix-map`, LTO).
- Wrapping compilers yourself (ccache, sccache, distcc, or similar): cactup
  leaves a compiler that already runs through one of these alone.
- Changing the order in which thorns build (relevant to A8 once Fortran is
  cached; the 2026-10-01 `build-speedup` commit `996c71f` does this, and
  the M0a code is unaffected by it).

## What the cache side adds that the speed side should know about

With the knob `build-cache` off (the default), nothing at all: the build
script is byte for byte what it is without the cache.

With `build-cache = record` (M0a, on `feature/build-cache` only):

- **C1.** One probe run and two tiny self-test `make` runs per build, after
  `make <config>-config` (and after `-clean`). The probe creates
  `configs/<name>/build/` if a `realclean` removed it.
- **C2.** `make <config>` runs in a subshell with `MAKEFILES` naming
  `<attempt>/cc/inject.mk` (after any entries the user already had). Every
  make below it reads that file. In every one of them it defines an empty
  rule for itself and reassigns `MAKEFILE_LIST` to drop its own name. In
  Cactus's object sub-makes it also defines the `COMPILE_*` overrides and
  one helper variable (`cactup_cc_run`), removes itself from the
  `MAKEFILES` handed on to child processes, and runs one `cat` of the
  thorn's two make fragments while parsing.
- **C3.** Each object compile runs as a child of a short-lived cactup
  process (on the order of a millisecond on top of the compiler; to be
  measured properly in M0c) which appends one line to
  `<attempt>/cc/events.jsonl`. Compilers, flags and objects are unchanged.
  **The recipe text changes**: with `SILENT=no`, or `make -n`, the compile
  line reads `'<cactup>' __cc '<conf>' 'gcc' '/bin/bash' <flags...>` where
  it read `gcc <flags...>`. Anything that parses make's echoed commands
  sees that.
- **C4.** One line in the build output after the compile step:
  `cactup: build cache: compiles recorded: N`. Files in the attempt
  directory: `cc/config.toml`, `cc/inject.mk`, `cc/selftest/`,
  `cc/selftest.log`, `cc/events.jsonl`.
- **C5.** Edits in the cactup repo: `Cargo.toml` (`rustix` as a direct
  dependency), `src/main.rs` (wrapper dispatch at the top of `main`),
  `src/build/mod.rs` (`prepare`: two build-script steps, through one call
  into the new module), `src/build/attempt.rs` (one path helper),
  `src/database.rs` and `src/args.rs` (the knob and its help), the new
  `src/objcache/` and `tests/objcache.rs`, and one step in
  `.github/workflows/ci.yml`.

Planned, not there yet: an extra preprocessor run (`-E`) per cached unit
(one on a hit, two on a miss), which speed measurements taken with the cache
on will include; `-ffile-prefix-map=...` flags added by the wrapper for GCC
and Clang family compilers; a `cactup cache` command; one call in `execute`
after make.

## Cache-side planned changes

- 2026-10-01: M0a is in review. Then M0b (keys, richer event log, a report
  comparing two builds), then M0c (measurements). Nothing is served until
  the measurements are reviewed.

## Cache-side landed changes

- 2026-10-01, on `feature/build-cache` (not on master): M0a as described
  under C1-C5. Checked in `~/cacti/build-cache` with GNU make 4.4.1 and GCC
  14.2: a 25-thorn configuration (357 objects, all byte-identical with and
  without the wrapper, `config-data` unchanged); a configuration that
  builds HDF5 from source (384 Cactus objects logged, nothing of cactup in
  HDF5's build or its installed `h5cc`); and the first configuration again
  on the flesh at `build-speedup` `996c71f` (357 objects, identical).
  Spec: §18 of `design/cactup-simfactory-design-new.md` on that branch.

## Speed-side planned changes

(speed side: please list here anything touching A1-A8, the compile recipes,
the build-script steps cactup runs, the MDB `[build]` keys or optionlists,
or the cactup files in C5.)

## Speed-side landed changes

(none recorded)

## Ownership of shared cactup files

Until the speed side says otherwise, the cache side assumes it is the only
one editing `src/build/mod.rs` `prepare`/`execute` around the make steps and
`src/build/attempt.rs` `BuildMeta`. If the speed side also needs to edit
them, say so here and we split by function, and rebase on master before
every milestone.

## Open questions

- 2026-10-01, cache side -> speed side:
  a. Where does your work live (repo, branch, worktree; a flesh fork, or
     cactup-side changes, or both)?
  b. Which of the changes listed above are you planning or have already
     made? What does the compile recipe look like in your version?
  c. Do you change cactup's `prepare`/`execute`, or the MDB
     (`[build].make`, optionlists)? Is an `mdb/GENERATION` bump planned?
  d. Does anything in C1-C5, or in the planned additions below them, break
     or distort your measurements?

## Change log

- 2026-10-01  cache side  Created the file: dependencies A1-A8, additions
  C1-C4, planned milestones, questions a-d.
- 2026-10-01  cache side  Added A9 (exported compiler variables, the
  `VAR:n` marker); recorded M0a as landed on the feature branch, in review.
- 2026-10-01  cache side  Replaced the injection after review (it now
  redefines the `COMPILE_*` recipes instead of setting compiler variables
  per target): rewrote the A list (A1-A5 current, A6-A8 for later) and the
  C list (C1-C5 current). Noted that `build-speedup` `996c71f` was tested.
- 2026-10-01  cache side  After review round 2: A1 now requires the
  compiler reference in command position and an unconditional single
  definition; A2 (d), A3's limit and A5's `/proc` added; C2-C5 completed
  (the echoed recipe text, `MAKEFILE_LIST`, the count line, `Cargo.toml`,
  CI). The wrapper now takes the compiler text as an argument.
- 2026-10-01  cache side  After review round 3: A1 also requires that the
  rules file includes no other makefile and that `define NAME` has nothing
  after the name; command position excludes quoted text and continuation
  lines.
