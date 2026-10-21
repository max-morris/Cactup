+++
title = "Optionlists"
description = "Define compiler flags, optimizations, and enabled thorns for your machine"
+++

# Optionlists

An **optionlist** is a TOML file that specifies how to build Cactus on your machine.
It has two parts:

- A `[cactup]` header — cactup-only metadata (where the binary may run, default
  flag, build universe, per-flavor thorn toggles). This is **never** written
  to the native Cactus optionlist.
- An `[options]` table — the raw Cactus optionlist `NAME = value` entries (compilers,
  flags, feature switches). cactup renders these back to Cactus's native
  `NAME = value` format before building, then substitutes any `@NAME@` tokens.

Each machine can have multiple optionlists, called **flavors** (e.g., `default`,
`cuda`, `debug`). Users choose which flavor when building with `--flavor`.

> [!NOTE]
> While porting a machine you do not have to install an optionlist into the MDB
> to try it. `cactup build <config> --optionlist <path>` builds from any file —
> a full optionlist like the ones below, an `[options]` table on its own, or the
> native Cactus `.cfg` you are porting *from*. A file with a `[cactup]` header
> is validated exactly as if it already lived in `optionlists/`, so you can
> settle the header before committing it. See
> [Building Configs](../users/building-configs.html) for the details.

> [!NOTE]
> An optionlist's `universe` field (below) and a machine's queue-submitted
> builds (`[build].default-action`/`[buildsubmitscripts]`, covered in
> [meta.toml Reference](meta-toml.html)) are independent. A universe says
> *where* `make` runs — a container, a module-loaded shell, the bare host.
> Queue submission says *how the build reaches a machine* — through the
> batch scheduler instead of your terminal. A cluster can use either, both,
> or neither.

## File structure

Optionlists are TOML files in the `optionlists/` directory of your machine:

```
~/.cactup/machines/<machine>/optionlists/
  default.toml
  cuda.toml
  debug.toml
```

## Header: [cactup]

The optional `[cactup]` header carries cactup-only metadata. It may be omitted
entirely (all fields default):

```toml
[cactup]
default = true                    # the implicit choice among this machine's flavors
description = "Default optimized build for my cluster"
universe = "et-sif"               # build this flavor inside this universe (optional)
enabled-thorns = ["ExternalLibraries/OpenBLAS"]   # per-flavor thorn toggles, on top of [build]
disabled-thorns = ["ExternalLibraries/LORENE"]

[cactup.runs-on]                  # where the built binary may run
queues = ["cpu"]                  # queues its simulations may be submitted to; omit for any
gpu = false                       # binary GPU capability; cross-checked vs the queue's gpu flag
default-queue = "cpu"             # where its simulations go without -q (optional; one of queues)
```

None of these keys are emitted to the native optionlist. Both `[cactup]` and
`[cactup.runs-on]` reject keys they do not know, so a misspelling fails the
load instead of being ignored. `runs-on` describes the *binary*: it is checked
when a simulation or test is submitted or run, never against the queue a
`cactup build submit` job compiles on.

`default-queue` is where a simulation or test of this flavor goes when the
user passes no `-q` and has no `queue` knob this flavor can use. Without it,
that is the machine's default queue, which a flavor may not be allowed to
use. A CPU-only flavor on a machine whose default partition is a GPU one is
the usual case, and setting `default-queue` saves its users from typing `-q`
on every submit. It must be one of `queues` when that list is given, and a
queue the machine has; the build refuses it otherwise. Unlike the rest of
`runs-on`, which is recorded with each config when it is built, cactup reads
`default-queue` from the flavor's current file whenever a job is submitted,
so changing it takes effect for existing configs without a rebuild.

A change to the `[options]`, to `universe`, or to the thorn toggles rebuilds
a config built from the flavor. Editing anything else in the header
(`runs-on`, `default`, `description`, `coerce-run-universe`), or any comment,
does not: those never reach the compiler, and the next `cactup build` just
records the new values. The header fields are documented in full below:

## Generated reference

{{cactup:mdb-optionlist}}

## Options: [options]

The `[options]` table holds the raw Cactus optionlist entries as `NAME = value` pairs.
Values must be strings, booleans, or integers — **floats are rejected** (quote a
dotted value as a string if you really need one). `VERSION` is required and is always
emitted first; whenever it changes, the config is reconfigured and rebuilt from
scratch.

```toml
[options]
VERSION = "2018-12-13"

CPP = "cpp"
CC  = "gcc"
CXX = "g++"

FPP = "cpp"
F90 = "gfortran"

CFLAGS   = "-g -std=gnu99"
CXXFLAGS = "-g -std=gnu++17"
F90FLAGS = "-g -fcray-pointer -ffixed-line-length-none"
LDFLAGS  = "-rdynamic"

OPTIMISE           = "yes"
C_OPTIMISE_FLAGS   = "-O2"
CXX_OPTIMISE_FLAGS = "-O2"
F90_OPTIMISE_FLAGS = "-O2"

DEBUG   = "no"
WARN    = "yes"
OPENMP  = "yes"
MPI     = "MPICH"
```

