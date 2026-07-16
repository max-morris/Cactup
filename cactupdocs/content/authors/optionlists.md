+++
title = "Optionlists"
description = "Define compiler flags, optimizations, and enabled thorns for your machine"
+++

# Optionlists

An **optionlist** is a TOML file that specifies how to build Cactus on your machine. It includes:

- Compiler command and flags
- Optimization levels (e.g., `-O2`, `-O3`, debug symbols)
- Enabled/disabled thorns and their options
- Build-time configuration like CCTK variables

Each machine can have multiple optionlist **variants** (e.g., `default`, `cuda`, `intel`). Users choose which variant when building.

## File structure

Optionlists are TOML files in `optionlists/` directory of your machine:

```
<mdb>/machines/<machine>/optionlists/
  default.toml
  cuda.toml
  debug.toml
```

## Header: [cactup]

Every optionlist must start with a `[cactup]` section:

```toml
[cactup]
version = 2
description = "Default optimized build for my cluster"
enabled-thorns = ["CactusBase", "CactusEinstein"]
```

- `version` — required, must be `2`
- `description` — optional, human-readable description
- `enabled-thorns` — optional, list of thorn arrangements/names to enable
- `disabled-thorns` — optional, thorns to explicitly disable
- `universe` — optional, default build universe for this variant
- `build-type` — optional: `debug`, `optimize`, `profile`, `unsafe` (not typically needed; users control via `cactup build --debug`, etc.)

## Compiler configuration: [build.*]

Specify the C, C++, and Fortran compilers and flags:

```toml
[build.c]
command = "gcc"
flags = "-Wall -std=c99"

[build.cxx]
command = "g++"
flags = "-Wall -std=c++11"

[build.fortran]
command = "gfortran"
flags = "-Wall"
```

## Optional compiler variants

Users can request build variants via `--debug`, `--optimize`, etc. Define variant-specific flags:

```toml
[build.c.debug]
flags = "-g -O0 -Wall"

[build.c.optimize]
flags = "-O2 -Wall"

[build.c.unsafe]
flags = "-Ofast -march=native"
```

When a user runs `cactup build myconfig --optimize`, cactup uses `[build.c.optimize]` flags instead of the base flags.

## Thorn configuration: [thorns.*]

Configure individual thorns or arrangements:

```toml
[thorns."CactusEinstein/ADMBase"]
config-file = "configurations/my_admbase_config"

[thorns."McLachlan/ML_BSSN"]
enabled = true
```

## Macros and options: [options.*]

Set Cactus build macros:

```toml
[options.CCTK_BUILD_SYSTEM]
value = "inet"

[options.MPI]
value = "yes"
```

## Generated reference

{{cactup:mdb-optionlist}}

## Complete example optionlist

```toml
[cactup]
version = 2
description = "Optimized build for SLURM cluster with GCC"
enabled-thorns = [
  "CactusBase",
  "CactusEinstein",
  "EinsteinEOS",
  "EinsteinUtils",
  "CactusNumerical",
]
disabled-thorns = ["CactusTest"]

[build.c]
command = "gcc"
flags = "-Wall -std=c99"

[build.c.debug]
flags = "-g -O0 -Wall"

[build.c.optimize]
flags = "-O2 -march=native -Wall"

[build.c.profile]
flags = "-O2 -g -Wall -fno-omit-frame-pointer"

[build.cxx]
command = "g++"
flags = "-Wall -std=c++11"

[build.cxx.optimize]
flags = "-O2 -march=native -Wall"

[build.fortran]
command = "gfortran"
flags = "-Wall -ffree-line-length-none"

[build.fortran.optimize]
flags = "-O2 -march=native -ffree-line-length-none"

[options.MPI]
value = "yes"

[options.PTHREADS]
value = "yes"

[thorns."CactusEinstein/ADMBase"]
enabled = true

[thorns."McLachlan/ML_BSSN"]
enabled = true
enabled-options = ["BSSN_DRIVE_SHIFT=yes"]
```

## Multiple variants on one machine

A machine can offer variants for different build environments:

```
optionlists/
  default.toml          # CPU, native build
  cuda.toml             # GPU/CUDA
  intel.toml            # Intel compiler
  debug.toml            # Debug symbols
```

In meta.toml, declare them:

```toml
[variants.optionlist]
variants = ["default", "cuda", "intel", "debug"]
```

Users choose with:

```sh
cactup build myconfig --variant cuda
```

## GPU optionlists

For GPU builds, include GPU-specific compiler flags and thorns:

```toml
[cactup]
version = 2
description = "CUDA GPU build"

[build.c]
command = "gcc"
flags = "-Wall -std=c99 -I/usr/local/cuda/include"

[build.cxx]
command = "g++"
flags = "-Wall -std=c++11 -I/usr/local/cuda/include"

[build.fortran]
command = "gfortran"
flags = "-Wall -ffree-line-length-none"

[options.CUDA]
value = "yes"

[options.CUDA_PATH]
value = "/usr/local/cuda"
```

## Container vs native

If your machine supports both Singularity containers and native builds, create separate optionlists:

```toml
# optionlists/native.toml
[cactup]
version = 2
description = "Native build on host"
universe = "host"

# optionlists/container.toml
[cactup]
version = 2
description = "Build inside Singularity container"
universe = "et-sif"
```

## Validating optionlists

Test that an optionlist is valid and compatible:

```sh
cactup machine show --variants     # List variants and their details
cactup build myconfig --variant <name>  # Try building with it
```

## Common issues

**Unknown thorn**: Misspell a thorn name and the build will fail. Check the Einstein Toolkit repository.

**Missing compiler**: If `gcc` is in a module, load it in the machine's `[environment]` section before the build runs.

**Flag conflicts**: Different compilers have different flag meanings. Test flags on your system before committing them.

**Unicode in TOML**: Keep optionlist files in UTF-8 encoding, no BOM.

## Integration with universes

Optionlists can specify a default build universe:

```toml
[cactup]
universe = "et-sif"  # Build inside this Singularity container
```

Users can override with `--universe` or `--no-universe`:

```sh
cactup build myconfig --no-universe    # Ignore the universe, build natively
cactup build myconfig --universe host  # Use a different universe
```

The build universe is recorded with the config and affects how it runs.

## Next steps

- [Scripts & Variables](scripts-and-variables.html) — define submit/run scripts
- [meta.toml Reference](meta-toml.html) — declare variants in the machine definition
- [Porting a Cluster](porting-a-cluster.html) — complete example
