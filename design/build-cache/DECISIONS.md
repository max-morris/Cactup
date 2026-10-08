# Build cache: what Max has decided

The decisions that bind the work, in one place, with the date, the words
where they were given in words, and what each means for the code. The
reasoning and the numbers behind them are in `PLAN.md` (the first set) and
`RESULTS-M0c.md` (the second set). When a decision here and anything else
on the branch disagree, this file is right and the other is stale.

## Standing requirements (2026-10-01, the original request)

- **Never a stale object.** "Outdated objects are NEVER used when a fresh
  build is actually required, no matter the reason." A miss is always
  acceptable; a wrong hit never is.
- **No corruption under concurrency.** "Multiple active builds,
  installations, and Cactup session should NOT corrupt the build cache."
- **Careful eviction.** An updated thorn's old objects stay: "I might want
  to revert to that old version later or in a different installation."
- **The project keeps its shape.** One static MUSL binary, pure-Rust
  dependencies. A custom cache is acceptable; breaking that is not.
- **Rust thorns later must stay possible.**
- **Mixed architectures on one filesystem never share objects** (Frank:
  athena, saturn). Key on the cactup machine and on whatever else shows
  that hardware or software changed. A change of machine keys differently
  and invalidates nothing; "invalidate here does not imply evict."

## How the work is done (2026-10-01)

- **Where builds run.** Everything that writes goes in the cactup-managed
  installation `~/cacti/build-cache` (and its sibling `build-cache-b`).
  `/home/max/Cactus-2026` is for mixed-precision work and is read-only
  reference at most. Never write in `~/cacti/speedup-build`.
- **Review gates.** "At reasonable milestones in the implementation, gate
  progress behind a twin pair of harsh reviewer agents, who verify
  correctness, code quality, performance, safety/security, and adherence
  to cactup's principles, design philosophy, and UX language. Iterate until
  the reviewers sign off."
- **Branch.** A worktree on `feature/build-cache`, tracked on the remote.
  master is not touched. No AI attribution in commits.
