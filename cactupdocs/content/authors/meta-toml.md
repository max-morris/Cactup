+++
title = "meta.toml Reference"
description = "Complete reference for the machine configuration file schema"
+++

# meta.toml Reference

The **meta.toml** file is the core configuration for a machine in the MDB. It defines machine identity, paths, hardware specs, scheduler commands, queue definitions, and variants.

## File structure overview

A typical meta.toml:

```toml
[machine]
name = "Example Cluster"
nickname = "example"
hostname = "example.hpc.edu"
# ... more machine fields ...

[paths]
install-home = "/home/@USER@"
simulation-home = "/scratch/@USER@/simulations"
test-home = "/scratch/@USER@/tests"

[hardware]
max-cpus-per-node = 128
memory = 262144  # MB per node

[build]
make = "make -j@MAKEJOBS@"
make-jobs = 128

[environment]
env-setup = """export PATH="/opt/bin:$PATH" """

[scheduler]
submit = "sbatch @SCRIPTFILE@"
allocation-env = "SLURM_JOB_ID"
# ... more scheduler fields ...

[queues.default]
gpu = false
default = true
max-walltime = "24:00:00"

[variants.optionlist]
variants = ["default", "cuda"]

[variants.submitscript]
"default" = { queues = ["default"], default = true }

[variants.runscript]
"default" = { queues = ["default"], default = true }
```

## Detailed field reference

{{cactup:mdb-meta}}

## Template variables

When values in meta.toml contain `@VAR@` tokens, they are substituted at runtime:

- `@USER@` — logged-in username
- `@HOSTNAME@` — hostname
- `@MAKEJOBS@` — number of parallel make jobs
- Variables from optionlists and scripts (see [Scripts & Variables](scripts-and-variables.html))

To include a literal `@` character, use `@@`.

## Common patterns

### Single-queue SLURM cluster

```toml
[machine]
name = "Example SLURM Cluster"
nickname = "example"
hostname = "example.hpc.edu"

[paths]
install-home = "/home/@USER@"
simulation-home = "/scratch/@USER@/simulations"
test-home = "/scratch/@USER@/tests"

[hardware]
max-cpus-per-node = 64
memory = 256000

[scheduler]
submit = "sbatch @SCRIPTFILE@"
allocation-env = "SLURM_JOB_ID"
get-status = "squeue -j @JOB_ID@"
stop = "scancel @JOB_ID@"
submit-pattern = "Submitted batch job ([0-9]+)"
status-pattern = "@JOB_ID@ "
queued-pattern = " PD "
running-pattern = " R "
exec-host = "hostname -s"
exec-host-pattern = "(\S+)"
stdout = "cat @SIMULATION_NAME@.out"
stderr = "cat @SIMULATION_NAME@.err"
stdout-follow = "tail -n 100 -f @SIMULATION_NAME@.out @SIMULATION_NAME@.err"
max-walltime = "24:00:00"

[queues.default]
gpu = false
default = true

[variants.optionlist]
variants = ["default"]

[variants.submitscript]
"default" = { queues = ["default"], default = true }
"test" = { queues = ["default"], test = true }

[variants.runscript]
"default" = { queues = ["default"], default = true }
"test" = { queues = ["default"], test = true }
```

### Cluster with CPU and GPU queues

```toml
[queues.cpu]
gpu = false
default = true
max-walltime = "24:00:00"

[queues.gpu]
gpu = true
max-walltime = "12:00:00"

[variants.optionlist]
variants = ["default", "cuda"]

[variants.submitscript]
"default" = { queues = ["cpu", "gpu"], default = true }

[variants.runscript]
"default" = { queues = ["cpu", "gpu"], default = true }
```

### Workstation with no batch system

