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
get-status-many = "squeue -h -u @USER@ -o '%i %t (%r)'"
stop = "scancel @JOB_ID@"
submit-pattern = "Submitted batch job ([0-9]+)"
status-pattern = "@JOB_ID@ "
queued-pattern = " PD "
running-pattern = " R "
exec-host = "hostname -s"
exec-host-pattern = '(\S+)'
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
get-status-many = "squeue -h -u @USER@ -o '%i %t (%r)'"
stop = "scancel @JOB_ID@"
submit-pattern = "Submitted batch job ([0-9]+)"
status-pattern = "@JOB_ID@ "
queued-pattern = " PD "
running-pattern = " R "
```

`get-status-many` is optional: one call instead of one per job. It must list every
live job of `@USER@`, one per line, with the job id as the first field; the rest of
the line is classified by the same `status`/`queued`/`running`/`holding` patterns
used for `get-status`. A job id absent from the listing is treated as not queued.

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
kind = "apptainer"   # informational only; cactup does not switch on it
wrapper-argv = ["singularity", "exec", "-B", "/scratch", "/path/to/et.sif"]

[universes.host]
# Identity universe: no wrapper, just environment setup
```

A universe declares at most one wrapper form — `wrapper-argv` (a prefix argv, run as
`<wrapper-argv…> /bin/sh -c <command>`) or `wrapper` (a single shell template
containing exactly one `@COMMAND@` token, which must sit **outside** the template's
own quotes). Declaring neither gives an identity universe, useful purely as a carrier
for env-setup overrides.

Universes can override environment setup key-by-key (each set `env-*` key replaces the
machine's; unset keys inherit):

```toml
[universes.intel]
env-build-setup = """
module load intel
"""
```

## Knobs (user defaults)

Knobs are **not** part of meta.toml. They are per-user default values (allocation,
queue, mail settings) stored in cactup's global database under `~/.cactup`, not in
the machine definition. A `~/.cactup` lives on exactly one machine, so knobs need no
machine keying. Set them with `cactup knob`:

```sh
cactup knob allocation my_project
cactup knob queue default
```

The known knobs are `allocation`, `mail`, `mail-type`, `queue`, `user`, and `email`
(`mail-type`, `user`, and `email` fall back to derived defaults when unset). See the
[CLI reference](../reference/cli.html) for `cactup knob`.

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