- **The build-speed work.** "You two need to keep mutual compatibility."
  Coordination is by the contract file
  (`~/tmp/build-cache-speedup-contract/CONTRACT.md`) and by direct message
  where a session can be reached; sessions are not stable ("I sometimes
  rotate claude code accounts"), so nothing may depend on a session's
  identity.

## Design choices (2026-10-01, before the plan)

| Question | Decision |
|---|---|
| Existing tool or our own | **Our own, built into cactup** (no existing cache handles Fortran modules or a store shared between hosts over NFS without POSIX locks) |
| Absolute paths in objects | **Relocatable by default**: compile with the Cactus root and the configuration directory mapped to fixed names, so installations can share objects. Objects then name files as `./arrangements/...`; a debugger needs a path substitution (names changed to `/cactup-root/...` by decision 8) |
| First deliverable | **A measurement pass** before anything is stored or served (done: `RESULTS-M0c.md`) |
| Eviction | **Explicit only, with a size notice.** Nothing is deleted automatically |

## After the measurements (2026-10-02)

Asked as seven questions with options; the answers, in Max's numbering.

1. **M1 goes ahead.** The store, then serving with audit mode, then the
   `cache` maintenance commands, each behind the review gate. The
   acceptance gate for serving stays as planned: a full audit build in two
   installations with zero mismatches.

2. **Fortran comes right after C and C++ are served, ahead of CUDA.**
   It is about a third of the compile time on a real thornlist (31% and
   35% in the two 275-thorn builds). Finish M1 for C and C++ first: the
   store, serving and audit mode are shared, and audit mode is what will
   prove Fortran. Then gfortran, then the CUDA compilers.

3. **GCCs with a `specs` file: check before building anything.** Such a
   GCC is not cached today, and Spack is believed to write that file into
   the GCCs it builds. Nobody has checked on a cluster. The check is one
   command per cluster, `gcc -print-file-name=specs` with the build's
   modules loaded: an absolute path back means the file exists. Only then
   is the refinement built (accept a specs file that differs from the
   built-in specs, `gcc -dumpspecs`, in link sections only, and includes
   no other file). **Waiting on that check**: Max runs it, or gives a way
   to run it.

   *Checked on qbd, 2026-10-02* (module `gcc/13.2.0`, a site-built GCC
   at `/usr/local/packages/compilers/gcc/13.2.0`): it has a specs file,
   so today the cache declines every C and C++ compile there. Against
   `gcc -dumpspecs` the file changes one built-in section,
   `*link_libgcc:` (`%D` becomes `%(link_libgcc_rpath) %D`), and adds
   one, `*link_libgcc_rpath:` (`-rpath
   /usr/local/packages/compilers/gcc/13.2.0/lib64`). Link step only: the
   refinement would accept it. That settles that the refinement is
   needed.

   *Spack-built GCCs checked, 2026-10-07*, on mike (GCC 10.3.0 and
   11.2.0) and Deep Bayou, `db1` (11.2.0, 8.4.0, 9.3.0): each is a Spack
   install (`.spack/` with its build record in the prefix) and each has a
   specs file, in one of two shapes. The newer Spack writes qbd's
   (`*link_libgcc:` gains `%(link_libgcc_rpath)`, a new section holds the
   `-rpath`); the older one (db1's 8.4.0 and 9.3.0, `spec.yaml` era)
   puts `-rpath <prefix>/lib:<prefix>/lib64` at the front of `*link:` and
   drops the file's final blank line. `specs::link_only` accepts all five,
   run on each compiler's own `-dumpspecs`, specs file and driver. (The
   link-only list was read off GCC 14's driver; GCC 8 to 11 build the
   link command the same way.)

4. **Thornlist changes costing hits: accepted for M1.** The key keeps
   covering the bytes of every file a compile reads. To be revisited once
   audit mode exists, "since that is the tool that can prove a narrower
   key on real compiles." Nothing narrower is to be built before then.

5. **The locale leaves the key, behind a trial, in M1b.** For a compiler
   that passes a per-compiler trial (the same source, non-ASCII bytes
   included, compiled under two locales gives one object), `LANG`, `LC_*`
   and `LANGUAGE` are not keyed; a compiler that fails it keeps them. The
   stored compiler messages are then in whatever language the first build
   had. Narrowing to `LC_CTYPE` alone was rejected: it "would not fix the
   measured case" (a session without `LANG` against one with it).

6. **`-march=native` on a host with mixed cores stays uncached.** No
   pinning of compiles to one kind of core.

7. The two known ways the cache could change what gets compiled:

   a. **A thorn's own compile recipe, defined indirectly: stand down for
      the thorn.** Besides a mention of `COMPILE_`, a thorn whose
      `make.code.defn` or `make.code.deps` contains an `include`
      directive, a `define`, or `$(eval` is left to plain make. (Counted
      on the Einstein Toolkit master tree on 2026-10-02: none of 348
      thorns has any of these, so it costs nothing there.)

   b. **A name the recipe's shell redefines at startup: ask the shell.**
      Once per build and compiler, ask the recipe's own shell what the
      compiler's name resolves to, and leave the compile to the shell
      when that is not the program the cache found. Reviewed with M1a.
      If it turns out not to work, the limit is accepted as stated. What
      is ruled out is turning the cache off wherever `BASH_ENV` is set:
      "Module systems are common so we want them to work."

## During M1b (2026-10-07)

8. **The path map names the tree with absolute placeholders.** Asked
   because a relocated object recorded `./arrangements/...` relative to a
   compile directory `./configs/@config/scratch`, which a debugger cannot
   resolve. Answer: "Absolute placeholders". The Cactus root maps to
   `/cactup-root/` and the configuration directory to
   `/cactup-root/configs/@config/`; objects record those (also in
   `__FILE__`), and gdb is pointed at them with two `set substitute-path`
   rules, the configuration's first (a reviewer showed that with Cactus's
   default, no line directives, the sources are the copies in the
   configuration's `build`, which the root rule alone does not reach). This replaces the 2026-10-01 design choice's
   `./arrangements/...` names.

9. **A directory renamed away and back during a compile: a stated
   limit.** Asked after a reviewer published a wrong object by swapping a
   symlinked include directory during a compile and swapping it back.
   Answer: "Stated limit". Every entry a keyed file's name resolves
   through (directories and symlinks) is watched by device and inode, so a
   symlink swapped and swapped back (it is a new symlink each time), or a
   directory replaced by another, fails the check after the compile. What
   is not watched is a directory's change time, which moves whenever
   anything is created inside it (the build tree, `$HOME`) and would stop
   honest compiles from being published; so the very same directory
   renamed away and renamed back while a compile reads through it is
   written into spec §18.5's limits. Audit mode would still catch an object
   it produced.

10. **The contract goes into `CLAUDE.md` when the cache lands on master**
    (2026-10-07: "When it lands on master"). `CLAUDE-contract.md` stays on
    the branch; it is added to `CLAUDE.md`, before the "On-disk formats"
    section, in the same step that merges `feature/build-cache`, so no
    session reads rules for code its tree lacks.

11. **The machine database says where the cache lives** (2026-10-07: "New
    MDB key"). Asked after a side question found the store defaulting to
    `$HOME`, where a cold Einstein Toolkit build writes about half a
    gigabyte from compute nodes. `[paths] build-cache-home`, resolved and
    frozen like `simulation-home`; the user's `build-cache-dir` knob wins
    over it, `$CACTUP_HOME/cache` is the fallback. Set per machine beside a
    per-user `simulation-home` on scratch or work (not on machines whose
    `simulation-home` is a home tree or another person's directory). MDB
    generation 2. *Superseded the same day by decision 12.*

12. **The cache lives beside the installations; every path has a knob**
    (2026-10-07, replacing decision 11's placement). Max: "the build cache
    should be a sibling of wherever the build objects are already placed
    anyway, since some level of build i/o is already expected there. Would
    that be the installation directory? That should be more robust than a
    scratch dir. The build-cache directory in these existing MDBs should be
    migrated to somewhere akin to what the installation directory is
    already set to. Furthermore, I think these default directories should
    all have knob overrides, not just the build cache dir." So: the store's
    default is `<install-home>/.cactup-build-cache` (the `install-home`
    fallback included, `~/.cactup/cacti`); no machine in the MDB sets
    `build-cache-home`, which stays an optional `[paths]` key for a site
    whose builds belong elsewhere, so MDB generation 2 is dropped (no
    machine uses the key; the schema gaining it is not breaking). Each
    `[paths]` key has a maintenance knob of the same name, an absolute path
    that wins over the machine's value (`install-home`, `simulation-home`,
    `test-home`, `scratch-home`, `build-cache-home`); `build-cache-dir` is
    gone, `build-cache-home` replaces it. Asked whether to drop the
    optional `[paths] build-cache-home` key, since the knob covers one
    user: "I say keep it. I can see a site wanting to override it."

## Fortran (2026-10-07)

13. **Fortran objects name `/cactup-root/` paths too.** gfortran writes a
    source's path into the object for its runtime error messages ("At line
    7 of file ..."): the main file's name as given, or the path in a line
    marker; GCC 14 applies no `-f*-prefix-map` to it. Asked whether to
    compile a renamed copy (line markers naming the mapped paths, so
    Fortran is shared between configurations and installations and its
    runtime errors name `/cactup-root/...` files, as C's `__FILE__` does
    under decision 8) or to key the paths and share Fortran only within one
    configuration path. Answer: "Rename, share". As built (after the
    Einstein Toolkit audit found the first way wrong in 63 objects, below):
    the compile names its source by its path from the working directory
    (`../build/<Thorn>/x.f90`, the same in every tree), since gfortran's
    runtime checks write the name it was given ("In file '...', around line
    7"); a source whose line markers name files is compiled as a copy with
    those names mapped, under the source's own file name (a module file
    records it) in `.cactup/` beside the source, named from the working
    directory too, with the source's directory first in `-I`. Runtime
    errors then name `/cactup-root/...` files where Cactus wrote line
    markers, and `../build/<Thorn>/x.f90` where it did not.

    The first way (a copy in a private directory, mapped by a prefix map)
    passed the small tests and two-tree trials, which used no runtime
    checks: the "In file" string named the private directory, so every
    build made different objects of 63 sources, and audit mode said so.

## Still open

- (Answered: decision 3's cluster check, on qbd's site-built GCC and on
  five Spack-built GCCs on mike and db1: all link-only.)
- (Answered: the contract goes into `CLAUDE.md` with the merge, decision
  10.)
