# Sizing the tutorial machine

Measured 2026-10-07 and 08, on the image built from `cactup.pin` 0ea33983,
notebooks 1 to 9 (notebook 10 is mostly prose and adds nothing measurable).
The machine is provisioned on site, so the requirements are written as a
hardware and VM specification, not as a cloud instance type.

## For the sysadmin

One VM (or bare-metal host) runs JupyterHub and one Docker container per
attendee. For **N** simultaneous attendees it needs:

| | Per attendee | Fixed | 10 | 20 | 30 | 50 |
|---|---|---|---|---|---|---|
| vCPUs | 3 | 2 | 32 | 62 | 92 | 152 |
| Memory | 2.5 GB | 16 GB | 48 GB | 72 GB | 96–128 GB | 160 GB |
| Disk space | 15 GB | 40 GB | 200 GB | 350 GB | 500 GB | 800 GB |
| Disk write throughput, sustained, many writers | 38 MB/s | | 380 MB/s | 760 MB/s | 1.1 GB/s | 1.9 GB/s |

- **The disk is the requirement most likely to be missed.** Use local NVMe
  (or NVMe passed through to the VM), not network-backed storage (Ceph,
  NFS, iSCSI), spinning disks, or SATA SSDs. Docker's storage
  (`/var/lib/docker`, which holds the attendees' home volumes) must be on it.
  Check it before the image exists with the `fio` command under
  "Checking the provisioned machine": fio's reported `WRITE: bw=` should be
  at least the throughput row above.
- **vCPUs should be real**, not overcommitted on the hypervisor. The
  measurements ran on physical cores without SMT. If the VM's vCPUs are
  hyperthreads, plan on 4 per attendee instead of 3, or run the CPU check
  below.
- **Memory**: more than the table's minimum is not wasted. It lets the
  kernel buffer the burst of writes when everyone builds at once (see the
  disk section). 128 GB for 30 attendees is the comfortable choice.
- **Software**: Linux with cgroup v2; Docker Engine whose containers can be
  given a cpuset (rootful Docker, or rootless with the `cpuset` controller
  delegated to the user's systemd slice).
- **Network**: inbound ports 443 and 80 (HTTPS, and the redirect to it), or
  only the site proxy's way in if the site puts its own proxy in front. For
  setting up, outbound access to the package repositories, quay.io, Docker
  Hub and PyPI (the hub's and Caddy's images); with Let's Encrypt
  certificates, outbound access to Let's Encrypt from then on. During the
  workshop the tutorial needs nothing else outbound: the attendees'
  containers have no route out at all, and the 18.5 GB tutorial image is
  copied onto the machine once beforehand (README, "Deploying").
- A GPU is optional: notebook 4b shows a GPU queue only when the VM passes
  an NVIDIA GPU through, and never runs a GPU job otherwise.

A machine below the CPU or disk figures still runs the tutorial; what
degrades is described in "What happens below these figures".

## How it was measured

On a development machine: Intel Core Ultra 7 255H (16 cores, no SMT), 62 GB
of memory, a WD Blue SN580 NVMe SSD (a consumer drive) with ext4, rootless
Docker 29 on cgroup v2.

- `tools/sizing/measure.py` runs the notebooks in order in one tutorial
  container, exactly as `tests/run_all.py`'s in-order pass does (the cells
  as written, no reading time), and samples the container's cgroup every
  second: memory charged, the part held by programs (anon and shmem) as
  against reclaimable file cache, CPU time, and the host's disk writes; and
  the home's size after each notebook. `tools/sizing/analyze.py` makes the
  tables below from that.
- It ran four ways: with run_all's CPU quota of 4 and no memory limit; with
  a 2 GB memory limit; and notebook 7 alone with a CPU quota of 2, and of 1.
  Three containers also ran notebooks 1 and 2 at the same moment.
- `tools/sizing/copybench.py` copies B1's tree (the restore in notebook 2's
  build cell) N times at once, in 1 MiB reads and writes as the make shim
  does, for N = 1, 10 and 30.

## Per attendee

