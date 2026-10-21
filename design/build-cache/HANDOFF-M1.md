# Build cache: handoff for the start of M1

Written 2026-10-02 for whoever starts M1, who may be a session that has
seen none of what came before. It says where things stand, what M1 is,
what to do first, and what is known that the code does not say.

## Read these, in this order

1. `DECISIONS.md`: what Max has decided. It wins over everything else.
2. This file.
3. `STATUS.md`: the rules that are easy to forget (its first section),
   what each finished milestone is, and every review verdict. The
   verdicts are worth reading for the kind of hole the reviewers found.
4. Spec §18 in `design/cactup-simfactory-design-new.md`: the cache as it
   is. §18.5 ("Keys") is the part M1 stands on, with its list of what the
   key rests on.
5. `RESULTS-M0c.md`: the measurements.
6. `PLAN.md`, sections "Store", "Eviction", "Audit mode", "Fortran and
   Rust": the design M1 implements. The plan is as approved; where it and
   the spec differ about the injection or the key, the spec is right.
7. The contract with the build-speed work,
   `~/tmp/build-cache-speedup-contract/CONTRACT.md` (a copy:
   `CONTRACT.snapshot.md`).
8. `/home/max/src/Cactup/CLAUDE.md`: cactup's contracts (interrupts,
   progress, static linking, link()-based locking, compute-node
   isolation, no backward compatibility, MDB generations, no §-references
   in anything a user reads).

## Where things stand

The cache **records and nothing more**. With the knob `build-cache =
record`, every Cactus object compile runs through `cactup __cc`, which
keys it, runs it unchanged, checks the key again, and logs one line.
`cactup cache report` compares two builds' logs. No object is stored,
none is served, and no flag is added to any real compile.

Everything on `feature/build-cache` up to `129ecf7` has passed the review
gate (two reviewers, both signed off on that commit). The commits after
it are documents only.

On 2026-10-02 Max gave the go-ahead for M1 and answered the open
questions (`DECISIONS.md`, "After the measurements").

## What M1 is

Serving C and C++ objects (GCC and Clang) from an instance-wide store.
Three parts, each ending at the review gate.

**M1a: the store, and the points carried over.**

- The store itself, per `PLAN.md` "Store": one immutable, self-describing
  file per key under `<root>/v<N>/<machine>/<2 hex>/<key>`; published by
  temp file, `sync_all`, `hard_link`, and the `nlink == 2` check that
  `src/lock.rs` uses; no lock and no index; restored by copying while
  verifying a checksum into a temp sibling, then `rename`; anything that
  does not verify is a miss and is removed.
- The cache root: knob `build-cache-dir`, default `$CACTUP_HOME/cache`,
  resolved by `prepare` and frozen into `<attempt>/cc/config.toml`
  (`objcache::BuildConf`). The compute-node path (`build run
  --config-dir`) may read that file and the cache root and nothing else
  of cactup's state: an addition to the D11 list that spec and
  `CLAUDE.md` have to state.
- Decision 7a: stand down for a thorn whose make fragments contain an
  `include` directive, a `define` or `$(eval` (today:
  `probe::inject_mk` looks for `COMPILE_` in `cat` of the two files).
  Match directives, not the bare word: `findstring include` would also
  hit comments and names like `INCLUDE_DIRS`.
- Decision 7b: once per build attempt and compiler name, ask the
  recipe's shell what the name resolves to (`<shell> -c 'command -v
  <name>'`, in the recipe's environment), and leave compiles to the shell
  when the answer is not the file `identity::find_program` finds.
  Remember the answer beside the compiler's identity
  (`<attempt>/cc/compilers/`), tied to what it depends on (the shell,
  `BASH_ENV` and `ENV` and the files they name, `PATH`). If this cannot
  be made sound, say so to Max: the fallback he chose is to accept the
  limit as spec §18.4 states it, never to turn the cache off where
  `BASH_ENV` is set.
- Decision 3's refinement (added 2026-10-02, after qbd's check showed a
  link-only specs file): accept a GCC `specs` file whose differences
  from `gcc -dumpspecs` are confined to sections only the link step
  reads, and new sections only those reference, with no `%include`,
  `%rename` or language entries; the file's bytes join the compiler's
  identity. Anything else stays declined.
- Two small points left by the last review: a doubled blank line after
  `in_english` in `src/objcache/key.rs`; no test pins that
  `identity::ask` runs the driver in English.

**M1b: serving.**

- A new value of the `build-cache` knob that serves (and one that
  audits). On a hit: restore the object, replay the stored compiler
  messages, exit 0, no compile. On a miss: compile, check the key again,
  publish if it still holds.
- **The miss compile gets the path map's flags** when the key was made
  with the map (`key::PathMap::flags`): the stored object must be the
  one the key describes. This is the first time the cache changes a real
  compile, and the point where a build with the cache on stops being
  byte-identical to one without (paths read `./arrangements/...`). The
  plan has a knob to turn the mapping off per build.
