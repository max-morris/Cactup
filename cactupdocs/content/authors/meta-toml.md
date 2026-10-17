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
mdb-generation = 3
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
unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'
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
submit = "exec setsid nohup @SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & echo $!"
blocking-submit = "setsid @SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & pid=$!; echo $pid; wait $pid"
allocation-env = ""
get-status = "ps @JOB_ID@"
job-ids = "pids"
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
unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'
stop = "scancel @JOB_ID@"
submit-pattern = "Submitted batch job ([0-9]+)"
status-pattern = "@JOB_ID@ "
queued-pattern = " PD "
running-pattern = " R "
```

`get-status-many` is optional: one call instead of one per job. It must list every
live job of `@USER@`, one per line, with the job id as the first field; the rest of
the line is classified by the same `status`/`queued`/`running`/`holding` patterns
used for `get-status`. A job id absent from the listing is treated as not queued
(a listing says nothing of the jobs it leaves out, so `unknown-job-pattern`
plays no part there). A listing command that exits non-zero has failed:
cactup then asks job by job with `get-status`, so the listing must exit 0
when it ran, also when it lists nothing. The listing serves `sim list`, `sim
show`, `test list`, `test show`, `build list`, `build show` and `sim
delete`; every other command asks job by job with `get-status`.

`unknown-job-pattern` (optional) is a regular expression matching a line of
`get-status`'s output, stdout or stderr, that says the scheduler does not
know the job, `@JOB_ID@` substituted as in the other patterns. For Slurm's
`squeue -j` that is `slurm_load_jobs error: Invalid job id specified`; for
Torque's `qstat`, `Unknown Job Id`; for PBS Pro's, `Unknown Job Id|Job has
finished` (the second for a finished job, when job history is on).

With it set, cactup tells a job the scheduler has forgotten from a status
query that failed: output with no line for the job (no line matching
`status-pattern`) means the job is gone only if a line matches
`unknown-job-pattern`. Otherwise the query failed (the controller timed out,
say). For a run that has not recorded its end, a command that acts on the
answer refuses, printing the query, both patterns and what it printed: `sim
delete`, `sim stop`'s graceful wait (a forced `sim stop -f` asks nothing),
`test stop`, `test delete`, `build stop`, and the checks before a new `sim
submit`/`sim run`, `test run`/`test submit` and build; `sim list`, `test
list`, `build list` and their `show`s print the state as `UNKNOWN`, and
`build prune` keeps the attempt. For a run that has recorded its end (a
finished simulation restart, test run or build), asking is only the check
for a job the scheduler put back in the queue, made for 14 days after the
end (later, the job id may name an unrelated job, and the record is taken
as ended without asking), so a failed query is taken as the job ended, with
a one-line note, and nothing is held up. Without the key, no line for the
job means the job is gone, as before: a query that failed reads as a job no
longer in the queue. Check the pattern against what the command really
prints for a job the scheduler no longer knows (an old job id will do): one
that never matches makes every job the scheduler has forgotten look like a
failed query wherever cactup asks job by job (`sim stop`'s wait, `test
stop` and `delete`, `build stop`, the checks before a new run or build),
which then refuse; where the listing answers instead (`sim list`, `sim show`, `test list`,
`test show`, `build list`, `build show`, `sim delete`), such a job is just
not listed, and so gone.

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
# PBS Pro with job history on: 'Unknown Job Id|Job has finished'
unknown-job-pattern = 'Unknown Job Id'
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
submit = "exec setsid nohup @SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & echo $!"
blocking-submit = "setsid @SCRIPTFILE@ < /dev/null > @STDOUT_FILE@ 2> @STDERR_FILE@ & pid=$!; echo $pid; wait $pid"
allocation-env = ""
get-status = "ps @JOB_ID@"
job-ids = "pids"
stop = "pkill -g $(ps -o pgid= -p @JOB_ID@)"
submit-pattern = "(.*)"
status-pattern = "^ *@JOB_ID@ "
queued-pattern = "$^"
running-pattern = "^"
```