### Memory: 2.5 GB

| Notebook | Wall s | CPU s | Programs, GiB | Charged, GiB | Home after, GiB |
|---|---|---|---|---|---|
| 1 | 22 | 58 | 0.35 | 2.19 | 1.3 |
| 2 | 114 | 142 | 0.32 | 5.76 | 3.0 |
| 3 | 106 | 62 | 0.40 | 8.44 | 4.3 |
| 4a | 36 | 128 | 0.84 | 11.08 | 6.1 |
| 4b | 100 | 50 | 0.31 | 13.72 | 7.6 |
| 5 | 188 | 160 | 0.63 | 14.06 | 8.9 |
| 6a | 7 | 5 | 0.18 | 9.83 | 8.9 |
| 6b | 14 | 9 | 0.18 | 9.83 | 8.9 |
| 7 | 293 | 904 | 0.41 | 11.48 | 9.5 |
| 8 | 53 | 56 | 0.25 | 11.18 | 9.5 |
| 9 | 119 | 120 | 0.56 | 11.32 | 9.9 |

(The run without a memory limit. "Charged" is everything the container's
cgroup is billed for, almost all of it file cache from the restores and
installs, which the kernel reclaims under pressure.)

- Programs never held more than **0.84 GiB** (notebook 4a's two
  installs); 0.80 GiB in the repeat. The kernel's own memory for the
  container peaked at 0.6 GiB.
- With a **2 GB memory limit**, every notebook passed, and took the same
  time (notebook 2: 107 s against 114; notebook 7: 296 against 293).
- A notebook's kernel, with the tutorial's magics and matplotlib loaded,
  holds about 90 MB. The scripted run has one kernel at a time; an attendee
  who leaves all twelve notebooks open holds about 1 GB of kernels.

So 2.5 GB per attendee: the measured peak, the kernel's share, and every
notebook's kernel left running. Give each container a memory *limit* well
above that (8 GB, the hub's default `MEM_LIMIT`): a limit is a cap against a
runaway, not a reservation, and a tight one also caps how much of a restore
the kernel can buffer for that container (writeback limits are per cgroup).

The fixed 16 GB covers the files every container reads (the image's bakes,
6.9 GB, and git mirrors, 3.2 GB: one copy in the page cache, shared by all
containers), the hub, Docker and the system.

### CPU: 3 vCPUs (2 at the very least)

- The whole tutorial costs an attendee **1,690 CPU seconds** (1,696 in the
  repeat), about 28 core-minutes. Over the notebooks' budgets (about 5.7
  hours in all) that averages 0.1 core: the machine is idle most of the day.
- It comes in bursts of 4 cores: the installs in notebooks 1 and 4a (15 to
  30 s each), and notebook 7's checkpointed run, which keeps 4 processes busy
  for about 3½ minutes. Builds that really compile (notebooks 5 and 9) peak
  near 4 cores for seconds.
- In a workshop everyone reaches the same cell within minutes, so the peaks
  coincide, and notebook 7 sets the floor. Its run is a chain of eight
  jobs, each computing for one minute of walltime before it checkpoints:
  with less CPU, each job gets less done, and the run has to finish within
  the eight.

  | CPU per container | Iterations per job | Run finished? |
  |---|---|---|
  | 4 | about 2,950 | yes, in the 4th job |
  | 2 | about 1,230 | yes, in exactly the 8th job: no margin |
  | 1 | about 470 | no, stopped at 3,768 of 9,600; the notebook's check fails |

So 3 vCPUs per attendee, which leaves notebook 7 a margin, and 2 as the
floor. The containers' cpusets (4 cores each, shared round-robin when the
machine has fewer than 4 × N) spread the attendees evenly over whatever
there is.

### Disk space: 15 GB

The home reaches **9.9 GiB** by the end of notebook 9, the same in both full
runs: up to five installs' sources, the restored build trees, and simulation
output (the table above). 15 GB leaves room for an attendee's own
experiments. The fixed 40 GB is the image (18.5 GB), room to load it, and
the system.

