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

# 7. Long runs: chained jobs, checkpoints and restarts

In this notebook you will

- build a config that can checkpoint,
- run a simulation longer than one job may run, as a chain of jobs, each
  carrying on from the last one's checkpoint,
- follow it while it runs, and read what it did,
- stop a run so that it can carry on later,
- see what is fixed when a simulation is submitted, and what happens to a
  run when its job is interrupted.

*Time: about 30 minutes.*

```{code-cell} ipython3
%%shell
cactup-tutorial-catch-up 7
```

## A run longer than a job

A cluster limits how long a job may run: its queue's *walltime*. A
simulation that needs longer runs as a chain of jobs. Each one stops before
its time is up, writes a *checkpoint* (the simulation's complete state), and
the next one *recovers* from it and carries on.

cactup does the chaining: given a walltime (`-w`) longer than the queue
allows, it submits a chain of jobs at once, each waiting for the one before.
Cactus does the rest, as the parameter file tells it: when to stop, where to
write the checkpoint, and where to look for one when it starts. Each job is
a *restart* of the simulation, `output-0000`, `output-0001` and so on.

This machine's `short` queue allows three minutes per job, and each job here
computes for one minute before it checkpoints. The run needs two to six
minutes of computing, depending on how busy the machine is, so it takes two
to seven jobs. It asks for 24 minutes, eight jobs, to leave room: a job that
starts after the run has finished recovers, finds nothing left to do, and
ends at once. A real run is sized the same way, with room to spare.

## A config that can checkpoint

CarpetX writes checkpoints through a file format library (Silo or openPMD),
and it can only if its config was built with one. The `tutorial` config
wasn't. `tutorial-ckpt.th` is the tutorial thornlist plus Silo, the HDF5
library Silo stores its data in, and TerminationTrigger, which lets cactup
ask a run to stop (more on that below). Leaving out comments, blank lines and
the `!` lines (which say where each repository comes from), the two lists
differ in exactly those three thorns:

```{code-cell} ipython3
%%shell --expect-fail
diff <(grep -v -e '^#' -e '^!' -e '^$' /opt/cactup-tutorial/thornlists/tutorial.th) <(grep -v -e '^#' -e '^!' -e '^$' /opt/cactup-tutorial/thornlists/tutorial-ckpt.th)
```

(`diff` reports a difference with exit status 1, so the cell is marked as
expected to fail.) Build a config from it, `tutorial-ckpt`. As in notebook
5, the first command checks that the stock installation's sources are as
fetched, because this build was prepared ahead of time from them:

```{code-cell} ipython3
%%shell
cactup-tutorial-sources-clean && cactup build tutorial-ckpt --thornlist /opt/cactup-tutorial/thornlists/tutorial-ckpt.th
```

## The parameter file

A standing wave in a periodic box, evolved by WaveToyX. The comments explain the parts that matter
here; `@...@` are cactup's variables, which it fills in for each restart
(notebook 8 is about them).

```{code-cell} ipython3
%%file ~/wave_ckpt.par
# A standing wave in a periodic box, evolved by WaveToyX. The run stops
# itself before its job's walltime runs out, writes a checkpoint, and the
# next job of the chain recovers from it and carries on.

$cells = 64

ActiveThorns = "
  CarpetX
  IOUtil
  ODESolvers
  TerminationTrigger
  WaveToyX
"

Cactus::presync_mode = "mixed-error"
# TerminationTrigger keeps its values on process 0 only, which CarpetX's
# checks for unset values would take for an error on the others.
CarpetX::poison_undefined_values = no

CarpetX::ncells_x = $cells
CarpetX::ncells_y = $cells
CarpetX::ncells_z = $cells
CarpetX::periodic   = yes
CarpetX::periodic_x = yes
CarpetX::periodic_y = yes
CarpetX::periodic_z = yes

WaveToyX::initial_condition = "standing wave"
ODESolvers::method = "RK3"

# Stop after the last iteration, or once this job's checkpoint walltime
# (its walltime minus the checkpoint buffer) has passed, whichever comes
# first. max_runtime is in minutes.
Cactus::terminate       = "any"
Cactus::cctk_itlast     = 9600
Cactus::max_runtime     = @CHECKPOINT_WALLTIME_SECONDS@ / 60.0

# Checkpoint when the run stops, into one directory that every restart of
# the simulation shares, and recover from the newest checkpoint there, if
# there is one.
CarpetX::checkpoint_method  = "silo"
CarpetX::recover_method     = "silo"
IO::checkpoint_dir          = "@SIMULATION_DIR@/checkpoints"
IO::recover_dir             = "@SIMULATION_DIR@/checkpoints"
IO::checkpoint_on_terminate = yes
IO::recover                 = "autoprobe"

IO::out_dir   = $parfile
IO::out_every = 32
CarpetX::out_norm_vars  = "WaveToyX::state WaveToyX::error"
CarpetX::out_norm_every = 8
CarpetX::out_metadata    = no
CarpetX::out_performance = no

# `cactup sim stop` asks a run to stop by writing 1 into the restart's
# TERMINATE file, which TerminationTrigger creates and watches.
TerminationTrigger::create_termination_file = yes
TerminationTrigger::termination_from_file   = yes
TerminationTrigger::termination_file        = "@RUNDIR@/TERMINATE"
```

## Submitting a chain

24 minutes on `short`, four processes of one thread each. (One thread per
process, because TerminationTrigger, the thorn `sim stop` relies on below,
isn't safe to run on several threads under CarpetX: now and then it aborts
a run as it starts.)

```{code-cell} ipython3
%%shell
while squeue -h -n chain | grep -q .; do sleep 1; done
cactup sim submit chain ~/wave_ckpt.par --config tutorial-ckpt -T 4 -c 1 -q short -w 00:24:00 --checkpt-buffer 00:02:00 --overwrite
squeue
```

All eight jobs are in the queue now: the first waiting to start (or already
running), the others each for the one before (`Dependency`).

`--checkpt-buffer` is how long before its walltime a job should stop, to
leave time for the checkpoint: `@CHECKPOINT_WALLTIME_SECONDS@` is the
walltime minus the buffer, here one minute of each three-minute job (a
real run would compute for most of its walltime, and leave a few minutes for
the checkpoint). Without it, cactup takes at least ten minutes, which is
more than a whole job on `short`: the checkpoint walltime would be 0, and
every job would stop as soon as it started. cactup doesn't warn about that,
so on a short queue, always pass the buffer.

## Watching it

`cactup sim show` lists the restarts and what state each is in:

```{code-cell} ipython3
%%shell
cactup sim show chain
```

`cactup sim log NAME` prints the end of the running restart's output and
error streams. Its follow modes keep printing as the run goes on, until you
press Ctrl-C (or stop the cell): `-o` follows the output, `-e` the errors,
and `-f` both, side by side, in a full-screen view meant for a terminal
(*File > New > Terminal*; `q` quits). Wait for the first job to start, then
follow its output for twenty seconds (`--timeout` stops the cell then):

```{code-cell} ipython3
%%shell
until squeue -h -n chain -t R | grep -q . || ! squeue -h -n chain | grep -q .; do sleep 1; done
sleep 5
```

```{code-cell} ipython3
%%shell --timeout 20
cactup sim log chain -o
```

Cactus reports every iteration, so the output runs fast: the box shows the
newest lines, and the `OutputGH: iteration N` lines show the run's progress.
A follow stays with the restart it started on: when the chain moves on to
`output-0001`, start it again.

The whole run takes about three minutes on a quiet machine, longer on a busy
one. The next cell waits for all eight jobs to finish (the ones after the
run's end take a few seconds each):

```{code-cell} ipython3
%%shell
while squeue -h -n chain | grep -q .; do sleep 5; done
cactup sim show chain
```

Only the latest restart is the active one; the earlier ones show INACTIVE.
The simulation's `log.txt` is cactup's record of everything that happened
to it:

```{code-cell} ipython3
%%shell
cat ~/.cactup/simulations/ET_2026_05_v0/tutorial-ckpt/chain/log.txt
```

And Cactus's own account, from each restart's output (each line starts with
the restart it came from): each job checkpointed when its time was up, and
the next recovered from that same iteration, until the last iteration,
9600; a job that starts after that recovers at 9600 and ends at once. The
checkpoints themselves are in `checkpoints/`, one for each
iteration a job stopped at (the jobs that ended at 9600 share one).

```{code-cell} ipython3
%%shell
(cd ~/.cactup/simulations/ET_2026_05_v0/tutorial-ckpt/chain &&
 grep -a -E '^INFO \(CarpetX\): (Checkpointing before terminating|RecoverGH)' output-[0-9][0-9][0-9][0-9]/chain.out &&
 ls checkpoints)
```

The error of the numerical solution, from the restarts that computed
something, is one continuous curve. `cactup sim show NAME --output-dir`
says where a restart's output is (`--restart-id N` picks the restart);
`read_tsv` is notebook 2's.

```{code-cell} ipython3
import subprocess
from pathlib import Path

import matplotlib.pyplot as plt
import pandas as pd


def read_tsv(path):
    """A CarpetX TSV file, its columns named from its header line."""
    with open(path) as f:
        header = f.readline().lstrip("# ").split("\t")
    names = [column.split(":", 1)[1].strip() for column in header]
    return pd.read_csv(path, sep="\t", comment="#", names=names)


def output_dir(sim, restart):
    """A restart's output directory, as cactup reports it."""
    cmd = ["cactup", "sim", "show", sim, "--output-dir", "--restart-id", str(restart)]
    return Path(subprocess.run(cmd, capture_output=True, text=True, check=True).stdout.strip())


fig, ax = plt.subplots(figsize=(8, 4))
for restart in range(8):
    tsv = output_dir("chain", restart) / "wave_ckpt" / "norms" / "wavetoyx-error.tsv"
    if tsv.exists():
        err = read_tsv(tsv)
        if len(err) > 1:  # a restart that only recovered and ended has nothing to plot
            ax.plot(err["time"], err["wavetoyx::u_err.L2norm"], label=f"output-{restart:04d}")
ax.set_xlabel("t")
ax.set_ylabel("L2 norm of the error in u")
ax.set_title("One run, several jobs")
ax.legend()
plt.show()
```

## Stopping a run

`cactup sim stop NAME` stops a simulation's running job. If the restart has
a `TERMINATE` file, as this parameter file has TerminationTrigger make, cactup
writes `1` into it, and the run stops at its next iteration, checkpoints and
ends: a later restart can carry on from there. Without one, cactup cancels
the job, and whatever it computed since its last checkpoint is lost.

On a chain, `sim stop` stops only the running job, and the next one, already
in the queue, then starts (more below), so this is shown on a single job:
the same parameter file, on `debug`, stopped once it is past iteration 1000
(the cell waits for Cactus to say so in its output):

```{code-cell} ipython3
%%shell
while squeue -h -n stopme | grep -q .; do sleep 1; done
cactup sim submit stopme ~/wave_ckpt.par --config tutorial-ckpt -T 4 -c 1 -w 00:10:00 --checkpt-buffer 00:01:00 --overwrite
out=~/.cactup/simulations/ET_2026_05_v0/tutorial-ckpt/stopme/output-0000/stopme.out
until grep -a -q 'OutputGH: iteration [0-9]\{4\}' $out 2>/dev/null || ! squeue -h -n stopme | grep -q .; do sleep 1; done
cactup sim stop stopme
while squeue -h -n stopme | grep -q .; do sleep 1; done
```

What the run said when it got the request:

```{code-cell} ipython3
%%shell
grep -a -h -o -E 'Found termination signal in termination file|Checkpointing before terminating at iteration [0-9]+' ~/.cactup/simulations/ET_2026_05_v0/tutorial-ckpt/stopme/output-0000/stopme.out | awk '!seen[$0]++'
```

On a chain, the jobs still in the queue start one after another once the
running one stops, each carrying on from the checkpoint. To stop the chain,
cancel those first (`scancel` with their ids, which `squeue` lists), then
`cactup sim stop NAME`. `cactup sim delete -f NAME` cancels every job too,
but deletes the simulation and its checkpoints with it.

## Fixed at submit time

A submitted job doesn't change when you change things after submitting it:

- its batch script runs the cactup build that submitted it, by its version
  (`cactup-0ea3398` and the like), even after a `cactup update`;
- each restart records the variables and knobs it was submitted with in its
  `restart.toml`;
- a simulation keeps its own copy of the executable (`.cactup/exe`), made
  when the simulation was created: rebuilding the config doesn't change it,
  not even for a later restart of the same simulation (notebook 5 relied on
  that).

```{code-cell} ipython3
%%shell
(cd ~/.cactup/simulations/ET_2026_05_v0/tutorial-ckpt/chain &&
 tail -n 1 output-0001/.cactup/submit-script &&
 grep -E '^(CACTUP|EXECUTABLE|CHECKPOINT_WALLTIME_SECONDS) ' output-0001/.cactup/restart.toml)
```

The submit script's last line names the cactup build (`cactup-0ea3398`);
`restart.toml` has the checkpoint walltime this job was given (60 seconds)
and the simulation's own copy of the executable.

## When a job is interrupted

Say this container stops while a job runs: over lunch, the tutorial server
stops containers nobody is using. SLURM then *requeues* the job, and it
runs again from the start when the container is back. A run that recovers
from the newest checkpoint it finds loses only the work since that
checkpoint. This one checkpoints only when it stops, so it would lose the
whole job's work; a long run also checkpoints as it goes
(`IO::checkpoint_every_walltime_hours`, say).

cactup's restarts are not SLURM's requeues: a requeued job runs the same
restart (`output-0000`) again. `sim show` shows it queued, then running,
on that restart. Its output files keep the interrupted attempt and the new
one, one after the other: the error stream counts them (`Times SLURM
requeued this job: 0`, then `1`). And `log.txt` records a second
`compute-node run of output-0000`.

## Cleaning up

```{code-cell} ipython3
%%shell
cactup sim delete chain
cactup sim delete stopme
rm -f ~/wave_ckpt.par
```

## Where this is documented

- [Running simulations](https://max-morris.github.io/Cactup/users/running-simulations.html)
  (walltime and chaining, `--checkpt-buffer`, stopping)
- [Monitoring](https://max-morris.github.io/Cactup/users/monitoring.html)
  (`sim show`, `sim log` and its follow modes)
- [Submit and run scripts](https://max-morris.github.io/Cactup/authors/scripts-and-variables.html)
  (the checkpoint walltime variables)

Next: **notebook 8**, parameter files: cactup's variables in them,
parameter files written by Python, and a parameter sweep.
