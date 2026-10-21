# Build cache: what the measurements say (M0c)

2026-10-01, on `plato` (16 cores of three kinds, local disk), GCC 14.2,
GNU make 4.4.1, a static release build of cactup at `045eb76`, `-j 8`.
Nothing is stored or served yet: every number below comes from builds in
record mode, which key each compile and log the key, and from `cactup cache
report`, which compares two builds' logs. "Would be served" means the other
build has a successful compile with the same key.

The builds are in `~/cacti/build-cache` (A) and `~/cacti/build-cache-b`
(B, a second `cactup install master`); logs and scripts in
`~/tmp/build-cache-m0c/`. Every timing is one run.

## The short version

- **A second installation of the same sources would get every C and C++
  compile that goes through the cache from it**: 2781 of 2782 keyed
  compiles on a 275-thorn build, the one miss being a file that embeds the
  compile time. (Five more objects are built by thorns' own rules and
  never reach the cache.)
- **That is 65% of the compile time, not 100%, because Fortran is not
  cached yet** and is about a third of the compile time on a real
  thornlist (31% in A's build, 35% in B's; 2% on the 25-thorn sample).
- **Recording cost 6 to 9% wall time** on the whole build (one plain run,
  two record runs). A serving cache pays that same cost on a miss; on a
  hit it pays about half of it and does not compile.
