# M4: CUDA (`nvcc`): design note

Status: draft for Max, 2026-10-12. Facts marked *(seen)* come from
read-only probes of CUDA 13.2.1 on saturn (`nvcc --version`,
`bin/nvcc.profile`, `nvcc -dryrun` with saturn's own `CUCCFLAGS`); the
rest is inferred and marked *(to check)*.

## How Cactus compiles CUDA

The MDB's GPU machines (saturn, athena, delta, perlmutter, et-cuda, ...)
set `CUCC = "nvcc"` and `CUCCFLAGS` with `--compiler-bindir <host g++>`,
`-x cu`, `--relocatable-device-code=true`, `--objdir-as-tempdir`,
`--gpu-architecture sm_80`, `--forward-unknown-to-host-compiler`,
`--extended-lambda`, `--expt-relaxed-constexpr`. Cactus compiles each `.cu`
file (and, with `-x cu`, the C++ files of CUDA thorns) with
`$(CUCC) $(CUCCFLAGS) ... -c`. Today the wrapper refuses `nvcc` ("not a
compiler cactup knows"), so these compiles run uncached.

## What one `nvcc -c` runs *(seen)*

1. `g++ -E` (the host pass), with `-D__CUDACC__`, `-D__NVCC__`,
   `-D__CUDA_ARCH_LIST__=800`, CUDA's include and `cccl` system include.
2. `cudafe++`: splits host and device code; writes `<obj>.cudafe1.cpp`,
   `.stub.c`, `.gpu`, and a module ID file (`--gen_module_id_file`); is
   given `--orig_src_file_name` and `--orig_src_path_name`.
3. `g++ -E` again (the device pass), with `-D__CUDA_ARCH__=800`.
4. `cicc` (from `nvvm/bin`, by `$CICC_PATH`): device code to PTX, with
   `libdevice` (`nvvm/libdevice`, by `NVVMIR_LIBRARY_DIR`).
5. `ptxas`: PTX to a cubin for `sm_80`.
6. `fatbinary`: cubin and PTX into a fat binary, embedded as C.
7. `g++ -c`: the host object, with the fat binary embedded.

With `--objdir-as-tempdir` every intermediate is written next to the
object (`<obj>.cpp1.ii`, `.cudafe1.*`, `.ptx`, `.fatbin`, `.module_id`, …)
and removed at the end.

`bin/nvcc.profile` sets the environment of those programs: `PATH +=
nvvm/bin:bin:`, `LD_LIBRARY_PATH += <top>/lib:`, `INCLUDES`,
`SYSTEM_INCLUDES`, `LIBRARIES`. With `LD_LIBRARY_PATH` unset this leaves
an empty entry (`.../lib:`), the working directory, in the environment of
every program `nvcc` runs *(seen)*.

## Proposed design

**Identity.** `nvcc` is identified like a GCC driver, with its own helper
set: `nvcc`, `nvcc.profile`, `cudafe++`, `cicc`, `ptxas`, `fatbinary`,
`nvlink` (not run by `-c`, hashed anyway for `-rdc` links later), `bin2c`,
the `libdevice` bitcode, and `nvcc --version`. Each found as `nvcc` finds
it (`nvcc.profile`'s `PATH`, `CICC_PATH`, `NVVMIR_LIBRARY_DIR`), with the
places passed over watched as for GCC. The host compiler named by
`--compiler-bindir` (or found on `PATH` as `nvcc` would) is identified as
a GCC is today, and its identity goes into the key. The libraries of each
program are watched in the environment `nvcc` gives them, and that
environment's empty `LD_LIBRARY_PATH` entry is a problem to settle first
(below).

**Key.** The text part: the output of both preprocessor passes as `nvcc`
would run them (host and device, per architecture), taken from `nvcc
-dryrun` for the compile's own arguments and run by cactup (as the C and
C++ key runs `-E` today), mapped as today. The files read by both passes
are the key's files. The arguments part: the compile's flags, keyed as
written for the ones the reader knows, with `nvcc`'s own flag list (the
CUDA reader declines anything it does not know, as the GCC reader does).
The check after the compile: both preprocessor passes again, as for C and
C++ today.

**What is stored.** The object only: the intermediates `--objdir-as-tempdir`
writes are removed by `nvcc` itself, and none is an output of the recipe.

## Open questions

1. **Determinism and the path map: answered** *(seen, CUDA 12.2.1 with
   GCC 11.4 on qbd, Max's go for writing probes, 2026-10-12)*. The same
   compile twice gives the same object, with and without `-g`. From two
   directories the objects differ in the module ID symbol
   (`_ba9d5637_4_a_cu_8e450714` against `_71c18592_4_a_cu_8e450714`):
   its first part is a hash of the name `nvcc` hands `cudafe++` as
   `--orig_src_path_name`, the source's resolved path (the working
   directory, the output name and a symlink on the way change nothing).
   With `-Xcudafe --orig_src_path_name=/fixed/a.cu` and the host compiler's
   `-ffile-prefix-map` (through `-Xcompiler`), the objects compiled from
   two directories are identical, with and without `-g`. So a compile the
   cache runs passes the source's mapped name that way, as the path map
   does for GCC, and CUDA objects are shared across installations. (The
   probes' sources each defined a routine of external linkage; for those
   that do not, the module ID's second part is another matter: below.)
2. **The empty `LD_LIBRARY_PATH` entry `nvcc.profile` adds: answered**
   (decision 22): such entries are watched from each compile's directory,
   not refused. The identity follows `nvcc`'s own environment for the
   programs it runs.
3. **Host compilers other than GCC**: Cray's `CC` (delta, perlmutter) is
   M4b (decision 23).
4. **Audit mode**: audit builds on qbd's compute nodes (A100, `gpu2`, by
   Slurm; Max, 2026-10-12: no load to worry about), then saturn and
   athena.

## The module ID (decisions 24 and 25) *(seen, qbd, CUDA 12.2.1)*

`cudafe++` names a translation unit's module ID (`__nv_module_id`, the
`__cudaRegisterLinkedBinary_`/`__fatbinwrap_` symbols, and every entity of
internal linkage, `_GLOBAL__N__<id>…`) `_<h1>_<length>_<name>_<h2>`, `h1` a
hash of `--orig_src_path_name`. Where the unit defines a routine or
variable of external linkage that serves, `h2` comes from its name and
nothing else changes it (mtime, inode, directory, host, time: tried).
Where none does (86 of 99 CarpetX units), `h2` is made of the working
directory (`getcwd`), the time the source file was last changed rendered as
local time (`TZ` changes it; a file that cannot be found gives the time of
the compile), and the process ID, appended as `_<pid>`. Cactus compiles from
each configuration's `scratch` directory, on a copy of each source made
for that configuration: every configuration, and every copy, made another
object. `-Xcudafe --module_id=…` is accepted and ignored; `cudafe++`'s debug
levels do not print the strings it hashes.

Without `--gen_module_id_file`, `cudafe++` reads the ID from the file
`--module_id_file_name` names, as `cicc` does after it, and every name
above carries it (tried). So a compile the cache runs is `nvcc`'s own dry
run (`-dryrun`, of the compile with the map's flags), run step by step by
cactup in the environment it gives, with that one flag left out and an ID
made of the key in `cudafe++`'s shape (`cuda::Plan`, `cuda::module_id`):
the same twice, from any directory, at any time, under any process ID;
three runs without a PID namespace gave identical objects. A step that
fails ends the compile with its exit status and its messages, as `nvcc`
does (tried with an undefined identifier: both exit 2, same words). The
identity's trial compiles a unit without an external routine in two
places with one given ID and requires the ID in both objects
(`takes_module_id`), and the objects equal (`relocates`); an `nvcc` that
does not take the ID is not cached. The PID namespace of decision 24 is
gone with it (kept on `feature/build-cache-m4-pidns`).
