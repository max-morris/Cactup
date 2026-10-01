#!/usr/bin/env python3
"""Run the tutorial's notebooks headless, as an attendee would, and check them.

    tutorial/tests/run_all.py [--image IMAGE] [--out DIR] [--only 1,2] [--no-alone]

Each run starts a fresh tutorial container (hostname cactup-tutorial, on a
network with no route out) and executes notebooks in it with nbclient, as
the tutorial user, with CACTUP_TUTORIAL_STRICT=1: a cell whose command fails
unexpectedly (or a cell marked --expect-fail that succeeds) fails the run.

- In order: every notebook, one after another, in one container; then the
  editor-files check (below); then every notebook again, in the same
  container, since every cell must be safe to run again; then a full
  cactup-tutorial-reset from a notebook, catch-up in the same kernel, and a
  kernel started afterward.
- Alone: each notebook in a container of its own, relying on its catch-up
  cell for everything earlier notebooks would have done (in parallel).

For each notebook it checks that the outputs contain what they must
(EXPECT, or on the second pass RERUN), that none contains what must never
appear (NEVER), that no cell not marked --expect-fail printed an error on
the way (a cell's status is only its last command's), and, in order, that
catch-up had nothing to do (on the second pass, but for notebook 2
switching back to the stock installation). The executed notebooks are saved in --out.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

# What each notebook's outputs must contain, in order of appearance.
EXPECT = {
    "01": [
        r"Hello from cactup-tutorial, as cactus",
        r"interrupted",
        r"cactup is installed",
        r"cactup +updated to \w+",
        r"ET_2026_05_v0 \(latest\)",
        r"Success! Installed release ET_2026_05_v0",
        r"file:///opt/cactup-mirrors/",
        r"cactup-tutorial \(System MDB",
    ],
    "03": [
        r"Success! Installed custom thornlist",
        r"Switched to installation et-mp",
        r"Built config tutorial",
        r"\+CarpetX/TestReal4",
        r"flesh — SKIP: remote URL",
        r"origin now points at https://github.com/max-morris/Cactus.git",
        r"now on a different commit",
        r"Rebuilding config tutorial from scratch: the Cactus flesh is not what it was",
        r"Built config tutorial",
        r"TestReal4\[state4\]: PASS",
        r"Switched to installation ET_2026_05_v0",
    ],
    "04a": [
        r"Success! Installed custom thornlist /opt/cactup-tutorial/thornlists/carpetx.th",
        r"Success! Installed release master",
        r"- carpetx \(custom thornlist",
        r"Switched to installation carpetx",
        r"et-master\n  release: +master",
        r"Switched to installation ET_2026_05_v0",
        r"Uninstalled carpetx",
    ],
    "04b": [
        r"gpu \[gpu\]",
        r"Success! Installed custom thornlist /opt/cactup-tutorial/thornlists/tutorial-gpu.th",
        r"Built config tutorial-gpu",
        r"gpu: true +compatible-queues: gpu",
        r"only compatible with queue\(s\) gpu",
        r"gpu +inact",
        r"Required partition not available",
        r"Built config tutorial-debug",
        r"flags: debug=true",
        r"Config tutorial-debug is now active",
        r"Config tutorial is now active",
        r"Deleted config tutorial-debug",
    ],
    "02": [
        r"Built config tutorial",
        r"status: complete",
        r"Submitted lw restart output-0000 as job \d+",
        r"state: +FINISHED",
        r"Done\.",
        r"cottonmouthz4c4m-hamcons\.tsv",
        r"21 snapshots of gt along x",
    ],
}

# The second pass, run again from the top: what must still show.
RERUN = {
    "01": [
        r"cactup +updated to \w+",
        r"file:///opt/cactup-mirrors/",
    ],
    "02": [
        r"is up to date|Built config tutorial",
        r"state: +FINISHED",
        r"21 snapshots of gt along x",
    ],
    "03": [
        r"already exists",
        r"is up to date",
        r"TestReal4\[state4\]: PASS",
    ],
    "04a": [
        r"Success! Installed custom thornlist /opt/cactup-tutorial/thornlists/carpetx.th",
        r"already exists",
        r"Uninstalled carpetx",
    ],
    "04b": [
        r"already exists",
        r"tutorial-gpu is up to date",
        r"Required partition not available",
        r"Built config tutorial-debug",
    ],
}

# What no notebook may ever show: the build replay missing its bake or
# showing through, cactup noticing edits nobody made, a Python traceback.
NEVER = [
    r"not precomputed",
    r"the source tree has moved",
    r"Traceback \(most recent call last\)",
    r"make_shim",
]

# What a cell not marked --expect-fail may not print, even if its last
# command succeeded: an error from cactup (or anything else) on the way.
ERRORS = [
    r"(?mi)^error: ",
    r"(?m)^fatal: ",
    r"(?m)^\w+: error: ",
    r"command not found",
    r"(?m)^(?:ls|cat|cd|cp|mv|rm|find|grep|head|tail|bash|sh): .*No such file or directory",
]

RUNNER = r'''
import json, os, sys
import nbformat
from nbclient import NotebookClient
from nbclient.exceptions import CellExecutionError

src, dst = sys.argv[1], sys.argv[2]
nb = nbformat.read(src, as_version=4)
client = NotebookClient(nb, timeout=1800, kernel_name="python3",
                        resources={"metadata": {"path": os.path.dirname(src)}})
error = None
try:
    client.execute()
except CellExecutionError as e:
    error = str(e)[-3000:]
nbformat.write(nb, dst)
cells = []
for cell in nb.cells:
    if cell.cell_type != "code":
        continue
    text = []
    for out in cell.get("outputs", []):
        if out.get("output_type") == "stream":
            text.append(out.get("text", ""))
        elif out.get("output_type") in ("display_data", "execute_result", "update_display_data"):
            text.append(out.get("data", {}).get("text/plain", ""))
        elif out.get("output_type") == "error":
            text.append("\n".join(out.get("traceback", [])))
    cells.append({"source": cell.source[:200], "text": "".join(text)})
print(json.dumps({"error": error, "cells": cells}))
'''


def docker(*args: str, check: bool = True, **kw) -> subprocess.CompletedProcess:
    return subprocess.run(["docker", *args], check=check, text=True, capture_output=True, **kw)


class Container:
    def __init__(self, image: str, tag: str):
        self.name = f"cactup-tutorial-runall-{tag}-{os.getpid()}"
        self.net = f"{self.name}-net"
        docker("network", "create", "--internal", self.net)
        try:
            docker("run", "-d", "--name", self.name, "--network", self.net, "--hostname", "cactup-tutorial",
                   "--cpus", "4", "-e", "CACTUP_TUTORIAL_TOKEN=runall", image)
            for _ in range(180):
                if docker("exec", self.name, "curl", "-fs", "http://127.0.0.1:8888/api",
                          check=False).returncode == 0:
                    break
                time.sleep(1)
            else:
                raise SystemExit(f"{self.name}: the notebook server did not start")
        except BaseException:
            self.close()
            raise

    def shell(self, script: str) -> subprocess.CompletedProcess:
        return subprocess.run(["docker", "exec", "-i", "-u", "cactus", "-w", "/home/cactus", "-e",
                               "HOME=/home/cactus", "-e", "USER=cactus", self.name, "bash", "-c",
                               "umask 022; export PATH=/home/cactus/.cactup/bin:$PATH; " + script],
                              text=True, capture_output=True)

    def run(self, notebook: str, out_dir: Path) -> dict:
        path = f"/home/cactus/notebooks/{notebook}"
        executed = f"/tmp/{notebook}"
        # As the notebook server's kernels run: umask 022, and the CPUs the
        # entrypoint exports.
        proc = subprocess.run(
            ["docker", "exec", "-i", "-u", "cactus", "-w", "/home/cactus/notebooks", "-e", "HOME=/home/cactus",
             "-e", "USER=cactus", "-e", "CACTUP_TUTORIAL_STRICT=1", self.name, "sh", "-c",
             'umask 022; export CACTUP_TUTORIAL_CPUS="$(sed -n "s/^Cpus_allowed_list:[[:space:]]*//p" '
             '/proc/self/status)"; exec /opt/venv/bin/python - "$@"', "sh", path, executed],
            input=RUNNER, text=True, capture_output=True,
        )
        if proc.returncode != 0:
            return {"error": proc.stderr[-3000:], "cells": []}
        subprocess.run(["docker", "cp", f"{self.name}:{executed}", str(out_dir / notebook)],
                       check=False, capture_output=True)
        return json.loads(proc.stdout)

    def close(self) -> None:
        docker("rm", "-f", self.name, check=False)
        docker("network", "rm", self.net, check=False)


def number(notebook: str) -> str:
    """A notebook's number as its file name spells it: "01", "04a"."""
    return notebook.split("-", 1)[0]


