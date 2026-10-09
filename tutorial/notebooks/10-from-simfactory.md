---
jupytext:
  text_representation:
    extension: .md
    format_name: myst
kernelspec:
  display_name: Python 3
  language: python
  name: python3
---

# 10. From SimFactory to cactup

In this notebook you will

- translate SimFactory commands and options into cactup's, including the
  few that look alike but mean something different,
- read a SimFactory machine file next to the cactup entry made from it,
- find where your SimFactory settings went: `defs.local.ini`, and the
  variables in scripts and parameter files,
- see what cactup doesn't do, so nothing surprises you on your cluster.

*Time: about 25 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 10
```

## SimFactory is still there

Every installation of an Einstein Toolkit release contains SimFactory, where
it always was: the release's thornlist checks it out, and cactup installs
whatever the thornlist names. cactup itself never runs it and never reads
its files, so while you move over, `sim` keeps working in the same tree.
Give SimFactory's configs names of their own, though: `sim build` refuses a
config cactup built, which has no SimFactory `properties.ini`.

```{code-cell} ipython3
%%shell
ls ~/Cactus/simfactory
ls ~/Cactus/simfactory/mdb/machines | head -5
```

## Commands

| SimFactory | cactup |
|---|---|
| `sim setup` | nothing to set up: an installation finds or makes its machine entry; settings are knobs (below) |
| `sim checkout`, `GetComponents` | `cactup install`, `cactup inst refetch` (notebooks 1 and 3) |
| `sim build NAME --thornlist=T` | `cactup build NAME --thornlist T` |
| `sim build` | `cactup build NAME` (see below) |
| `sim build --optionlist=F` | `cactup build --optionlist F`, or a machine's variant with `--variant` (notebook 6b) |
| `sim list-configurations` | `cactup config list` |
| `sim create N --parfile=P` | `cactup sim create N P` |
| `sim create-submit N --parfile=P`, `sim submit N` | `cactup sim submit N P`, `cactup sim submit N` |
| `sim create-run`, `sim run` | `cactup sim run N P` (in the foreground, where you are) |
| `sim run-debug` | `cactup sim run N P --debug` (Cactus under gdb, where the machine's run script supports it, as with `run-debug`) |
| `sim list-simulations` | `cactup sim list` |
| `sim show-output N` (`--follow`) | `cactup sim log N` (`-f`; notebook 7) |
| `sim get-output-dir N` | `cactup sim show N --output-dir` |
| `sim stop N` | `cactup sim stop N` |
| `sim cleanup N` | `cactup sim clean N` |
| `sim purge N` | `cactup sim delete N` |
| `sim create N --testsuite` | `cactup test submit` (notebook 9) |
| `sim list-machines` | `cactup machine list` |
| `sim whoami` | `cactup machine show` |
| `sim print-mdb-entry M` | `cactup machine show M` |
| `--machine M` | `--machine M` (skips matching the host name) |
| `--walltime`, `--queue`, `--allocation` | `-w`, `-q`, `-a` |

The options that shape a run differ more than their names suggest:

- **`--procs` counts threads; `-T` counts processes.** SimFactory's
  `--procs=8 --num-threads=2` is four MPI processes of two threads each:
  `-T 4 -c 2` in cactup. SimFactory's `--num-threads` is cactup's `-c`, and
  `--ppn-used` is `-t` (processes per node, notebook 5) times `-c`.
- **Nodes.** SimFactory works out the nodes from `--procs` (one, unless you
  say) and the machine's `ppn`. cactup asks you for nodes instead: `-n` is
  the number of nodes (one unless you say), and the processes fill them. On
  QueenBee4, LONI's cluster at LSU that the next section looks at, the CPU
  partition `workq` has 64 cores a node and eight threads per process
  unless you say: `-q workq -n 2` is 16 processes of eight threads, and `-q
  workq -n 2 -c 1` is 128 processes of one. `-T` alone never adds nodes: ask
  for more processes than one node holds without `-n`, and the scheduler
  refuses the job.
- **Nothing carries over between submits.** When you submit a simulation
  again, SimFactory reuses the previous restart's processes, threads and
  walltime. cactup takes each `sim submit`'s own options, and without them
  fills a node and asks for the queue's longest walltime.
- **Chained jobs** work as they did: submit a simulation again while a
  restart is queued or running, and the new job waits for it; ask for a
  longer walltime than the queue allows, and the run becomes a chain of
  jobs (notebook 7).
- **`sim build` with no name** builds a config named `sim`. `cactup build`
  with no name rebuilds the active config, and a new config has to be named.
- **`sim build --debug`** turns optimization off; `cactup build --debug`
  adds Cactus's debug checks and keeps optimization on (notebook 4b).
  `cactup sim run --debug`, in the table, is something else: it asks the
  run script to start Cactus under gdb.
- **`sim stop`** stops every active restart of the simulation; `cactup sim
  stop` stops the active one, running or queued (notebook 7).
- **`--define`, `--replace`, `--substitute`** all become a knob: put
  `@KNOB(name)@` where the value goes in the parameter file, and submit with
  `-K name=value` (notebook 8). So `--replace Grid::xmax=10`, which rewrote
  that line of the file for you, becomes `Grid::xmax = @KNOB(xmax)@` in the
  file and `-K xmax=10` on the command line. Knob names are lowercase, with
  dashes, not underscores: a define `NUM_LEVELS` becomes `num-levels`.

## Machine entries

A SimFactory machine is one `.ini` file. Its submit script, run script and
optionlist are separate files, shared between machines, which the `.ini`
names. Here is QueenBee4's, as the release has it:

```{code-cell} ipython3
%show ~/Cactus/simfactory/mdb/machines/qbd.ini --lines 8:71
```

cactup's entry for QueenBee4 is a directory holding everything about the
machine, scripts and optionlists included. `machine show` sums it up:

```{code-cell} ipython3
%%shell
cactup machine show qbd
ls ~/.cactup/mdb/gen-1/qbd
cat ~/.cactup/mdb/gen-1/qbd/hostname.regexp
```

(The summary's first line names the directory behind `~/.cactup/mdb/gen-1`,
the link to the machine database cactup downloaded, as notebook 6a showed.
A `?` is a value the entry leaves unset for the whole machine: each GPU
queue gives its own GPU count, and a process gets one GPU unless you say.)

The entry describes QueenBee4 as it is today; the release's `.ini` is
older. It built inside a Singularity image, the entry builds natively with
CUDA, and values such as `memory` differ. Look at how the same needs are
met:

- **Building on the compute nodes.** QueenBee4 wants builds there, not on
  the login nodes. The `.ini` did it by wrapping `make` in `srun`, which
  held your terminal until `make` finished. cactup's entry says a build
  goes to the queue, and in what shape (with the `buildsubmitscripts` the
  summary lists). `cactup build` then submits it as a job and returns, like
  `sim submit`; `cactup build NAME --block` waits for it, `cactup build log
  NAME -f` follows its output, and `build list` and `build show` report it
  like any build:

```{code-cell} ipython3
%%shell
sed -n '/^\[build\]/,/^\[environment\]/p' ~/.cactup/mdb/gen-1/qbd/meta.toml | grep -v -E '^ *#|^\[environment'
```

- **Queues.** The `.ini` names one queue, `gpu2`, and its scripts branched
  on the queue's name. cactup's entry has a table per partition: the two
  GPU partitions with their GPU counts, and four CPU partitions. The queue
  picks the submit and run scripts (the `cpu` variants on the CPU
  partitions, as in notebook 6b). A config for the CPU partitions is built
  with `--variant cpu`, the CPU-only optionlist, and runs only on those
  four queues.
- **Threads.** The `.ini`'s `num-threads = 64` ran one process per node.
  cactup's default on the GPU partitions is one process per GPU, splitting
  the node's 64 cores: 32 threads each on `gpu2`, 16 on `gpu4`. On the CPU
  partitions it is eight processes a node, of eight threads each.
- **Scripts.** In place of `qbd.sub`, `qbd.run` and the shared optionlist
  `db-sing-nv.cfg`, the entry has files of its own. The submit script is a
  Python program (`submitscripts/default.py`), since the `.sub` computed
  values with `@( ... )@`, which cactup doesn't have (below).

How the keys moved, in general:

| `.ini` key | cactup |
|---|---|
| `name`, `nickname`, `location`, `description`, `webpage`, `status`, `hostname` | `[machine]`, the same names (`hostname` is information only: cactup doesn't use it to find the machine) |
| `aliaspattern` | the file `hostname.regexp` |
| `envsetup` | `[environment] env-setup` |
| `sourcebasedir`, `basedir` | `[paths] install-home`, `simulation-home` |
| `ppn`, `num-threads`, `memory`, `num-smt` | `[hardware] max-cpus-per-node`, `default-cpus-per-task`, `memory`, `threads-per-cpu` (a queue can override them) |
| `make`, `makejobs`, `enabled-thorns`, `disabled-thorns` | `[build]` |
| `submit`, `getstatus`, `stop`, the `*pattern` keys, `exechost` | `[scheduler]`, as `submit-pattern` and so on |
| `queue` | a `[queues.NAME]` table per queue, `default = true` on the default |
| `maxwalltime` | `max-walltime`: in a queue's table for that queue, or in `[scheduler]` for every queue that doesn't set its own |
| `submitscript`, `runscript`, `optionlist` | files in the entry's `submitscripts/`, `runscripts/`, `optionlists/`, chosen by `[variants]` (notebook 6b) |
| `cpu`, `cpufreq`, `flop/cycle`, `spn`, `mpn`, `nodes`, `min-ppn`, `max-num-threads`, `maxqueueslots` | dropped |
| `stdout`, `stderr`, `stdout-follow` | dropped: `cactup sim log` reads the files itself |
| `user`, `trampoline`, `rsynccmd`, `sshcmd`, and the other remote-access keys | dropped (see below) |

Three things work differently:

- **cactup refuses keys it doesn't know**, so a typo in `meta.toml` is an
  error, not a setting quietly ignored.
- **Finding the machine**: `hostname.regexp` is matched against this host's
  name and its short form, without a DNS lookup (SimFactory matched one
  name, made fully qualified by DNS). QueenBee4's matches `qbd2` as well as
  `qbd2.loni.org`. When nothing matches, `cactup install` makes a machine
  entry of your own for the host (notebook 6a), where SimFactory sent you
  to `sim setup`.
- **Moving your own `.ini` over** is by hand: cactup has no converter.
  `cactup machine create NAME --from-existing M` (notebook 6a) starts you
  from the closest existing entry, and fills in `[hardware]` from the host
  you run it on: on a cluster, that is the login node, so check it against
  the compute nodes. The documentation's "Porting a cluster" page goes
  through the rest. A SimFactory optionlist (`.cfg`) works as it is:
  `cactup build NAME --optionlist my-cluster.cfg`.

## Your settings: `defs.local.ini` becomes knobs

What `sim setup` asked for and wrote into `simfactory/etc/defs.local.ini`
lives in cactup's knobs (notebook 1), kept with cactup's other state in
`~/.cactup`, not inside a Cactus tree:

```{code-cell} ipython3
%%shell
cactup knob | sed -n 1,7p
```

| `defs.local.ini` | cactup |
|---|---|
| `user`, `email`, `allocation` | the knobs `user`, `email`, `allocation` (`-a` overrides the allocation for one submit); most machine entries, QueenBee4's included, give `email` to the scheduler for job mail, as SimFactory did |
| `sourcebasedir`, `basedir` | the machine entry's `[paths]`; for one installation, `cactup install --install-prefix`; for one simulation, `cactup sim create ... --sim-dir`; to change the defaults, an entry of your own (notebook 6a) |
| a per-machine section | knobs are the same on every machine; a machine entry of your own for what differs |
| (none) | `queue`: your default queue (`-q`); `mail`, `mail-type` (`-m`, `-M`): job mail, for machine entries whose scripts use them, as this tutorial's does |

## Scripts and parameter files

Submit scripts, run scripts and parameter files use `@NAME@` variables, as
in SimFactory. Most names are the same: `@SIMULATION_NAME@`,
`@RESTART_ID@`, `@RUNDIR@`, `@PARFILE@`, `@EXECUTABLE@`, `@CONFIGURATION@`,
`@NODES@`, `@WALLTIME@` and its parts, `@QUEUE@`, `@ALLOCATION@`, `@USER@`,
`@EMAIL@`, `@CHAINED_JOB_ID@`, `@SOURCEDIR@`, `@MACHINE@`, `@MEMORY@`,
`@SIMULATION_ID@`, `@HOSTNAME@`, `@SCRIPTFILE@`, `@RUNDEBUG@`,
`@DEBUGGER@`, and `@ENV(NAME)@`. The others:

| SimFactory | cactup |
|---|---|
| `@NUM_PROCS@` | `@TASKS@` |
| `@NUM_THREADS@` | `@CPUS_PER_TASK@` |
| `@NODE_PROCS@` | `@TASKS_PER_NODE@` |
| `@PPN@` | `@MAX_CPUS_PER_NODE@` (the machine's; there is no option for it) |
| `@NUM_SMT@` | `@THREADS_PER_CPU@` |
| `@PROCS@`, `@PPN_USED@` | none: compute them, `$(( @TASKS@ * @CPUS_PER_TASK@ ))` and `$(( @TASKS_PER_NODE@ * @CPUS_PER_TASK@ ))` in a shell script |
| `@BASEDIR@` | `@SIM_HOME@`, which is `simulation-home/INSTALLATION`; the simulation itself is at `@SIMULATION_DIR@`, `@SIM_HOME@/CONFIG/NAME` |
| `@SIMFACTORY@` | `@CACTUP@`: the exact cactup build that submitted the job (notebook 7) |
| `@SHORT_SIMULATION_NAME@` (`NAME-NNNN`) | `@SHORT_SIMULATION_NAME@` (`NAME-N`) |
| `@FROM_RESTART_COMMAND@`, `@SUBMITSCRIPT@` | none: the job's last line runs `@CACTUP@ sim run @SIMULATION_NAME@ --installation=@ALIAS@ --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ --restart-id=@RESTART_ID@` (`@ALIAS@` is the installation's name; notebook 5); copy it from an existing entry's submit script |
| `@CPUFREQ@` | none |
| `@( python expression )@` | none: write the script as a Python program (notebook 6b) |

Here is a parameter file as a SimFactory workflow would have it, with
SimFactory's variable names in its run title. cactup checks it when you
submit. (The `squeue` loops in these cells wait
for an earlier `sf` job, so the cells can be run again; the last one also
waits for this job before reading its output.)

```{code-cell} ipython3
%%shell --expect-fail
while squeue -h -n sf | grep -q .; do sleep 1; done
cp ~/Cactus/arrangements/CarpetX/WaveToyX/test/radiative.par ~/sf.par
echo 'Cactus::cctk_run_title = "@SIMULATION_NAME@: @NUM_PROCS@ processes, @NUM_THREADS@ threads each"' >> ~/sf.par
cactup sim submit sf ~/sf.par -T 2 -c 2 -w 00:05:00 --overwrite
```

SimFactory would have left an unknown variable in place, unreplaced, for
Cactus to trip over; cactup refuses it before the job is queued. It names
the first unknown variable it meets, in its own copy of your file (the
line number is the same in yours). It has made the simulation by then,
though, so `--overwrite` replaces it, here and below. With cactup's names:

```{code-cell} ipython3
%%shell
while squeue -h -n sf | grep -q .; do sleep 1; done
sed -i 's/@NUM_PROCS@/@TASKS@/; s/@NUM_THREADS@/@CPUS_PER_TASK@/' ~/sf.par
cactup sim submit sf ~/sf.par -T 2 -c 2 -w 00:05:00 --overwrite
while squeue -h -n sf | grep -q .; do sleep 1; done
grep run_title "$(cactup sim show sf --output-dir)/sf.par"
```

Other differences in how the variables are filled in:

- **Checked at submit, filled in once per restart**, when it starts, into
  the restart's copy (`output-NNNN/sf.par`); comments are left alone.
  SimFactory substituted when the simulation was made and again at each
  restart.
- **No automatic edits.** SimFactory set `TerminationTrigger::max_walltime`
  to the job's walltime by itself; with cactup you write
  `@CHECKPOINT_WALLTIME_SECONDS@` (or `_HOURS`) where you want it
  (notebook 7).
- **`.rpar` files** become `.py` parameter files: Python programs that
  cactup runs to write the parameter file (notebook 8).
- **Checkpoints stay where Cactus writes them.** SimFactory linked the
  previous restart's checkpoint files into each new restart's directory.
  cactup doesn't: point `IO::checkpoint_dir` and `IO::recover_dir` at a
  directory all the restarts share, such as `@SIMULATION_DIR@/checkpoints`
  (notebook 7).

## The simulation directory

The layout is SimFactory's: `output-0000`, `output-0001`, ..., with
`output-NNNN-active` pointing at the latest restart, `NAME.out` and `NAME.err`
in each, and `log.txt` beside them. What SimFactory kept in `SIMFACTORY/`
directories is in `.cactup/` ones:

```{code-cell} ipython3
%%shell
cd ~/.cactup/simulations/ET_2026_05_v0/tutorial/sf
ls -A . .cactup output-0000 output-0000/.cactup
```

| SimFactory | cactup |
|---|---|
| `BASEDIR/NAME` | `simulation-home/INSTALLATION/CONFIG/NAME` (or where `sim create --sim-dir` put it) |
| `SIMFACTORY/properties.ini`, `exe/`, `cfg/`, `par/` | `.cactup/simulation.toml`, `exe`, `cfg/`, `par/` |
| `output-NNNN/SIMFACTORY/properties.ini` | `output-NNNN/.cactup/restart.toml` |
| `output-NNNN/SIMFACTORY/SubmitScript`, `RunScript` | `output-NNNN/.cactup/submit-script`, `run-script` |

`output-NNNN/.cactup/ENVIRONMENT` is the environment the run started Cactus
with (notebook 6b), and `heartbeat` is a file the running job touches
regularly, which is how cactup tells a running job from a dead one.

So a script of yours that reads SimFactory's `properties.ini` files needs
rewriting for `restart.toml`, whose keys differ (`tasks`, `cpus`, `nodes`,
`walltime`, `job-id`, ...). cactup finds simulations through its own record
of them, not by looking through a directory, so a SimFactory simulation
directory isn't one cactup knows.

## What cactup doesn't do

- **Work on a remote machine from your laptop**: `sim login`, `sim
  execute`, `sim sync` and `--remote`. cactup runs on the cluster itself; it
  is one file to copy there.
- **Archive simulations** (`sim archive`), or start an **interactive** job
  (`sim interactive`).
- **Pick a restart or a job** for `sim stop` and `sim show-output`
  (`--restart-id`, `--job-id`). `cactup sim show N --output-dir` does take
  `--restart-id`.

What it does that SimFactory doesn't, you have seen: releases and
installations, build provenance and deltas, script variants and universes,
Python scripts and parameter files, knobs, self-update, and a machine
database that updates itself.

## Cleaning up

```{code-cell} ipython3
%%shell
while squeue -h -n sf | grep -q .; do sleep 1; done
cactup sim delete sf
rm -f ~/sf.par
```

## On your own cluster

Install cactup there with the installer from notebook 1:

```sh
curl --proto '=https' --tlsv1.2 -sSf https://max-morris.github.io/Cactup/cactup-init.sh | sh
```

then run `cactup machine show`. If it names your cluster, `cactup install`
a release and carry on as here. If it doesn't, `cactup install` makes a
machine entry of your own (notebook 6a), and "Porting a cluster" below
takes it from there.

## Where this is documented

- [Getting started](https://max-morris.github.io/Cactup/users/getting-started.html)
- [Running simulations](https://max-morris.github.io/Cactup/users/running-simulations.html)
- [Building configurations](https://max-morris.github.io/Cactup/users/building-configs.html)
- [Scripts and variables](https://max-morris.github.io/Cactup/authors/scripts-and-variables.html)
- [Porting a cluster](https://max-morris.github.io/Cactup/authors/porting-a-cluster.html)
- [The machine entry, `meta.toml`](https://max-morris.github.io/Cactup/authors/meta-toml.html)
- [Optionlists](https://max-morris.github.io/Cactup/authors/optionlists.html)

That is the end of the tutorial. Every notebook can be run again from the
top: its first cell catches up whatever it needs. In the Terminal,
`cactup-tutorial-reset --notebook N` puts back notebook N's file as it was
shipped, if you have changed it, and `cactup-tutorial-reset` on its own
(it asks first) returns your home to where you started.