These are standard Cactus optionlist names — the same ones you would put in a
hand-written Einstein Toolkit `.cfg` optionlist. cactup does not invent its own
compiler-flag schema; it renders `[options]` straight back to the native format.
Booleans render as Cactus's `yes`/`no`, integers as plain decimals, strings verbatim.

> **Tip:** Keeping `yes`/`no` values as TOML strings (`OPTIMISE = "yes"`) makes the
> render byte-identical to the original Cactus optionlist. Writing them as TOML
> booleans (`OPTIMISE = true`) also works and renders to `yes`/`no`.

## Complete example optionlist

```toml
[cactup]
default = true
description = "Optimized build for SLURM cluster with GCC"

[cactup.runs-on]
queues = ["cpu", "long"]
gpu = false

[options]
VERSION = "2024-06-01"

CPP = "cpp"
CC  = "gcc"
CXX = "g++"
FPP = "cpp"
F90 = "gfortran"

CFLAGS   = "-g -std=gnu99"
CXXFLAGS = "-g -std=gnu++17"
F90FLAGS = "-g -fcray-pointer -ffixed-line-length-none"

OPTIMISE           = "yes"
C_OPTIMISE_FLAGS   = "-O2 -march=native"
CXX_OPTIMISE_FLAGS = "-O2 -march=native"
F90_OPTIMISE_FLAGS = "-O2 -march=native"

WARN         = "yes"
C_WARN_FLAGS = "-Wall"

OPENMP           = "yes"
CPP_OPENMP_FLAGS = "-fopenmp"

MPI = "MPICH"
```

## Multiple flavors on one machine

A machine can offer flavors for different build environments:

```
optionlists/
  default.toml          # CPU, native build
  cuda.toml             # GPU/CUDA
  debug.toml            # Debug symbols
```

There is nothing to declare in meta.toml: a machine's flavors are simply the
`*.toml` files in its `optionlists/` directory.

Users choose with:

```sh
cactup build myconfig --flavor cuda
```

With several flavors, exactly one should be marked `default = true` in its
`[cactup]` header; that is the implicit choice when `--flavor` is omitted. A
machine with a single flavor uses it implicitly.

## GPU optionlists

For GPU builds, mark `gpu = true` under `[cactup.runs-on]` (so its simulations only
run on GPU queues) and add the CUDA-specific options:

```toml
[cactup]
description = "CUDA GPU build"

[cactup.runs-on]
queues = ["gpu"]
gpu = true

[options]
VERSION = "2024-06-01"

CC  = "gcc"
CXX = "g++"
F90 = "gfortran"

CUCC       = "nvcc"
CUCCFLAGS  = "-std=c++17 -arch=sm_80"

OPTIMISE = "yes"
OPENMP   = "yes"
MPI      = "MPICH"
```

## Per-flavor thorn toggles

Different flavors sometimes compile different thorns — e.g. a CUDA flavor that
drops a thorn nvcc cannot build. Put those toggles in the `[cactup]` header; they are
merged on top of the machine-level `[build].enabled-thorns`/`disabled-thorns`:

```toml
[cactup]
disabled-thorns = ["ExternalLibraries/LORENE", "EinsteinInitialData/Meudon_Bin_BH"]
enabled-thorns  = ["ExternalLibraries/OpenBLAS"]

[cactup.runs-on]
gpu = true
```

## Container vs native

If your machine supports both a container and native builds, create separate
optionlists and point each at its build universe:

```toml
# optionlists/native.toml
[cactup]
default = true
description = "Native build on host"
universe = "host"

[options]
VERSION = "2024-06-01"
CC = "gcc"
# ...
```

```toml
# optionlists/container.toml
[cactup]
description = "Build inside the Einstein Toolkit container"
universe = "et-sif"

[options]
VERSION = "2024-06-01"
CC = "gcc"
# ...
```

Users can override the build universe with `--universe` or `--no-universe`:

```sh
cactup build myconfig --no-universe    # ignore the universe, build natively
cactup build myconfig --universe host  # use a different universe
```

The build universe is recorded with the config and (unless `coerce-run-universe` is
set false in the header) also becomes the default universe its simulations run in.

## Validating optionlists

```sh
cactup machine show --flavors          # list flavors and their headers
cactup build myconfig --flavor <name>  # try building with it
```

## Common issues

**Missing `VERSION`**: every `[options]` table must declare `VERSION`; it is emitted
first and a change forces a full rebuild.

**Floats**: `[options]` values may only be strings, booleans, or integers. Quote a
dotted value as a string if you need it.

**Missing compiler**: if `gcc`/`nvcc` lives in a module, load it in the machine's
`[environment]` section so it is on `PATH` when the build runs.

## Next steps

- [Scripts & Variables](scripts-and-variables.html) — define submit/run scripts
- [meta.toml Reference](meta-toml.html) — route scripts to queues in the machine definition
- [Porting a Cluster](porting-a-cluster.html) — complete example