- A dependency file asked for by the recipe (`Compile::depend`) has to
  exist after a hit: give the key's preprocessor run those flags in
  serving mode, so that it writes the file (shown to be byte for byte
  what the compile writes). Contract A6 says what the recipe must keep
  true for this.
- Decision 5: a per-compiler trial for the locale, like
  `identity::relocates` for paths. A compiler that makes one object of
  the same source (non-ASCII bytes in it) under two locales is keyed
  without `LANG`, `LC_*`, `LANGUAGE`; one that does not keeps them.
- Audit mode: on a hit, compile anyway and compare bytes; on a mismatch
  compile once more to tell a wrong hit from a compiler that is not
  deterministic; the build keeps the fresh object either way.
- **The gate for M1b** is Max's: a full audit build in two installations
  (`build-cache`, `build-cache-b`) with zero mismatches, besides the
  reviewers' sign-off. Clang has to be audited too, not only GCC.

**M1c: living with the store.**

- `cactup cache stats`, `cache gc --unused-for <age> [--to-size <n>]`,
  `cache verify`, and a notice when a size knob is exceeded. Nothing is
  ever deleted automatically. `gc` runs under a heartbeat
  `lock::LinkLock`, with ages in the fileserver's clock.
- These commands walk many files: `par::parallel_map`, prodash progress,
  interrupt polling, as `CLAUDE.md` requires.
- The text in `CLAUDE-contract.md` goes into `CLAUDE.md` only with Max's
  word (the file is untracked and shared by every session).
- User documentation: the "in development" section of
  `cactupdocs/content/users/building-configs.md` becomes the real one.

**After M1**, in this order (decision 2): gfortran, then the CUDA
compilers, later a Rust adapter. And once audit mode exists, decision 4
comes back: whether the key can be narrower than "the bytes of every
file read".

## First steps

1. Read the list above. Check that the worktree is clean at the tip of
   `origin/feature/build-cache` and that `cargo test` passes (commands
   below).
2. Read the contract file for anything the build-speed side has written
   since 2026-10-01, and look at its flesh branch
   (`~/cacti/speedup-build/Cactus/repos/flesh`, branch `build-speedup`,
   read-only). It had uncommitted changes to `lib/sbin/CST` and
   `CSTUtils.pl` on 2026-10-01.
3. Ask Max for the outcome of the cluster check (decision 3), once. Do
   not build the specs-file refinement before it.
4. Write the M1a design into spec §18 before the code: the entry format,
   publish and restore step by step with what a crash at each step
   leaves behind, the D11 addition. The reviewers read the spec against
   the code.
5. Build M1a. Then the gate.

## What M0 learned that M1 must not forget

- **The standard is: one key, one object.** Every review round was
  spent on pairs of compiles that shared a key and differed in the
  object. The reviewers find these by running real compilers. For
  anything new that enters or leaves the key, write the two-tree audit
  first (`tests/objcache.rs`, `compiles_that_share_a_key_produce_the_same_object`
  and its neighbors) and reason second.
- **Try, do not assume, per compiler.** What a compiler does with
  overlapping prefix maps, what it writes into debug information, where
  it takes flags from: each was wrong when assumed and is now tried
  (`identity::relocates`) or asked (`key::flags_from_elsewhere`).
- **Fail open, always.** Whatever the cache cannot do, the compile runs
  as `make` asked. In serving this gets harder: a restore that fails
  half way, a store that is full or unreachable, a signal during
  publish. Each has to end in a plain compile or a clean exit, never in
  a wrong or partial object. `make` deletes the target of an interrupted
  recipe; a restore must not leave one that looks finished.