### Disk throughput: 38 MB/s each, at the same moment

An attendee writes about 13 GB over the day, nearly all of it when a
build cell restores a precomputed tree. The largest is notebook 2's: B1's
tree, 1.5 GB. The restore runs while the build's recorded output replays,
and has to be done by the replay's end, about 40 s in (the whole replay
takes 46 s). When the whole room runs that cell together, the disk must
absorb N × 1.5 GB in about 40 s: 38 MB/s per attendee.

Simultaneous restores on the development machine's NVMe:

| Restores at once | Each took | Longest stall | All on disk after | Throughput |
|---|---|---|---|---|
| 1 | 4 s | 0.01 s | 4.5 s | 340 MB/s |
| 10 | 23 s | 5.5 s | 36 s | 430 MB/s |
| 30 | 150 s | 11 s | 191 s | 240 MB/s |

The first 10 to 12 GB of a burst land in the page cache at memory speed
(Linux lets dirty pages reach 20% of memory before it throttles the
writers), then the writers slow to the disk's sustained rate. A consumer
drive's sustained rate also drops once its fast cache fills, which is why
30 copies did worse than 10. So both more memory and a faster disk help;
neither replaces the other at 30 attendees.

Three containers running notebooks 1 and 2 at once took exactly as long as
one alone (18 and 108 s each).

## What happens below these figures

- **Disk too slow**: nothing fails. A restore that falls behind makes the
  build's replay wait for it, so notebook 2's build cell takes longer than
  46 s, its output pausing. At 30 attendees on the development machine's
  drive, about 2½ minutes. The replay gives up only if a copy makes no
  progress at all for 120 s.
- **CPU short**: installs and builds slow down in proportion. Below about 2
  vCPUs per attendee, notebook 7's run doesn't finish within its chain (the
  table above), so the cell that shows the last checkpoint and the plot of
  the whole run come out incomplete.
- **Memory short**: only under 1 GB or so per attendee, with several
  notebooks' kernels open, would programs start to be killed. Short of that,
  restores and installs reread files instead of finding them cached.

## Ways to need less

- **Stagger notebook 2's build cell**: the instructor starts the room in
  groups a minute apart. Free, and it divides the disk's peak.
- **Restore by cloning**: on a filesystem with reflinks (XFS formatted with
  `reflink=1`, or btrfs) holding both the bakes and the homes, a restore
  could share the bake's blocks instead of copying them. The make shim
  copies with plain reads and writes today; it would have to use
  `os.copy_file_range` (which clones where the filesystem can), except for
  the executables and `datestamp.o`, whose build stamps it rewrites. That
  would cut most of the restore's writes (B1's executable, about 340 MB,
  would still be written) and much of each home's size. Not built or
  measured.
- **A smaller grid in notebook 7** would lower the CPU floor.

## Checking the provisioned machine

Before the image is there, the disk, with `fio` on the filesystem that will
hold `/var/lib/docker`; N writers of 1.5 GB each:

```sh
fio --name=restore --directory=/var/lib/docker/fio-test --rw=write --bs=1M \
    --size=1536M --numjobs=N --ioengine=psync --end_fsync=1 --group_reporting
```

`WRITE: bw=` should be at least N × 38 MB/s (at 10 writers on the
development machine, under other load, fio reported 168 MB/s while
`copybench.py` reached 205 MB/s: fio is the stricter of the two). Remove the
directory afterward.

Once the image is loaded, from a checkout of this repository:

```sh
tutorial/tools/sizing/copybench.py /var/lib/docker/copybench N   # the restore burst itself
tutorial/tools/sizing/measure.py ~/sizing-cpu2 --only 7 --cpus 2  # notebook 7 on 2 vCPUs: must pass
tutorial/tools/sizing/measure.py ~/sizing-full                    # one attendee, every notebook
tutorial/tools/sizing/analyze.py ~/sizing-full
```

The development machine's `measure.py --only 7 --cpus 2` passed with no
margin; on hyperthreaded vCPUs, if it fails, plan on 4 vCPUs per attendee.
