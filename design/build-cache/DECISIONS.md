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
| Absolute paths in objects | **Relocatable by default**: compile with the Cactus root and the configuration directory mapped to fixed names, so installations can share objects. Objects then name files as `./arrangements/...`; a debugger needs a path substitution |
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
   needed. A Spack-built GCC has not been checked yet.

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

## Still open

- The cluster check of decision 3 on a Spack-built GCC (qbd's site-built
  GCC is checked: a link-only specs file).
- Whether the contract text in `CLAUDE-contract.md` goes into the
  repository's `CLAUDE.md` (untracked, shared by every session): asked
  when M1c lands, not before.
