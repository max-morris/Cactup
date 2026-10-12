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

1. **Determinism and the path map** *(to check; needs a real compile, so
   writing a scratch directory on saturn)*: is the object the same when
   compiled twice, and the same from two directories under the path map?
   The module ID (`--gen_module_id_file`, needed by `-rdc`) and the
   `--orig_src_*` names may embed a random number, a time, or the source's
   path. If the module ID is random, `-rdc` objects cannot be cached as
   they are; if it embeds the path, only within one configuration.
2. **The empty `LD_LIBRARY_PATH` entry `nvcc.profile` adds**: with
   decision 18's rule, every `nvcc` compile would be refused whenever the
   user's `LD_LIBRARY_PATH` is unset. Either the identity follows `nvcc`'s
   own environment and watches that search from each compile's directory
   (decision 20 would then cover a library appearing there within one
   compile), or such compiles are refused. A decision for Max.
3. **Host compilers other than GCC**: delta and perlmutter name Cray's `CC`
   wrapper as the host compiler, which the cache refuses today; those
   machines would stay uncached unless the wrapper is seen through.
4. **Audit mode**: audit builds on saturn and athena (sm_80) prove it, as
   for GCC; mike's NVIDIA nodes (by Slurm) after.