def check(label: str, notebook: str, result: dict, expect: dict, quiet_catch_up: bool,
          allowed_catch_up: str = r"$^") -> list[str]:
    """Problems with one executed notebook."""
    problems = []
    if result["error"]:
        problems.append(f"{label} {notebook}: a cell failed:\n{result['error']}")
    if not result["cells"]:
        problems.append(f"{label} {notebook}: no cells ran")
    text = "\n".join(c["text"] for c in result["cells"])
    if number(notebook) not in expect:
        problems.append(f"{label} {notebook}: nothing to check it against (add it to EXPECT and RERUN)")
    position = 0
    for pattern in expect.get(number(notebook), []):
        m = re.compile(pattern).search(text, position)
        if m is None:
            problems.append(f"{label} {notebook}: expected output not found (in order): {pattern}")
        else:
            position = m.end()
    for cell in result["cells"]:
        patterns = NEVER + ([] if "--expect-fail" in cell["source"] else ERRORS)
        for pattern in patterns:
            if re.search(pattern, cell["text"]):
                problems.append(f"{label} {notebook}: forbidden output {pattern!r} in cell:\n{cell['source']}")
        if quiet_catch_up and "cactup-tutorial-catch-up" in cell["source"]:
            lines = [ln for ln in cell["text"].splitlines()
                     if ln.startswith("catch-up:") and not re.search(allowed_catch_up, ln)]
            if lines:
                problems.append(f"{label} {notebook}: catch-up had things to do after the notebooks before it:\n"
                                + "\n".join(lines))
    return problems