Note the shape change from `submit`: no `exec`, no `nohup` — the shell has
to survive to reach `wait` — and the job id is echoed *before* the wait so
it parses immediately instead of only once the job is already done.

Set `job-ids = "pids"` on such a machine (the default is `"scheduler"`): it
declares that a job id is the pid of a process on the host the job was
submitted from, so the scheduler is that host's own processes. A pid means
nothing on another host and is reused once its process is gone, so cactup
records with each job id the host it was submitted from and the start time
of the process with that pid, read as soon as the id is known. Later it runs
`get-status` or `stop` for the job only on that host, and only while the
process with that pid is still the one that started then; otherwise the job
is taken as gone, and nothing is run. A job recorded on another host is
never taken as gone while its run's lock is held: `stop` says it runs on that
host and to stop it there, and `delete` refuses; once the lock is let go (or
goes stale) it is taken as ended. A job recorded by an older cactup,
without its host and start time, is asked about only where a process with
its pid runs on this host and started no later than the record was made,
and cactup never stops it (it may be another process): stop it yourself if
it is the job. A run that has recorded its end is not asked about again on
such a machine, where nothing requeues a job.

The `submit` (and `blocking-submit`) of such a machine must start each job
in a process group of its own, with `setsid` as above: `stop` stops a whole
group (`pkill -g`), so cactup runs it only for a job whose process leads its
own group, and refuses otherwise (a job sharing the submitting shell's group
would take the shell and its other jobs with it), naming the pid to stop by
hand. `setsid` keeps the backgrounded pid: it starts a new session in the
same process when that process does not lead a group already, as a
background job of `sh -c` does not.

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
unset, empty and set) and fail on one that leaves such an entry, or that
stops before its end. A setup only sets the environment, so it runs through
anywhere; anything that makes or fetches what a phase needs (a software
environment created the first time, a file under `$HOME`) belongs in a
provisioning key instead (below), which neither these tests nor
`cactup cache doctor` run. A setup that sources a module system's init file
the test host has is not run there: the test says it is not checked on that
host, and CI, which lacks the file, checks it (where the environment
variable `CI` is set to anything but empty, `0` or `false`, case and
surrounding blanks ignored, such a skip
fails the test).

`env-setup` runs before every build, submit and run. Each phase can add its
own, appended after `env-setup`:

```toml
[environment]
env-setup        = "module load gcc/11 openmpi/4"   # every phase, scheduler commands too
build-env-setup  = "module load cmake/3.27"         # the build script only
submit-env-setup = "module load slurm"              # submit and build-submit scripts,
                                                    # and the scheduler commands
run-env-setup    = "..."                            # the run script only
```

cactup's own scheduler commands run after the submit setup, with no
provisioning: `submit` and `blocking-submit` after the submit universe's
(its `env-setup` and `submit-env-setup` where it sets them, the machine's
where not), and `get-status`, `get-status-many`, `stop` and `exec-host`
after the machine's own `[environment]` `env-setup` and `submit-env-setup`.

### Provisioning

