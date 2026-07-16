+++
title = "Building Configs"
description = "Build Cactus configurations with variants, universes, and customized compiler settings"
+++

# Building Cactus Configurations

A Cactus **configuration** is a compiled Cactus executable built from your installation with specific thorns, compiler settings, and optionlist variant. This guide covers everything you need to know about building configs.

## Basic build

Build a config with default settings:

```sh
cactup build myconfig
```

This creates a config named `myconfig` using:
- The active Einstein Toolkit installation
- The machine's default optionlist variant
- Default compiler flags (no optimization, no debugging)

## Build options

### Optimization and debugging

```sh
cactup build myconfig --optimize   # -O2 optimization
cactup build myconfig --debug      # Debug symbols and checks
cactup build myconfig --profile    # Profiling support
cactup build myconfig --unsafe     # Fast-math style (-Ofast) — use with care
```

### Parallel builds

Control how many parallel `make` jobs to use:

```sh
cactup build myconfig -j 8         # 8 parallel jobs
cactup build myconfig -j max       # All available threads
cactup build myconfig              # Machine default (usually 1)
```

The `-j max` option is useful in batch environments: it uses all threads available in the current compute context (e.g., an `srun` allocation), not just the login node.

### Reconfiguration and cleaning

Force a full rebuild:

```sh
cactup build myconfig --reconfig --clean --force
```

- `--reconfig` — Re-run `configure` before building
- `--clean` — Run `make clean` before building
- `--force` — Rebuild even if the config is already built

### Thornlist

Use a custom thornlist:

```sh
cactup build myconfig --thornlist /path/to/custom.th
```

By default, cactup uses the installation's built-in thornlist.

### Virtual executables

For testing or special cases, copy a prebuilt `cactus_<config>` into place instead of building:

```sh
cactup build myconfig --virtual-executable /path/to/cactus_myconfig
```

This skips `configure` and `make` entirely, useful when the executable was built elsewhere.

## Optionlist variants

Your machine may provide multiple **optionlist variants** — different compiler environments or configurations. For example:

- `default` — CPU build
- `cuda` — GPU/CUDA build
- `intel` — Intel compiler variant

List available variants:

```sh
cactup machine show --variants
```

Build with a specific variant:

```sh
cactup build myconfig --variant cuda
```

If your machine has multiple variants, you must choose one explicitly (unless one is marked default). Each variant can have different compiler flags, GPU support, and compatible queues for job submission.

## Universes

A **universe** is an optional build environment that wraps the build process. Common examples:

- **Singularity container** — executes `make` inside a container image
- **Module-loaded shell** — sources module files before building
- **Native host** — no wrapping (default)

If your machine defines universe options, use:

```sh
cactup build myconfig --universe et-sif
```

Or force the native host context:

```sh
cactup build myconfig --no-universe
```

The universe is **recorded** with your config and affects how simulations are submitted and run. See [Running Simulations](running-simulations.html) for how universe affects job submission.

## Viewing build status

See all configs in the active installation:

```sh
cactup config list
```

View details of a specific config:

```sh
cactup config show myconfig
```

Set the active config (the default for `sim` and `test` commands):

```sh
cactup config use myconfig
```

## Deleting configs

Remove a config and its build artifacts:

```sh
cactup config delete myconfig
```

By default, cactup prevents deletion if the config is attached to any simulations. Force deletion:

```sh
cactup config delete myconfig --force
```

This removes the config but does not touch existing simulations (though they may become unable to run).

## Understanding build variants and universes

**Variants** and **universes** allow machines to support diverse build environments without fragmenting the MDB (machine database). Here's how they work:

### Optionlist variants

An **optionlist** is a TOML file specifying compiler flags, optimization levels, and enabled thorns. A machine can have multiple variants:

- Each variant has a name (`default`, `cuda`, etc.) and a description
- You choose which variant when building with `--variant`
- The variant determines the compiler environment and enabled features
- Variants are configured per-machine in the machine definition

### Universes

A **universe** is an optional build wrapper (Singularity, modules, etc.):

- The optionlist can specify a default universe, or you can override with `--universe`
- `--no-universe` forces native (host) compilation
- The recorded universe affects how simulations run — they run in the same universe they were built in
- Useful for machines that support both container-based and native builds with different optimizations

### Common patterns

**Single build environment**: Most machines have one optionlist variant and no universe wrappers. Just run:

```sh
cactup build myconfig
```

**GPU vs CPU**: Machines with both CPU and GPU queues often have two optionlist variants:

```sh
cactup build cpu-config --variant default
cactup build gpu-config --variant cuda
```

**Container + native**: Some machines support both Singularity and native builds:

```sh
cactup build container-build --universe et-sif
cactup build native-build --no-universe
```

## Full build command reference

{{cactup:cli command="build"}}

{{cactup:cli command="config build"}}

{{cactup:cli command="config list"}}

{{cactup:cli command="config show"}}

{{cactup:cli command="config use"}}

{{cactup:cli command="config delete"}}

## Troubleshooting

**"Must specify --variant"**: Your machine has multiple optionlist variants. Use `cactup machine show --variants` to list them, then `--variant <name>`.

**"Variant not found"**: Check the spelling with `cactup machine show --variants`.

**Build fails**: Check the build log in the config directory. Use `cactup show` to find the installation path, then look for `configs/myconfig/` inside it.

**"Universe not found"**: If the machine defines optional universes, use `cactup machine show` to see available ones.

## Next steps

- [Running Simulations](running-simulations.html) — submit or run a simulation with your built config
- [Test Suites](test-suites.html) — validate your config against the test suite
