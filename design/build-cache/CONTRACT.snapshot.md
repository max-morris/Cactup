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
  **in command position** (at the start of a recipe line, or right after
  `;`, `&&` or `||` — also across a `\` line continuation — outside quotes
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
- **A6 (M0b). Compile recipe shape.** One source file per compiler
  invocation, with `-c` and `-o <object>`, and no flag that makes the
  compiler write a second file — with one exception, added for the speed
  side's `C_DEPEND_COMPILE_FLAGS` recipe (seen in `~/cacti/speedup-build`
  on 2026-10-01): **dependency output written by the compile is
  understood** in the form `-MD` or `-MMD`, optional `-MP`, **`-MF <file>`
  given explicitly**, optional `-MT`/`-MQ <target>`. Those flags are not
  part of the key (same object with and without them) and are kept from
  the cache's preprocessor runs. When the cache serves an object it will
  have the dependency file written by its own preprocessor run with the
  same flags, so what the cache needs from the recipe is: (a) `-MF` always
  explicit (without it the file's place depends on `-o`); (b) the fix-up
  of the file (`DEPEND_COMPILE_FIXER`) stays a recipe line of its own,
  after the compile, not part of the compiler command; (c) no `-MG`, no
  `-MJ`. A compile of any other shape still runs exactly as given, but
  gets no key and will never be served.
- **A10 (M0b). What goes into a key, as far as the make system decides it.**
  None of this can break a build or make the cache wrong; all of it decides
  how often the cache can serve.
  (a) The key covers the *bytes* of every file a compile reads: the
  processed source copy and every header. Generated headers that most
  sources include (`cctk_DefineThorn.h`, `CParameterStructNames.h`, the
  per-thorn `cctk_Arguments` and parameter headers) therefore decide the
  hit rate. They must be the same bytes for the same inputs: no
  timestamps, no absolute paths, no ordering that varies from run to run
  (today they are: two configurations of one thornlist share every key).
  (b) Absolute paths are tolerated in exactly these places: file names on
  the command line, `-I` directories, and the *first line* of a processed
  source copy when it is `#line <n> "<absolute path of the original>"`
  (what `C_LINE_DIRECTIVES = yes` writes today). An absolute path anywhere
  else in a file a compile reads ties the object to its installation.
  (c) Compiles run from `<config>/scratch`; the Cactus root and the
  configuration directory are the two prefixes the cache maps away.
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

- **C1.** One probe run and three tiny self-test `make` runs per build, after
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
  process which appends one line to `<attempt>/cc/events.jsonl`.
  Compilers, flags and objects are unchanged. **Since M0b a recording
  build is slower**: for every C and C++ compile, cactup also runs the
  compiler's preprocessor twice (`-E`, before and after the compile) and
  reads every file that run names, twice, to key it; once per build and
  compiler it also compiles two tiny trial files. On the 25-thorn test
  configuration that summed to about a third of the compile time again;
  proper numbers come with M0c. Do not take speed measurements with
  `build-cache = record` on and compare them with ones taken with it off.
  **The recipe text changes**: with `SILENT=no`, or `make -n`, the compile
  line reads `'<cactup>' __cc '<conf>' 'gcc' '/bin/bash' <flags...>` where
  it read `gcc <flags...>`. Anything that parses make's echoed commands
  sees that.
- **C4.** One line in the build output after the compile step:
  `cactup: build cache: compiles recorded: N`. Files in the attempt
  directory: `cc/config.toml`, `cc/inject.mk`, `cc/selftest/`,
  `cc/selftest.log`, `cc/events.jsonl`, `cc/compilers/`, `cc/hosts/`.
- **C5.** Edits in the cactup repo: `Cargo.toml` (`rustix` as a direct
  dependency), `src/main.rs` (wrapper dispatch at the top of `main`),
  `src/build/mod.rs` (`prepare`: two build-script steps, through one call
  into the new module), `src/build/attempt.rs` (one path helper),
  `src/database.rs` and `src/args.rs` (the knob and its help), the new
  `src/objcache/` and `tests/objcache.rs`, and one step in
  `.github/workflows/ci.yml`.

Planned, not there yet: serving (then one preprocessor run on a hit, two on
a miss, and no compile on a hit); `-ffile-prefix-map=...` flags added by the
wrapper to the real compile for GCC and Clang family compilers (today they
go to the extra preprocessor runs only); one call in `execute` after make.

## Cache-side planned changes

- 2026-10-02: M0a, M0b and the measurements (M0c) are done and reviewed
  on `feature/build-cache` (up to `129ecf7`). The cache still only
  records. **Max has given the go-ahead for M1**: the store (M1a), then
  serving (M1b), then the maintenance commands (M1c); after that gfortran,
  then CUDA. What serving will change for the speed side, once it lands:
  real compiles gain `-ffile-prefix-map=...` flags (GCC and Clang
  family); on a hit no compiler runs, and a dependency file the recipe
  asked for (A6) is written by the cache's preprocessor run instead;
  thorns whose `make.code.defn` or `make.code.deps` contain an `include`
  directive, a `define` or `$(eval` are left out of the cache. Timings
  taken with the cache serving are not comparable with ones taken
  without it.

## Cache-side landed changes

- 2026-10-01, on `feature/build-cache` (not on master): M0a as described
  under C1-C5. Checked in `~/cacti/build-cache` with GNU make 4.4.1 and GCC
  14.2: a 25-thorn configuration (357 objects, all byte-identical with and
  without the wrapper, `config-data` unchanged); a configuration that
  builds HDF5 from source (384 Cactus objects logged, nothing of cactup in
  HDF5's build or its installed `h5cc`); and the first configuration again
  on the flesh at `build-speedup` `996c71f` (357 objects, identical).
  Spec: §18 of `design/cactup-simfactory-design-new.md` on that branch.
- 2026-10-01, on `feature/build-cache`: M0b. A recording build keys every
  C and C++ compile it can (see A6, A10, C3) and `cactup cache report`
  compares two builds. Same three configurations: objects still
  byte-identical to a plain build; two configurations of one thornlist
  share every key; a configuration with three thorns added shares 55%
  (the generated headers of A10 (a) are why it is not more).

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
- 2026-10-01  cache side  M0a passed its review gate at `300fd0b`.
- 2026-10-01  cache side  M0b in review: C1 (three self-test runs) and C3
  (a recording build now runs the preprocessor twice more per C/C++
  compile) updated; A1's command position now allows a continuation line
  after a separator.
- 2026-10-01  cache side  M0b after its first review: the key now covers
  the bytes of every file a compile reads, not only the preprocessed text.
  A6 is a current dependency (and says what `-MD` in the compile recipe
  would do); A10 added (generated headers must be deterministic and free of
  absolute paths; where absolute paths are tolerated); C3 and C4 updated.
- 2026-10-01  cache side  M0b passed its review gate at `045eb76`; landed
  and planned sections updated. No change to A or C since the last entry.
- 2026-10-01  cache side  **Load on plato:** M0c runs full Einstein Toolkit
  builds (`-j 8`) in `~/cacti/build-cache` and `~/cacti/build-cache-b`
  from about 21:00 local for a few hours. Timings either side takes on
  this host meanwhile are not comparable with quiet-host timings.
- 2026-10-01  cache side  A6 rewritten: the speed side's
  `C_DEPEND_COMPILE_FLAGS = -MD -MP` recipe (`-MF ... -MT ...` in
  `COMPILE_C/CXX/CU`, fix-up in `POSTPROCESS_*`) is being taken up by the
  cache as it stands; (a)-(c) say what must stay true of it. Not yet
  through review on the cache side.
- 2026-10-01  cache side  Load on plato is over (M0c builds finished about
  22:00). Tested against `build-speedup` `5d8deb7` with
  `C_DEPEND_COMPILE_FLAGS`/`CXX_DEPEND_COMPILE_FLAGS = -MD -MP`, in the
  cache side's own installation (flesh checked out at that commit, then
  put back): 25 thorns, 307 of 307 C/C++ compiles keyed with the same keys
  as with the option off, and all 357 objects and 357 `.d` files byte for
  byte those of a build without the cache. Your uncommitted changes to
  `lib/sbin/CST` and `CSTUtils.pl` were not part of that test.
- 2026-10-01  cache side  A6 as rewritten (dependency output written by
  the compile) is through review on the cache side at `129ecf7`. Planned
  section updated: M1 waits for Max.
- 2026-10-02  cache side  M1 has Max's go-ahead; the planned section says
  what serving will change. Nothing has changed yet.