- **Record mode leaves objects untouched**: 3380 of 3381 byte-identical to
  a build without the cache, the other one again the compile-time file.
  (A serving cache will compile with the path map's flags: its objects
  then name files as `./arrangements/...`, which is the "relocatable
  paths" decision, and is not what was measured here.)
- Three things cost hits and are yours to decide: generated headers that
  change with the thornlist, the locale, and `-march=native` on this
  workstation. A fourth will matter on clusters: Spack-built GCCs.

## The large build

`et`: the Einstein Toolkit master thornlist without the CarpetX stack (42
entries removed: ADIOS2 and AMReX do not find the from-source MPI on this
host, which has nothing to do with the cache). 275 thorns, 3381 objects,
external libraries (hwloc, OpenMPI, HDF5, ...) built from source.

| | |
|---|---|
| Wall time, plain build | 548 s (one run, after the record run) |
| Wall time, record mode | 597 s in A (+9%), 581 s in B (+6%); one run each |
| Compiles through the wrapper | 3376 (5 objects are built by thorns' own rules for utility programs) |
| Keyed | 2782: every C (2224) and C++ (558) compile |
| Not keyed | 594 Fortran compiles |
| Compile time, summed | 1512 s: C 20%, C++ 48%, Fortran 31% |
| Keying, summed | 115 s (8% of compile time); 41 ms per keyed compile |
| Check after the compile, summed | 105 s (7%); 38 ms per keyed compile |
| Files read per keyed compile | 150 on average, 0.5 MB of preprocessed text |
| Objects identical to the plain build | 3380 of 3381 (`HTTPD/Content.c` uses `__DATE__` and `__TIME__`) |

Most of the 548 s is not Cactus compiling: 1512 s of compile time over 8
jobs is about 190 s of wall time, the rest is external libraries,
configuration and linking, which the cache does not touch. On a cluster
where the libraries come from modules the compiles are most of a build.

## What a cache filled by one build would serve another

`smoke` below is its attempt 0013 throughout (`--against-attempt 13`: later
attempts of `smoke` are the small edit-and-revert rebuilds).

| This build | Against | Would be served | Share of compile time |
|---|---|---|---|
| B `et` (other installation) | A `et` | 2781 of 2782 | 65% (the rest is Fortran) |
| A `smoke`, rebuilt with `-f` | A `smoke`, the build before | 307 of 307 | 98% |
| A `smoke2` (other configuration name, 25 thorns) | A `smoke` | 307 of 307 | 98% |
| A `ld1` (line directives on) | A `ld2` | 307 of 307 | 98% |
| B `ld1` (other installation, line directives on) | A `ld1` | 307 of 307 | 98% |
| A `smoke2`, built from a fresh login environment, same `LANG` | A `smoke` | 307 of 307 | 98% |
| A `smoke2`, fresh environment without `LANG` | A `smoke` | 0 of 307 | 0% |
| A `ext` (25 thorns plus HDF5, zlib and two thorns using them) | A `smoke` | 185 of 334 | 64% |

Edit and revert, on `smoke`:

| Change | Recompiled | Would be served by the build before the edit |
|---|---|---|
| A comment added to `WaveToy.c` | 1 | 0 |
| The file put back | 1 | 1 |
| A comment added to `Boundary.h` | 10 | 0 |
| The header put back | 10 | 10 |

So the cases the cache is for behave as intended: a new installation, a
configuration under another name, a forced rebuild, and going back to an
earlier state of a thorn, all find their objects, with line directives on
or off. Not measured: a rebuild forced by an option list edit. One that
changes a compile flag changes every key, as it must; one that does not
(`VERSION`, a linker flag) should behave like `-f`.

## What costs hits

1. **Fortran: 31% of compile time, not cached.** The plan has it after C
   and C++ serve, one compiler family at a time (it needs module files in
   the key). The large build says it should not wait long.
2. **Generated headers that list the thorns.** The key covers the bytes of
   every file a compile reads, because debug information records columns
   and (Clang) a checksum of each file. Adding thorns changes headers that
   nearly every source includes (`cctk_DefineThorn.h`,
   `CParameterStructNames.h`), so 116 compiles of `ext` miss that would
   have matched on their preprocessed text alone (55% served where 90%
   was possible).
   Options: accept it; or compile with `-gno-column-info` when serving and
   key the text and line numbers only (the debugger then knows lines but
   not columns); or key bytes only for files that contribute text. The
   last two need the same kind of proof the path map got.
3. **The locale is in the key.** A build from a session without `LANG`
   shares nothing with one that has `LANG=en_US.UTF-8`
   (`fresh-env.sh` beside the logs is how that was run). A batch job and a
   login shell can differ that way. The locale is keyed because a compiler
   may read its source by it and words its messages by it; GCC and Clang
   in practice do neither for the object. Options: keep it; or key only
   `LC_CTYPE` and `LC_ALL`; or take the locale out and store the
   compiler's messages as they came.
4. **`-march=native` on a host with mixed cores is not keyed.** On `plato`
   GCC resolves `native` to three different sets of cache parameters by
   the core it lands on: one compile, run twice, gave two objects. None of
   the builds above uses the flag; 11 MDB optionlists do, on clusters,
   where nodes have one kind of core.
5. **A GCC with a `specs` file on disk is not keyed.** Flags from such a
   file never pass the cache's reader. Spack writes one into the GCCs it
   builds (for the path of `libgcc`), so on a Spack-based cluster nothing
   would be cached as things stand. A refinement that stays sound: accept
   a specs file whose every section except the link ones equals the
   built-in one. Not built yet.

## What recording costs

Per keyed compile: two extra preprocessor runs (41 ms and 38 ms on
average, against 373 ms for the compile) and two reads of the 150 files
they name. On the whole build that was +9% wall time in A and +6% in B,
against A's one plain run; three runs are not a statistic. A serving cache does the first
run always and the second only on a miss, so a miss costs what recording
costs (plus storing the object, not measured), and a hit costs the first
run in place of the compile. On a network filesystem the file reads will cost more than here;
that is not measured (this host has no NFS or Lustre), and a memo of file
digests per build would cut it at the price of trusting change times.

## Compatibility with the build-speed work

Its flesh branch `build-speedup` (at `5d8deb7`) can have the compiler
write dependency files while compiling (`-MD -MP -MF ... -MT ...`). As
signed off at `045eb76` the cache declined every such compile. It now
understands those flags (not in the key, kept from its preprocessor runs).
Checked on that branch with the option on (configs `dep` and `dep0`,
recorded by a debug build of the working tree, not by the release binary
the other numbers come from): 307 of 307 keyed, the same keys as with the
option off, and all 357 objects and 357 dependency files byte for byte
those of a build without the cache (`objs-dep-record.txt` and
`objs-dep-plain.txt` beside the other logs; both lists were copied there
together afterward, so they do not themselves show which build each was
taken from. Both reviewers checked the claim independently: one rebuilt
the 25 thorns on that branch both ways in a tree of its own, the other
compared objects and dependency files compile by compile through the
wrapper). The contract file says what
has to stay true of that recipe.

## Not measured

A network filesystem; a compute node; a container universe; any compiler
but GCC 14.2 (Clang 19.1 only in the test suite); CUDA; the full thornlist
with CarpetX.