def editor_files(box: Container) -> list[str]:
    """Opening and saving a thorn's source in JupyterLab, and a vim swap file
    beside it, must not change the thorn (see "Editor files" in the README):
    the build stays up to date and no checkpoint lands in the source tree."""
    rel = "Cactus/arrangements/Cottonmouth/CottonmouthZ4c4m/src/CottonmouthZ4c4m_sync_state.cpp"
    script = f"""
import json, urllib.request
base = "http://127.0.0.1:8888/api/contents/{rel}"
def call(method, url, body=None):
    req = urllib.request.Request(url, method=method, data=None if body is None else json.dumps(body).encode(),
                                 headers={{"Authorization": "token runall", "Content-Type": "application/json"}})
    with urllib.request.urlopen(req) as r:
        return r.status, json.loads(r.read() or b"null")
status, _ = call("POST", base + "/checkpoints")
_, model = call("GET", base + "?type=file&format=text&content=1")
call("PUT", base, {{"type": "file", "format": "text", "content": model["content"]}})
print(status)
"""
    problems = []
    posted = subprocess.run(["docker", "exec", "-i", box.name, "/opt/venv/bin/python", "-"], input=script,
                            text=True, capture_output=True)
    if posted.stdout.strip() != "201":
        problems.append(f"editor files: creating a checkpoint returned {posted.stdout.strip() or posted.stderr}")
    out = box.shell(f"touch ~/{rel.rsplit('/', 1)[0]}/.CottonmouthZ4c4m_sync_state.cpp.swp; "
                    "cactup build tutorial 2>&1; echo '@@stray'; "
                    "find -L ~/Cactus -name .ipynb_checkpoints | sed -n 1p; echo '@@kept'; "
                    "find ~/.local/share/jupyter/checkpoints -name 'CottonmouthZ4c4m_sync_state*' | sed -n 1p").stdout
    build, rest = (out.split("@@stray", 1) + [""])[:2]
    stray, kept = (rest.split("@@kept", 1) + [""])[:2]
    if "is up to date" not in build:
        problems.append(f"editor files: the build after a checkpoint, a save and a swap file wasn't up to date:\n"
                        f"{build}")
    if stray.strip():
        problems.append(f"editor files: a checkpoint landed in the source tree: {stray.strip()}")
    if not kept.strip():
        problems.append("editor files: no checkpoint under ~/.local/share/jupyter/checkpoints")
    return problems


