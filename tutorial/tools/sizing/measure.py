#!/usr/bin/env python3
"""Measure what one attendee's container uses, notebook by notebook.

    tutorial/tools/sizing/measure.py OUT_DIR [--only 1,2] [--cpus 4] [--memory 2g]

Runs the notebooks in order in one tutorial container, as run_all.py's
in-order pass does, and samples the container's cgroup every second: memory
(the total charged to it, and the part programs hold: anon and shmem, as
against file cache the kernel can reclaim), CPU time, and the host's disk
reads and writes. After each notebook it records the home's size. analyze.py
turns OUT_DIR into a table per notebook. See tutorial/SIZING.md.

`--cpus` replaces run_all's CPU quota (4), and `--memory` adds a memory
limit, to find out how little a notebook still works with.
"""
import argparse
import json
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "tests"))
import run_all  # noqa: E402

WHOLE_DISK = re.compile(r"^(nvme\d+n\d+|sd[a-z]+|vd[a-z]+|xvd[a-z]+)$")


def read_stat(path: Path) -> dict:
    out = {}
    for line in path.read_text().splitlines():
        k, v = line.split()[:2]
        out[k] = int(v)
    return out


def disk_bytes(device: str | None) -> tuple[int, int]:
    """Bytes read and written so far, by one device or by every whole disk."""
    r = w = 0
    for line in Path("/proc/diskstats").read_text().splitlines():
        f = line.split()
        if f[2] == device or (device is None and WHOLE_DISK.match(f[2])):
            r += int(f[5]) * 512
            w += int(f[9]) * 512
    return r, w


def cgroup_of(container: str) -> Path:
    """The container's cgroup (v2), found through its first process: the same
    for rootful and rootless Docker, whichever cgroup driver."""
    pid = run_all.docker("inspect", "-f", "{{.State.Pid}}", container).stdout.strip()
    for line in Path(f"/proc/{pid}/cgroup").read_text().splitlines():
        if line.startswith("0::"):
            return Path("/sys/fs/cgroup") / line[3:].lstrip("/")
    raise SystemExit("no cgroup v2 entry for the container: is the host on cgroup v2?")


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("out")
    p.add_argument("--image", default="cactup-tutorial:tutorial")
    p.add_argument("--only", default=None, help="comma-separated notebook numbers")
    p.add_argument("--cpus", default=None, help="the container's CPU quota (run_all uses 4)")
    p.add_argument("--memory", default=None, help="a memory limit for the container, such as 2g")
    p.add_argument("--device", default=None, help="the disk to count I/O on (default: every whole disk)")
    p.add_argument("--tag", default="measure")
    a = p.parse_args()
    out = Path(a.out)
    (out / "nb").mkdir(parents=True, exist_ok=True)

    real_docker = run_all.docker

    def docker(*args, **kw):
        if args and args[0] == "run" and "-d" in args:
            args = list(args)
            if a.cpus:
                args[args.index("--cpus") + 1] = a.cpus
            if a.memory:
                args[1:1] = ["--memory", a.memory, "--memory-swap", a.memory]
        return real_docker(*args, **kw)
    run_all.docker = docker

    listing = docker("run", "--rm", "--entrypoint", "ls", a.image, "/opt/cactup-tutorial/notebooks").stdout
    notebooks = sorted(n for n in listing.split() if n.endswith(".ipynb"))
    if a.only:
        wanted = {n.zfill(2) if n.isdigit() else n.zfill(3) for n in a.only.split(",")}
        notebooks = [n for n in notebooks if run_all.number(n) in wanted]

    box = run_all.Container(a.image, a.tag)
    cg = cgroup_of(box.name)
    current = {"nb": "start"}
    stop = threading.Event()
    samples = open(out / "samples.jsonl", "w")

    def sampler():
        t0 = time.monotonic()
        while not stop.is_set():
            try:
                m = read_stat(cg / "memory.stat")
                c = read_stat(cg / "cpu.stat")
                r, w = disk_bytes(a.device)
                samples.write(json.dumps({
                    "t": round(time.monotonic() - t0, 2), "nb": current["nb"],
                    "mem": int((cg / "memory.current").read_text()),
                    "anon": m["anon"], "file": m["file"], "shmem": m["shmem"],
                    "kernel": m.get("kernel", 0), "cpu_us": c["usage_usec"],
                    "pids": int((cg / "pids.current").read_text()),
                    "disk_r": r, "disk_w": w}) + "\n")
                samples.flush()
            except OSError:
                pass
            stop.wait(1.0)

    th = threading.Thread(target=sampler, daemon=True)
    th.start()
    summary = []
    try:
        for i, nb in enumerate(notebooks):
            current["nb"] = run_all.number(nb)
            started = time.monotonic()
            result = box.run(nb, out / "nb")
            took = time.monotonic() - started
            problems = run_all.check("measure:", nb, result, run_all.EXPECT, i > 0)
            du = box.shell("du -sxk /home/cactus /var/spool/slurmctld /tmp 2>/dev/null").stdout
            row = {"nb": nb, "seconds": round(took), "problems": len(problems), "du": du}
            summary.append(row)
            print(f"{nb}: {row['seconds']} s, {len(problems)} problem(s)", flush=True)
            for prob in problems:
                print("  " + prob.splitlines()[0], flush=True)
        current["nb"] = "end"
        time.sleep(5)
    finally:
        stop.set()
        th.join()
        (out / "summary.json").write_text(json.dumps(summary, indent=1))
        box.close()


if __name__ == "__main__":
    main()