```toml
[machine]
name = "Workstation"
nickname = "mylab"
hostname = "mylab.local"

[hardware]
autodetect = true

[scheduler]
submit = "exec nohup @SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & echo $!"
allocation-env = ""
get-status = "ps @JOB_ID@"
stop = "pkill -g $(ps -o pgid= -p @JOB_ID@)"
submit-pattern = "(.*)"
status-pattern = "^ *@JOB_ID@ "
queued-pattern = "$^"
running-pattern = "^"
exec-host = "echo localhost"
exec-host-pattern = "(.*)"
stdout = "cat @SIMULATION_NAME@.out"
stderr = "cat @SIMULATION_NAME@.err"
stdout-follow = "tail -n 100 -f @SIMULATION_NAME@.out @SIMULATION_NAME@.err"

[queues.local]
gpu = false
default = true

[variants.optionlist]
variants = ["default"]

[variants.submitscript]
"default" = { queues = ["local"], default = true }
"test" = { queues = ["local"], test = true }

[variants.runscript]
"default" = { queues = ["local"], default = true }
"test" = { queues = ["local"], test = true }
```

## Scheduler configuration details

The `[scheduler]` section defines how cactup interacts with your batch system. Here are the key patterns:

### SLURM (sbatch)

```toml
[scheduler]
submit = "sbatch @SCRIPTFILE@"
allocation-env = "SLURM_JOB_ID"
get-status = "squeue -j @JOB_ID@"
stop = "scancel @JOB_ID@"
submit-pattern = "Submitted batch job ([0-9]+)"
status-pattern = "@JOB_ID@ "
queued-pattern = " PD "
running-pattern = " R "
```

### PBS/Torque (qsub)

```toml
[scheduler]
submit = "qsub @SCRIPTFILE@"
allocation-env = "PBS_JOBID"
get-status = "qstat @JOB_ID@"
stop = "qdel @JOB_ID@"
submit-pattern = "([0-9.]+)"
status-pattern = "@JOB_ID@"
queued-pattern = " Q "
running-pattern = " R "
```

### No batch system (background execution)

```toml
[scheduler]
submit = "exec nohup @SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & echo $!"
allocation-env = ""
get-status = "ps @JOB_ID@"
stop = "pkill -g $(ps -o pgid= -p @JOB_ID@)"
submit-pattern = "(.*)"
status-pattern = "^ *@JOB_ID@ "
queued-pattern = "$^"
running-pattern = "^"
```

## Environment variables

The `[environment]` section can define machine-wide environment setup:

```toml
[environment]
env-setup = """
export PATH="/opt/bin:$PATH"
export LD_LIBRARY_PATH="/opt/lib:${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
module load gcc
"""
```

This is sourced before every build, submit, and run command.

## Universes

Define optional build wrappers:

```toml
[universes.et-sif]
wrapper-argv = ["singularity", "exec", "-B", "/scratch", "/path/to/et.sif"]
description = "Singularity container with Einstein Toolkit dependencies"

[universes.host]
# Identity universe: no wrapper, just environment setup
description = "Native host build"
```

Universes can override environment setup:

```toml
[universes.intel]
env-build-setup = """
module load intel
"""
description = "Intel compiler environment"
```

## Knobs (machine defaults)

Machines can provide default knob values (cluster-level settings for allocation, queue, etc.):

```toml
[knobs]
allocation = "my_project"
queue = "default"
mail-type = "all"
```

Users can override or set knobs with `cactup knob`:

```sh
cactup knob allocation my_other_project
```

## Validation tips

- All `@VAR@` tokens in patterns must appear in the actual scheduler output
- Queue names in `[variants.*]` must exist in `[queues.*]`
- `build-universes` lists (on queues and on `[variants.*]` entries) must name declared universes and may not be empty; omit the key for "all universes"
- Variant names must match files in `optionlists/`, `submitscripts/`, and `runscripts/` directories
- Test variants should be present in all `[variants.*]` sections (used by `cactup test run/submit`)

## Next steps

- [Optionlists](optionlists.html) — define compiler settings
- [Scripts & Variables](scripts-and-variables.html) — write submit/run scripts
- [Machine Discovery](machine-discovery.html) — write discover.py
- [Porting a Cluster](porting-a-cluster.html) — complete example
