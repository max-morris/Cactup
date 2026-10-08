#!/usr/bin/env python3
"""N simultaneous restores of a precomputed build, as the make shim copies one.

    tutorial/tools/sizing/copybench.py DEST_DIR N [--tree TREE]

When the whole room runs notebook 2's build cell together, every container
copies the same 1.5 GB tree (B1's) out of the image into its home, and each
copy has to finish within the 46 seconds the build's replay takes. This
copies TREE (default: B1's tree in the tutorial image, extracted once into
DEST_DIR) N times at once, in 1 MiB reads and writes as the shim does, and
reports each copy's time, the longest stall any copy saw, and the time until
everything is on disk. DEST_DIR must be on the filesystem that will hold the
attendees' homes. It removes the copies when done. See tutorial/SIZING.md.
"""
import argparse
import json
import multiprocessing as mp
import os
import shutil
import subprocess
import time
from pathlib import Path

# Prints the directory of the bake whose bake.json says "id": "B1".
FIND_B1 = ("import json, pathlib; print(next(b.parent for b in pathlib.Path('/opt/cactup-bakes').glob('*/bake.json') "
           "if json.loads(b.read_text())['id'] == 'B1'))")


def copy_tree(args: tuple[str, str]) -> dict:
    src, dst = map(Path, args)
    t0 = last = time.monotonic()
    stall = 0.0
    n = 0
    for root, _, files in os.walk(src):
        r = Path(root)
        d = dst / r.relative_to(src)
        d.mkdir(parents=True, exist_ok=True)
        for f in files:
            s = r / f
            if s.is_symlink():
                os.symlink(os.readlink(s), d / f)
                continue
            with open(s, "rb") as fin, open(d / f, "wb") as fout:
                while chunk := fin.read(1 << 20):
                    fout.write(chunk)
                    n += len(chunk)
                    now = time.monotonic()
                    stall = max(stall, now - last)
                    last = now
    return {"seconds": time.monotonic() - t0, "stall": stall, "bytes": n}


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("dest")
    p.add_argument("n", type=int)
    p.add_argument("--tree", default=None, help="the tree to copy (default: B1's, from the image)")
    p.add_argument("--image", default="cactup-tutorial:tutorial")
    a = p.parse_args()
    dest = Path(a.dest)
    dest.mkdir(parents=True, exist_ok=True)
    tree = a.tree
    if tree is None:
        tree = str(dest / "source")
        if not Path(tree).exists():
            b1 = subprocess.run(["docker", "run", "--rm", "--entrypoint", "python3", a.image, "-c", FIND_B1],
                                capture_output=True, text=True, check=True).stdout.strip()
            cid = subprocess.run(["docker", "create", a.image], capture_output=True, text=True,
                                 check=True).stdout.strip()
            try:
                subprocess.run(["docker", "cp", f"{cid}:{b1}/tree", tree], check=True)
            finally:
                subprocess.run(["docker", "rm", cid], capture_output=True)
    # Read it once, so every copy reads from the page cache, as the
    # attendees' restores do (they all read the same image files).
    copy_tree((tree, str(dest / "warm")))
    shutil.rmtree(dest / "warm")
    os.sync()

    t0 = time.monotonic()
    with mp.Pool(a.n) as pool:
        res = pool.map(copy_tree, [(tree, str(dest / f"copy{i}")) for i in range(a.n)])
    copied = time.monotonic() - t0
    os.sync()
    durable = time.monotonic() - t0
    secs = sorted(r["seconds"] for r in res)
    print(json.dumps({
        "copies": a.n, "GB_each": round(res[0]["bytes"] / 1e9, 2),
        "seconds_median": round(secs[len(secs) // 2], 1), "seconds_max": round(secs[-1], 1),
        "longest_stall_s": round(max(r["stall"] for r in res), 2),
        "all_copied_s": round(copied, 1), "all_on_disk_s": round(durable, 1),
        "MB_per_s_to_disk": round(a.n * res[0]["bytes"] / durable / 1e6)}))
    for i in range(a.n):
        shutil.rmtree(dest / f"copy{i}")


if __name__ == "__main__":
    main()
