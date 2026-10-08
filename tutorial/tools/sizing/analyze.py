#!/usr/bin/env python3
"""Per-notebook figures from a measure.py run.

    tutorial/tools/sizing/analyze.py OUT_DIR

Columns: wall seconds; CPU seconds; cores in use on average, at the 95th
percentile of one-second samples, at most, and at most over five seconds;
GiB held by programs (anon + shmem), charged in all, and in file cache; GiB
written to disk (the whole host's disks: only meaningful on a quiet host);
the home's GiB after the notebook; the most processes at once.
"""
import json
import sys
from collections import OrderedDict

G = 1 << 30
out = sys.argv[1]
rows = [json.loads(line) for line in open(f"{out}/samples.jsonl")]
homes = {}
for r in json.load(open(f"{out}/summary.json")):
    homes[r["nb"].split("-", 1)[0]] = int(r["du"].split()[0]) / (1 << 20)
groups = OrderedDict()
for r in rows:
    groups.setdefault(r["nb"], []).append(r)

print(f"{'nb':5} {'wall':>5} {'cpu-s':>6} {'avg':>5} {'p95':>5} {'max':>5} {'5s-max':>6} "
      f"{'progs':>6} {'charged':>7} {'cache':>6} {'diskW':>6} {'home':>6} {'pids':>4}")
prev = None
total_cpu = 0.0
for nb, rs in groups.items():
    seq = ([prev] if prev else []) + rs
    inst = [(b["cpu_us"] - a["cpu_us"]) / 1e6 / (b["t"] - a["t"]) for a, b in zip(seq, seq[1:]) if b["t"] > a["t"]]
    five = [sum(inst[i:i + 5]) / 5 for i in range(max(1, len(inst) - 4))]
    first = seq[0]
    cpu = (rs[-1]["cpu_us"] - first["cpu_us"]) / 1e6
    total_cpu += cpu
    wall = rs[-1]["t"] - first["t"]
    s = sorted(inst) or [0.0]
    home = f"{homes[nb]:.1f}" if nb in homes else ""
    print(f"{nb:5} {wall:5.0f} {cpu:6.0f} {cpu / max(wall, 1):5.2f} {s[int(0.95 * (len(s) - 1))]:5.2f} "
          f"{s[-1]:5.2f} {max(five or [0]):6.2f} "
          f"{max(r['anon'] + r['shmem'] for r in rs) / G:6.2f} {max(r['mem'] for r in rs) / G:7.2f} "
          f"{max(r['file'] for r in rs) / G:6.2f} {(rs[-1]['disk_w'] - first['disk_w']) / G:6.2f} "
          f"{home:>6} {max(r['pids'] for r in rs):4}")
    prev = rs[-1]
print(f"total: {total_cpu:.0f} CPU seconds in {rows[-1]['t']:.0f} s; programs held at most "
      f"{max(r['anon'] + r['shmem'] for r in rows) / G:.2f} GiB, the kernel at most "
      f"{max(r['kernel'] for r in rows) / G:.2f} GiB")