def scratch_notebook(box: Container, name: str, cells: list[str]) -> None:
    """Write a notebook of %%shell cells into ~/notebooks in the container."""
    nb = {"nbformat": 4, "nbformat_minor": 5, "metadata": {"kernelspec": {"name": "python3",
          "display_name": "Python 3", "language": "python"}},
          "cells": [{"cell_type": "code", "metadata": {}, "execution_count": None, "outputs": [], "id": f"c{i}",
                     "source": src} for i, src in enumerate(cells)]}
    subprocess.run(["docker", "exec", "-i", "-u", "cactus", box.name, "sh", "-c",
                    f"cat > /home/cactus/notebooks/{name}"], input=json.dumps(nb), text=True, check=True)


def start_over(box: Container, nb3: str, out_dir: Path) -> list[str]:
    """Notebook 3's optional "start over" cell, uncommented and run, then
    the whole notebook again: as on its first run."""
    source = subprocess.run(["docker", "exec", box.name, "cat", f"/opt/cactup-tutorial/notebooks/{nb3}"],
                            capture_output=True, text=True, check=True).stdout
    cells = ["".join(c["source"]) for c in json.loads(source)["cells"] if c["cell_type"] == "code"]
    cell = next((c for c in cells if "# cactup uninstall et-mp" in c), None)
    if cell is None:
        return ["start over: notebook 3 has no start-over cell"]
    uncommented = re.sub(r"(?m)^# (cactup|rm) ", r"\1 ", cell)
    scratch_notebook(box, "zz-start-over.ipynb", [uncommented])
    first = box.run("zz-start-over.ipynb", out_dir)
    if first["error"]:
        return [f"start over: the start-over cell failed:\n{first['error']}"]
    return check("after starting over:", nb3, box.run(nb3, out_dir), EXPECT, False)


