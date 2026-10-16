+++
title = "meta.toml Reference"
description = "Complete reference for the machine configuration file schema"
+++

# meta.toml Reference

The **meta.toml** file is the core configuration for a machine in the MDB. It defines machine identity, paths, hardware specs, scheduler commands, queue definitions, and the routes that pick a submit, run, or build-submit script for each queue.

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
                                 # below) — requires [buildsubmitscripts]

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

# Each key names a script file (submitscripts/default.sh, ...); each entry
# routes queues to it. The optionlist flavors need no table: they are the
# *.toml files in optionlists/.
[submitscripts]
"default" = { queues = ["default"] }

[runscripts]
"default" = { queues = ["default"] }

# Optional — only if your cluster forbids compiling on the login node:
# [buildsubmitscripts]
# "default" = { queues = ["default"] }

# Overlay machines only (~/.cactup/machines/<name>/): the MDB generation
# this file was written for. `cactup machine create` fills it in.
[cactup]
mdb-generation = 2
```

See [MDB Generations](mdb-generations.html) for what `mdb-generation` means
and how to update a machine written for an older generation.

A `[paths]` key may also be `build-cache-home`, where the build cache keeps
its objects; no machine needs it, since the cache's default is beside the
installations in `install-home`. Every `[paths]` key is a default for each
user: a knob of the same name, an absolute path, wins over it.

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

[submitscripts]
"default" = { queues = ["default"] }
"test" = { queues = ["default"], test = true }

[runscripts]
"default" = { queues = ["default"] }
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

[submitscripts]
"default" = { queues = ["cpu", "gpu"] }

[runscripts]
"default" = { queues = ["cpu", "gpu"] }
```

The machine's two flavors, `optionlists/default.toml` and
`optionlists/cuda.toml`, need no entry here; each says where its binary may
run in its own `[cactup.runs-on]` header (see [Optionlists](optionlists.html)).

### Queue-submitted builds

A cluster that forbids compiling on the login node needs three things beyond
a normal machine: a `[buildsubmitscripts]` route, `[build].default-action`,
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

[buildsubmitscripts]
"default" = { queues = ["cpu", "gpu"] }
```

The build job's queue is only where `make` runs: it is never checked against
the flavor's `[cactup.runs-on]` queues or `gpu` flag, nor against a queue's
`build-universes`, so any queue can compile any config — a CPU flavor on a GPU
partition or the other way round. That queue (`-q`, else `[build].queue`, else
the default queue) routes the build-submit script; `--flavor` never picks one.
On a `[buildsubmitscripts]` route, `build-universes = ["X"]` means "serves
builds of configs whose build universe is X".

`cpus-per-task` falls back to `make-jobs` when unset, so a machine that
already tunes `make-jobs` to its node size gets a matching build-job shape
for free. `default-action` is what makes an unqualified `cactup build`
submit instead of trying (and failing) to compile locally; leaving it unset
still lets cactup submit automatically whenever a `[buildsubmitscripts]` route
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

[submitscripts]
"default" = { queues = ["local"] }
"test" = { queues = ["local"], test = true }

[runscripts]
"default" = { queues = ["local"] }
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
export LD_LIBRARY_PATH="/opt/lib${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}"
module load gcc
"""
```

Write path lists so they never get an empty entry: `${LD_LIBRARY_PATH:+:$LD_LIBRARY_PATH}`
adds a colon and the old value only when there is one. An empty entry (a
trailing `:` when the variable was unset, or `::`) means "the current
directory" to the loader, the compiler and the shell: every program and
library is then looked for in whatever directory a command runs in, the
build's own included, which is never what is meant (the build cache watches
those places, at a cost on every compile). The same goes for
`PATH`, `LIBRARY_PATH` and `CPATH`: for a list built in front of an old
value, use `${LIBRARY_PATH:+$LIBRARY_PATH:}`.

cactup's tests run every machine's build setup (with the path variables
unset, empty and set) and fail on one that leaves such an entry. A setup
that does more than set variables, say one that creates a software
environment the first time it runs, stops early off its own machine; the
tests then check what it set before it stopped, so put its `export`s first,
and name it, with why, in the test's `STOPS_HERE` list
(`src/objcache/doctor.rs`), or the tests fail.

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
derived defaults when unset), plus twelve that configure cactup itself
rather than a job — `autoupdate`, `update-url` and `mdb-url` (see
[Updating cactup](../users/updating.html)), `build-cache`,
`build-cache-relocate`, `build-cache-size` and
`build-cache-preserve-exact-warnings` (see
[Building Configs](../users/building-configs.html)), and one per `[paths]`
key, `install-home`, `simulation-home`, `test-home`, `scratch-home` and
`build-cache-home`, an absolute path that wins over the machine's value;
those twelve are never recorded with a simulation as knobs (the paths a job
needs are recorded as the paths they are). Users can also create
**custom knobs**
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
- Queue names in `[submitscripts]`, `[runscripts]` and `[buildsubmitscripts]` must exist in `[queues.*]`
- `build-universes` lists (on queues and on script routes) must name declared universes and may not be empty; omit the key for "all universes"
- Every key in a script table names a file in the matching directory (`submitscripts/<name>.sh` or `.py`, likewise `runscripts/` and `buildsubmitscripts/`); there are no reserved keys, so a script may be named `default`
- A script route has no machine-wide universe fallback: a route that should run inside a universe says so with its own `universe = "..."`
- Test-marked scripts should be present in both `[submitscripts]` and `[runscripts]` (used by `cactup test run/submit`)
- `[buildsubmitscripts]` is the one script table that's allowed to be entirely absent — a machine that never queues a build simply omits the table (and the `buildsubmitscripts/` directory) rather than needing an empty one
- `[build].default-action = "submit"` only works once a `[buildsubmitscripts]` route and `[scheduler].submit` are both declared; without either, `cactup build submit` errors naming what's missing (`cactup build run` always works regardless)
- `[scheduler].blocking-submit` is optional and needs no matching table of its own — a machine that omits it still supports `cactup build submit --block`, just via emulated polling instead of a native blocking submit — but when present, its output must parse with the same `submit-pattern` as `submit`

## Next steps

- [Optionlists](optionlists.html) — define compiler settings
- [Scripts & Variables](scripts-and-variables.html) — write submit/run scripts
- [Machine Discovery](machine-discovery.html) — write discover.py
- [Porting a Cluster](porting-a-cluster.html) — complete example