- **Signals.** The wrapper forwards stop signals to the compiler and,
  after the compile, dies at once by the signal (the handler does it,
  because a preprocessor's back end can keep a pipe open). Publishing
  happens after the compile: decide what a signal there does, and test
  it the way `a_signal_during_the_check_after_the_compile_is_not_waited_out`
  does.
- **Compiles with no one object** are declined, not keyed: listed in
  spec §18.5. `-march=native` on a host with mixed cores is one, and
  `plato` is such a host.
- **An entry is immutable, but "last used" is not.** The plan restamps
  last use after a build from its hit log. `CLAUDE.md` wants ages in the
  fileserver's clock, stamped by writing bytes. That is a design
  question for M1a or M1c, not solved in the plan.
- **Cost on a network filesystem is not measured.** Each keyed compile
  reads about 150 files twice. A per-attempt memo of file digests would
  save most of it and would trust change times where the check now reads
  bytes. Deferred until there are numbers from NFS or Lustre.
- **The log format changes freely** (no backward compatibility); `cache
  report` says so when it meets an older log.

## Working facts

**Repository.** Worktree
`/home/max/src/Cactup/.claude/worktrees/build-cache`, branch
`feature/build-cache`, remote `origin`. Never `cargo fmt`. American
spelling everywhere (a hook rejects British spellings in files written
with the edit tools; edits made from a shell bypass it, so check those by
hand). §-references only in comments.

**Tests.**

```
CACTUP_TEST_MAKES=$HOME/tmp/build-cache-tools/make-4.2.1/make:$HOME/tmp/build-cache-tools/make-4.3/make cargo test
CC_x86_64_unknown_linux_musl=musl-gcc cargo test --target x86_64-unknown-linux-musl --test objcache
cargo check --all-targets
```

The two extra makes were built from GNU sources during M0 and copied to
`~/tmp/build-cache-tools/` so that they outlive `/tmp`. The system make
is 4.4.1. `tests/objcache.rs` skips what needs a compiler the host lacks;
this host has GCC 14.2 and Clang 19.1.

**Real builds.** Installations `build-cache` (`~/cacti/build-cache`) and
`build-cache-b`, both `release master`. In `~/cacti/build-cache`:

| File | What |
|---|---|
| `smoke.th` | 25 thorns, C, C++ and Fortran: a build takes about 20 s |
| `et-trim.th`, `et.toml` | the master thornlist without the CarpetX stack (275 thorns), and the option list that builds it here (`HWLOC_EXTRA_LIBS = udev`): about 10 minutes at `-j 8` |
| `linedir.toml` | line directives on |
| `depend.toml` | dependencies written by the compile (needs the flesh at the build-speed branch) |
| `ext.th`, `ext.toml` | 25 thorns plus HDF5 built from source |

```
cactup -K build-cache=record build <config> --installation build-cache [--thornlist ...] [--optionlist ...] [-f] -j 8 < /dev/null
cactup cache report <config> --installation build-cache --against <config> [--against-installation build-cache-b] [--against-attempt N]
```

Turn the cache on per command with `-K`, never with `cactup knob`: the
database is shared with other sessions. The reference for "objects
untouched" is a plain `-f` build of the same configuration, compared by
`sha256sum` over `build/**/*.o`; `HTTPD/Content.c.o` differs between any
two builds (it uses `__DATE__` and `__TIME__`).

The full master thornlist does not build on `plato` (ADIOS2 and AMReX do
not find the from-source MPI). That is the host, not the cache.

**Measurement leftovers.** `~/tmp/build-cache-m0c/`: the release binary
the measurements were taken with (`cactup`, at `045eb76`), the script
(`full.sh`), logs and object hash lists.

**Sandbox.** Builds in `~/cacti`, writes under `~/tmp`, and `git push`
need the sandbox off. `/tmp` is a tmpfs that was 97% full on 2026-10-01,
most of it another project's: keep build directories out of it.

**Scratch space for reviewers.** They build in the worktree's `target/`
(a subdirectory of it for exports of other commits), not under `/tmp`.

## The review gate, in practice

Reviewer agents belong to the session that started them; a new session
starts its own pair. What worked:

- Two reviewers, started together, same brief, neither told of the
  other's findings or of the author's opinion of the code.
- The brief names the commit and the diff range (`git diff <last gate>
  <commit>`), the documents to read (`CLAUDE.md`, spec §2.4 and §18,
  `DECISIONS.md`, `STATUS.md`), and what to judge: correctness (above
  all, two compiles with one key and two objects; any way the cache
  changes or loses a compile; for M1, any way a stored object could be
  wrong, partial, or served to the wrong compile, and any way two builds
  at once could damage the store), code quality, performance, safety and
  security, and adherence to cactup's contracts, design philosophy and
  UX language.
- It tells them to reproduce and not to trust, to say for each finding
  whether they ran it or inferred it, to modify nothing in the
  repository, `~/cacti`, `~/.cactup`, and to end with exactly
  `VERDICT: SIGN-OFF` or `VERDICT: BLOCKED (n blocking findings)`.
- Fix every blocking finding, take the non-blocking ones or record why
  not, and send the new commit to both. The gate is passed when both
  sign off on the same commit. Record each round in `STATUS.md`.
- **Do not touch the worktree while they work.**

## The build-speed work

Its session cannot be reached by message from here; the contract file is
the channel, and it has not written in it yet. At every gate: read the
file, update the cache-side sections and the snapshot, and test against
its flesh branch in the cache side's own installation (fetch the branch
into `~/cacti/build-cache/Cactus/repos/flesh`, check it out detached,
build, put the flesh back on `master`). On 2026-10-01 the branch was
`build-speedup` at `5d8deb7`; with its option
`C_DEPEND_COMPILE_FLAGS = -MD -MP` the cache keyed every C and C++
compile and left objects and dependency files untouched.

What M1 changes for that side, to be written into the contract when it
lands: real compiles gain `-ffile-prefix-map=...` flags; a hit runs no
compiler; a hit's dependency file is written by a preprocessor run. Its
timing measurements must not be taken with the cache serving.

## Waiting on Max

- The cluster check for `specs` files (decision 3) on a Spack-built GCC.
  qbd's site-built GCC is checked (2026-10-02): link-only.
- Nothing else. M1 has his go-ahead.