def reset_then_catch_up(box: Container, out_dir: Path) -> list[str]:
    """A full reset run from a notebook, then catch-up in the same kernel,
    and a kernel started afterward: the server and the kernel's shell must
    both survive the reset."""
    problems = []
    scratch_notebook(box, "zz-reset.ipynb", [
        "%%shell\ncactup-tutorial-reset -y",
        "%%shell\ncactup-tutorial-catch-up 2",
        "%%shell\ncactup list\npwd\nls",
    ])
    result = box.run("zz-reset.ipynb", out_dir)
    text = "\n".join(c["text"] for c in result["cells"])
    if result["error"]:
        problems.append(f"reset: a cell failed:\n{result['error']}")
    for pattern in (r"as it was at the start", r"installed cactup", r"installing ET_2026_05_v0",
                    r"- ET_2026_05_v0 .*\(active\)", r"/home/cactus/notebooks\n01-getting-started\.ipynb"):
        if not re.search(pattern, text):
            problems.append(f"reset: expected output not found: {pattern}\n{text[-1500:]}")
    # A kernel started by the notebook server itself (nbclient starts its own,
    # without the server's runtime directory).
    started = box.shell(
        "curl -s -o /tmp/kernel.json -w '%{http_code}' -X POST -H 'Authorization: token runall' "
        "http://127.0.0.1:8888/api/kernels && "
        "curl -s -o /dev/null -X DELETE -H 'Authorization: token runall' "
        "http://127.0.0.1:8888/api/kernels/$(jq -r .id /tmp/kernel.json)"
    ).stdout.strip()
    if not started.startswith("201"):
        problems.append(f"reset: the notebook server could not start a kernel afterward (HTTP {started})")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--image", default="cactup-tutorial:tutorial")
    parser.add_argument("--out", default=None, help="where to save the executed notebooks")
    parser.add_argument("--only", default=None, help="comma-separated notebook numbers")
    parser.add_argument("--no-alone", action="store_true", help="skip the one-container-per-notebook runs")
    args = parser.parse_args()
    out = Path(args.out or tempfile.mkdtemp(prefix="runall-"))
    listing = docker("run", "--rm", "--entrypoint", "ls", args.image, "/opt/cactup-tutorial/notebooks").stdout
    notebooks = sorted(n for n in listing.split() if n.endswith(".ipynb"))
    if args.only:
        wanted = {n.zfill(2) if n.isdigit() else n.zfill(3) for n in args.only.split(",")}
        notebooks = [n for n in notebooks if number(n) in wanted]
    if not notebooks:
        raise SystemExit("no notebooks to run")
    problems: list[str] = []

    (out / "in-order").mkdir(parents=True, exist_ok=True)
    (out / "again").mkdir(parents=True, exist_ok=True)
    box = Container(args.image, "order")
    try:
        for i, nb in enumerate(notebooks):
            # Catch-up has nothing to do once the notebooks before it ran here.
            started = time.monotonic()
            found = check("in order:", nb, box.run(nb, out / "in-order"), EXPECT, i > 0)
            problems += found
            print(f"{'ok  ' if not found else 'FAIL'} in order: {nb} ({time.monotonic() - started:.0f} s)",
                  flush=True)
            if nb.startswith("02"):
                found = editor_files(box)
                problems += found
                print(f"{'ok  ' if not found else 'FAIL'} editor files after {nb}", flush=True)
        for nb in notebooks:
            started = time.monotonic()
            # (Except one thing: running notebook 2 after notebook 3 switches
            # back to the stock installation, and says so.)
            found = check("again:", nb, box.run(nb, out / "again"), RERUN, True,
                          r"made ET_2026_05_v0 the active installation" if nb.startswith("02") else r"$^")
            problems += found
            print(f"{'ok  ' if not found else 'FAIL'} again: {nb} ({time.monotonic() - started:.0f} s)",
                  flush=True)
        for nb3 in (nb for nb in notebooks if nb.startswith("03")):
            (out / "start-over").mkdir(exist_ok=True)
            found = start_over(box, nb3, out / "start-over")
            problems += found
            print(f"{'ok  ' if not found else 'FAIL'} notebook 3 again, after its start-over cell", flush=True)
        found = reset_then_catch_up(box, out / "again")
        problems += found
        print(f"{'ok  ' if not found else 'FAIL'} a full reset from a notebook, then catch-up", flush=True)
    finally:
        box.close()

    if not args.no_alone:
        (out / "alone").mkdir(exist_ok=True)

        def alone(nb: str) -> tuple[str, list[str], float]:
            started = time.monotonic()
            box = Container(args.image, f"alone{number(nb)}")
            try:
                return nb, check("alone:", nb, box.run(nb, out / "alone"), EXPECT, False), time.monotonic() - started
            finally:
                box.close()

        with ThreadPoolExecutor(max_workers=4) as pool:
            for nb, found, took in pool.map(alone, notebooks):
                problems += found
                print(f"{'ok  ' if not found else 'FAIL'} alone: {nb} ({took:.0f} s)", flush=True)

    print(f"executed notebooks: {out}")
    for p in problems:
        print(p, file=sys.stderr)
    print(f"{len(problems)} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