A setup describes the environment; a **provisioning** step makes or fetches
what a phase needs, once, on the machine itself: a conda environment, a stub
module under `$HOME`. `provision` runs wherever `env-setup` does in a script
(the build script, the submit script, the run script); `build-provision`,
`submit-provision` and `run-provision` in their phase only. Four things run
the setup without provisioning: cactup's scheduler commands (above), the
`make <config>-realclean` script a build adds when the Cactus flesh has moved
since the build was prepared (the build script that follows provisions),
`cactup cache doctor`, which runs a build attempt's setup to check what it
sets, and cactup's own tests of the MDB's build setups (above).
For a queued job, `provision` runs twice, as `env-setup` does: once in the
submit script (the job's script, where the scheduler starts it) and once in
the run or build script that job runs. In each phase the order is: the setup
(`env-setup`, then the phase's `*-env-setup`), then `provision`, then the
phase's own `*-provision`, so a provisioning step can use what the setup
loaded (a module that provides `curl`).

```toml
[environment]
env-setup = """
export PATH=/work/$USER/deps/env/bin:$PATH
"""
build-provision = """
if [ ! -d /work/$USER/deps/env ]; then
    fetch-and-create /work/$USER/deps/env
fi
"""
```

Each provisioning step runs in a subshell of its own, under `set -e`:

- **Nothing it sets reaches the phase.** What it exports, the directory it
  changes to, a function it defines: all lost when it ends. Put what the
  phase needs to see (`PATH`, `PERL5LIB`) in the setup, which may name
  directories the provisioning step creates.
- **The setup must not source or run what provisioning creates.** The setup
  runs first: on a host where a Python virtual environment or a conda
  environment does not exist yet, `. ~/venv/bin/activate` (or a conda
  `activate`) in the setup is fatal before provisioning ever makes it, and
  the phase never recovers. Write the exports that the activation would
  make instead (`export PATH=~/venv/bin:$PATH`, as rostam's `arm-conda`
  does for its conda environment).
- **A failure stops the phase, as under `set -e`.** A command that fails
  ends the build, the submit script or the run script with its status,
  whether or not the script itself runs `set -e`, and so does `exit N`
  with a nonzero `N`. As under `set -e`, a false test in `a && b` is not a
  failure, also on the step's last line (`[ -e "$f" ] || make-it` and
  `[ ! -e "$f" ] && make-it` both do what they say). `exit 0` ends the step
  early and the phase goes on.
- **A universe inherits it.** A universe that sets no provisioning key of
  its own runs the machine's: in a build or run script inside its wrapper
  (a container) when it has one, since cactup starts those scripts through
  the universe, where the tools the step uses (`python3`, `curl`) may be
  missing; in a submit script wherever the scheduler starts that script.
  Set the key empty in the universe (`provision = ""`,
  `build-provision = ""`, …) to turn the machine's step off there.
- **Unlike a failing setup line in a submit or run script, a failing step
  is fatal there.** The setup is put at the top of those scripts, ahead of
  any `set -e` of their own, so a failing setup line goes on (but for `.`
  of a missing file, which ends a script `/bin/sh` runs, though not one
  `bash` runs); a failing provisioning step stops the script, in any
  shell. (The build script runs its setup under `set -e` too.)
- It runs every time its phase does, so make it cheap when there is nothing
  to do (check first, as above), and safe when two builds run it at once
  (a lock, if it must).

Provisioning is not run before cactup's scheduler commands, by
`cactup cache doctor`, or by the MDB's tests, and it is not part of what the
build cache keys a compile by: it cannot change the environment a compile
sees. In a `.py` script, `ENV_SETUP` already carries the steps after the
setup, so printing it provisions.

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

Universes can override environment setup and provisioning key-by-key (each
`env-setup`, `*-env-setup`, `provision` or `*-provision` key a universe sets
replaces the machine's; unset keys inherit):

```toml
[universes.intel]
build-env-setup = """
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
- `[scheduler].job-ids` takes only `"scheduler"` or `"pids"`, and `[scheduler].unknown-job-pattern` must be a valid regular expression (with `@JOB_ID@` put in)
- `env-build-setup`, `env-submit-setup` and `env-run-setup` are generation-2 names: they are unknown keys now (`build-env-setup`, `submit-env-setup`, `run-env-setup`); see [MDB Generations](mdb-generations.html)
- `[scheduler].blocking-submit` is optional and needs no matching table of its own — a machine that omits it still supports `cactup build submit --block`, just via emulated polling instead of a native blocking submit — but when present, its output must parse with the same `submit-pattern` as `submit`

## Next steps

- [Optionlists](optionlists.html) — define compiler settings
- [Scripts & Variables](scripts-and-variables.html) — write submit/run scripts
- [Machine Discovery](machine-discovery.html) — write discover.py
- [Porting a Cluster](porting-a-cluster.html) — complete example
