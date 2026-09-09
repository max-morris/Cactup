+++
title = "MDB Overview"
description = "Understanding the two-layer machine database and how to add a cluster"
+++

# Machine Database (MDB) Overview

The **Machine Database (MDB)** defines how cactup interacts with your cluster. It specifies compilers, queues, batch system commands, submit scripts, and all the configuration for a particular machine.

## Two-layer MDB design

The MDB has two layers:

### System MDB

The system MDB lives at `~/.cactup/mdb/` — a git clone that cactup manages and updates for you. It contains definitions for common HPC clusters plus the `generic` fallback. Don't hand-edit it; add your own machines in the user overlay described below.

### User MDB

Your own machine definitions live in the user overlay at `~/.cactup/machines/`. These override system definitions and allow customization for local clusters or workstations.

Machine **name shadowing**: if you define `mylab.local` in the user MDB, it takes precedence over any system definition with the same name.

## Machine discovery

When you run cactup, it discovers which machine you're on:

1. If you specify `--machine myclu`, use that machine (skip discovery)
2. If you've run discovery before in this login shell on this host, use the cached result
3. Otherwise re-check the cached machine against the current hostname; if it no longer claims the host, run every machine's matcher (`hostname.regexp`, then `discover.py`; both MDB layers) to find one that does
4. If no match, fall back to `generic` (the built-in single-node workstation machine)

See [Machine Discovery](machine-discovery.html) for details on writing `hostname.regexp` and `discover.py`.

## Per-machine directory layout

Each machine (in the system or user MDB) is a directory with:

```
<mdb>/machines/<machine-name>/
  meta.toml                    # Machine metadata and configuration
  optionlists/
    default.toml               # One TOML per optionlist variant
    cuda.toml                  # (machine-specific names)
  submitscripts/
    default.sh                 # One shell script per submit variant
    test.sh                    # Special variant for `test submit`
  runscripts/
    default.sh                 # One shell script per run variant
    test.sh                    # Special variant for `test run`
  buildsubmitscripts/          # OPTIONAL — only clusters that forbid
    default.sh                 # login-node compiling need this one
  hostname.regexp              # Machine detection: one regex (optional)
  discover.py                  # Machine detection: Python fallback (optional)
```

### meta.toml

The **meta.toml** file defines:

- Machine identity: name, nickname, location, description, status
- Paths: install-home, simulation-home, test-home
- Hardware: max CPUs per node, memory, autodetect settings
- Build environment: make command, make-jobs default, and (optional) queued-build defaults
- Scheduler: submit, status, stop commands; queue patterns; environment setup
- Queues: one `[queues.<name>]` section per queue (GPU yes/no, max walltime)
- Variants: optionlist, submitscript, and runscript variants with metadata, plus an optional buildsubmitscript variant for clusters that queue builds

See [meta.toml Reference](meta-toml.html) for the full schema.

### Optionlists

An **optionlist** is a TOML file specifying compiler flags, optimization levels, enabled/disabled thorns, and build options. Examples:

- `default.toml` — standard CPU build
- `cuda.toml` — GPU/CUDA build
- `intel.toml` — Intel compiler variant
- `debug.toml` — debug symbols and checks

Users choose which variant when building: `cactup build myconfig --variant cuda`.

See [Optionlists](optionlists.html) for schema and template variables.

### Submit and run scripts

**Submit scripts** are shell scripts that render a simulation as a scheduler job script (SLURM `sbatch`, PBS `qsub`, etc.). cactup provides:

- Template variables like `@NODES@`, `@TASKS@`, `@WALLTIME@`, `@CHECKPOINT_WALLTIME@`
- Script variants for different submit modes (default, test)
- The machine's submit command to pass the script to the scheduler

**Run scripts** are similar but used for interactive runs — they don't submit to a queue, they execute directly (or under an allocation).

**Build submit scripts** are a third, optional kind: a sibling of a submit script that re-invokes `cactup build run` instead of `cactup sim run`. Only clusters that forbid compiling on the login node need one — most machines skip it entirely.

All three are shell (`.sh`) or Python (`.py`) scripts. See [Scripts & Variables](scripts-and-variables.html).

### hostname.regexp and discover.py

The **hostname.regexp** file holds one regular expression; the machine claims every host it matches (tested against the full hostname and its short form):

```
^mike\d+(\.hpc\.lsu\.edu)?$
```

A **discover.py** script is the fallback for sites where the hostname is not enough:

```python
def is_machine(hostname):
    return hostname.startswith("mike2.hpc.lsu.edu")
```

cactup checks every machine's regexp first and only runs the `discover.py` of machines whose regexp did not match (all of them in one `python3`). Exactly one claim wins; several make cactup ask. `generic` ships neither file, which is what makes it the fallback.

## Viewing machines

List all machines (system + user MDB combined):

```sh
cactup machine list
```

Show a machine's configuration:

```sh
cactup machine show mylab     # Show machine details
cactup machine show mylab --variants  # Show optionlist variants
```

Show the detected machine for the current host:

```sh
cactup machine show           # Show the current machine
```

## Adding a cluster to cactup

If you manage an HPC cluster or workstation, you can add it to cactup:

1. **Create a machine entry** in the user MDB:
   ```sh
   cactup machine create mylab --from-existing --silent
   ```

2. **Edit the generated meta.toml** to set compiler commands, queue names, paths, etc.

3. **Add optionlist variants** with compiler flags for your cluster

4. **Write submit and run scripts** that format jobs for your scheduler (SLURM, PBS, etc.)

5. **Write hostname.regexp** so your machine auto-detects (optional)

6. **Test with**:
   ```sh
   cactup machine show mylab
   cactup test run --machine mylab
   ```

See [Porting a Cluster](porting-a-cluster.html) for a complete walkthrough.

## Machine status

Machines in meta.toml have a `status` field indicating their maturity:

- `personal` — local/single-user machines, not centrally managed
- `test` — under development, not yet production-ready
- `production` — stable, actively supported

## Built-in machines

cactup ships with:

- **generic** — single-node workstation, no batch system (fallback for any unknown machine)
- **mike2.hpc.lsu.edu** (mike) — LSU HPC SuperMike cluster, SLURM scheduler

Other machines can be added to the system MDB as they are ported.

## Understanding universe

A **universe** is an optional build wrapper. Examples:

- Singularity container (`et-sif`)
- Module-loaded environment (`host`, `intel`)
- Native build (no universe, or `--no-universe`)

The build universe is **recorded** with your config and affects:

- How the config is built (container vs native, which modules are loaded)
- How simulations are submitted and run (they run in the same universe they were built in)

Machines can define universes in meta.toml to support hybrid builds (native + Singularity on the same cluster).

See [Building Configs](../users/building-configs.html) for how users interact with universes.

## Queue configuration

Queues represent scheduler partitions or queue names. Each machine has one or more queues:

```toml
[queues.default]
gpu = false
default = true
max-walltime = "24:00:00"

[queues.gpu]
gpu = true
max-walltime = "12:00:00"
```

Users submit to a specific queue with `-q <queue>`, and cactup validates that the optionlist variant is compatible.

## Next steps

- [meta.toml Reference](meta-toml.html) — detailed field documentation
- [Optionlists](optionlists.html) — compiler settings and thornlist configuration
- [Scripts & Variables](scripts-and-variables.html) — submit/run script templates
- [Machine Discovery](machine-discovery.html) — writing hostname.regexp / discover.py
- [Porting a Cluster](porting-a-cluster.html) — complete walkthrough of adding a machine
