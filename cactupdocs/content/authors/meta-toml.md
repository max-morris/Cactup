+++
title = "meta.toml Reference"
description = "Complete reference for the machine configuration file schema"
+++

# meta.toml Reference

The **meta.toml** file is the core configuration for a machine in the MDB. It defines machine identity, paths, hardware specs, scheduler commands, queue definitions, and variants.

The schema is closed: every table below accepts only the keys listed for it, and an unrecognized key fails the load with `unknown field \`x\`` plus the accepted spellings. A misspelled key is never silently ignored — so if you want to record something cactup has no key for, write it as a `#` comment.

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
# default-action = "submit"     # optional: force `cactup build` to queue a
                                 # build instead of running it in the
                                 # foreground (see "Queue-submitted builds"
                                 # below) — requires [variants.buildsubmitscript]

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

# Optional — only if your cluster forbids compiling on the login node:
# [variants.buildsubmitscript]
# "default" = { queues = ["default"], default = true }
```

## Detailed field reference

{{cactup:mdb-meta}}

## Template variables

When values in meta.toml contain `@VAR@` tokens, they are substituted at runtime:

- `@USER@` — logged-in username
- `@HOSTNAME@` — hostname
- `@MAKEJOBS@` — number of parallel make jobs
- Variables from optionlists and scripts (see [Scripts & Variables](scripts-and-variables.html))

To include a literal `@` character, use `@@`. Comments are left alone: a `#`
comment in a shell snippet is copied through verbatim, so a `@` there needs
no escape (see [Substitution rules](scripts-and-variables.html#substitution-rules)).

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
blocking-submit = "sbatch --wait @SCRIPTFILE@"
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

### Queue-submitted builds

A cluster that forbids compiling on the login node needs three things beyond
a normal machine: a `buildsubmitscript` variant, `[build].default-action`,
and (optionally) a build-specific job shape:

```toml
[build]
make           = "make -j@MAKEJOBS@"
make-jobs      = 64
default-action = "submit"     # cactup build always queues on this machine
queue          = "gpu"        # the build job's own queue — independent of
walltime       = "4:00:00"    # any run's queue/walltime; all six of these
nodes          = 1            # keys are optional and only ever consulted
tasks          = 1            # for a SUBMITTED build (a foreground `build
cpus-per-task  = 64           # run` ignores them entirely)

[variants.buildsubmitscript]
"default" = { queues = ["cpu", "gpu"], default = true }
```

`cpus-per-task` falls back to `make-jobs` when unset, so a machine that
already tunes `make-jobs` to its node size gets a matching build-job shape
for free. `default-action` is what makes an unqualified `cactup build`
submit instead of trying (and failing) to compile locally; leaving it unset
still lets cactup submit automatically whenever a `buildsubmitscript` variant
and `[scheduler].submit` are both present — `default-action = "run"` is the
escape hatch for a machine that *can* submit builds but shouldn't by default.
See [Building Configs](../users/building-configs.html) for the user-facing
side of this, and [Scripts & Variables](scripts-and-variables.html) for how
to write the buildsubmitscript itself.

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
blocking-submit = "@SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & pid=$!; echo $pid; wait $pid"
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

Note `blocking-submit` here has no `exec` and no `nohup`: the shell has to
stay alive to reach `wait`, and the job id is echoed *before* the wait so it
parses immediately instead of only after the job finishes.

## Scheduler configuration details

The `[scheduler]` section defines how cactup interacts with your batch system. Here are the key patterns:

### SLURM (sbatch)

```toml
[scheduler]
submit = "sbatch @SCRIPTFILE@"
blocking-submit = "sbatch --wait @SCRIPTFILE@"
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

`blocking-submit` is optional too: the same submission, but expressed so the
command doesn't return until the job has finished, for `cactup build submit
--block`. It's scanned for the job id with the same `submit-pattern` as
`submit` — the two must agree on format — and its own exit status is never
consulted as a build verdict; only whether a job id was parsed at all, since
that's the only way the submission itself can be said to have failed. A
machine that omits `blocking-submit` still supports `--block`: cactup just
submits normally and polls the build attempt instead of getting a free ride
from the scheduler. If you carry a trailing `; sleep N` on `submit` (to let
the scheduler register the job before an immediate status query), drop it
here — a blocking submit doesn't return until long after that would matter.

### PBS Pro / PBS/Torque (qsub)

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
# PBS Pro only — Torque's qsub has no blocking mode. Omit the key on Torque
# and cactup emulates --block by polling instead.
# blocking-submit = "qsub -W block=true @SCRIPTFILE@"
```

### No batch system (background execution)

```toml
[scheduler]
submit = "exec nohup @SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & echo $!"
blocking-submit = "@SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & pid=$!; echo $pid; wait $pid"
allocation-env = ""
get-status = "ps @JOB_ID@"
stop = "pkill -g $(ps -o pgid= -p @JOB_ID@)"
submit-pattern = "(.*)"
status-pattern = "^ *@JOB_ID@ "
queued-pattern = "$^"
running-pattern = "^"
```

Note the shape change from `submit`: no `exec`, no `nohup` — the shell has
to survive to reach `wait` — and the job id is echoed *before* the wait so
it parses immediately instead of only once the job is already done.

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

The standard knobs are `allocation`, `mail`, `mail-type`, `queue`, `user`, `email`,
`wisdom-frequency` and `wisdom-kind` (`mail-type`, `user`, and `email` fall back to
derived defaults when unset). Users can also create **custom knobs**
(`cactup knob -c my-name value`, removed with `cactup knob delete my-name`) and
override any knob for one command with `-K name=value`.

Scripts and optionlists may read knobs with `@KNOB(name)@` /
`@KNOB-OPTIONAL(name, default)@` — see
[Scripts & Variables](scripts-and-variables.html#substitution-rules). A machine
definition should not *depend* on a custom knob existing: the MDB is shared, and
a required `@KNOB(name)@` in a submit script fails for every user who has not
set it. Prefer `@KNOB-OPTIONAL(...)@` with a sensible default there, and reserve
the required form for the user's own parfiles. Knobs are **not** available in
`[paths]`, which resolve before any knob context exists. See the
[CLI reference](../reference/cli.html) for `cactup knob` and
[Running Simulations](../users/running-simulations.html#knobs) for the user's view.

## Validation tips

- All `@VAR@` tokens in patterns must appear in the actual scheduler output
- Queue names in `[variants.*]` must exist in `[queues.*]`
- `build-universes` lists (on queues and on `[variants.*]` entries) must name declared universes and may not be empty; omit the key for "all universes"
- Variant names must match files in `optionlists/`, `submitscripts/`, and `runscripts/` directories
- Test variants should be present in all `[variants.*]` sections (used by `cactup test run/submit`)
- `[variants.buildsubmitscript]` is the one script kind that's allowed to be entirely absent — a machine that never queues a build simply omits the table (and the `buildsubmitscripts/` directory) rather than needing an empty one
- `[build].default-action = "submit"` only works once a `buildsubmitscript` variant and `[scheduler].submit` are both declared; without either, `cactup build submit` errors naming what's missing (`cactup build run` always works regardless)
- `[scheduler].blocking-submit` is optional and needs no matching table of its own — a machine that omits it still supports `cactup build submit --block`, just via emulated polling instead of a native blocking submit — but when present, its output must parse with the same `submit-pattern` as `submit`

## Next steps

- [Optionlists](optionlists.html) — define compiler settings
- [Scripts & Variables](scripts-and-variables.html) — write submit/run scripts
- [Machine Discovery](machine-discovery.html) — write discover.py
- [Porting a Cluster](porting-a-cluster.html) — complete example
