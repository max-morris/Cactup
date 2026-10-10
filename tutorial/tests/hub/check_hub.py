#!/usr/bin/env python3
"""Check a running tutorial hub end to end, as attendees would use it.

    tutorial/tests/hub/check_hub.py --url http://127.0.0.1:8000 --shared DIR [--users 2] [--only 1,2]

Against a hub brought up with tutorial/deploy/compose.yaml (see
tutorial/tests/hub/run.sh for a local one), it:

- signs up the users through the login page with the session token read
  from the shared directory, and checks what must be refused (no token, a
  wrong token, a short password, a taken name with the wrong password);
- starts their servers through the hub's API and runs every notebook in
  order in each user's container, at the same time, with run_all's checks;
- with --hub-container, checks that every setting jupyterhub_config.py
  makes, with all its optional ones on, is one JupyterHub knows (it only
  warns about the others, and ignores them), then restarts the hub itself:
  the servers must keep running, untouched (a restarted hub picks them up
  again);
- stops one server and starts it again: the home must survive, and SLURM's
  job ids must keep counting up;
- where Docker applies cpusets, checks the attendees got different cores.
"""

from __future__ import annotations

import argparse
import re
import secrets
import subprocess
import sys
import time
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

import requests

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import run_all  # noqa: E402


# Run in the hub's container: the settings jupyterhub_config.py makes that
# their class doesn't have.
UNKNOWN_SETTINGS = """
from cactup_cyol.authenticator import CYOLAuthenticator
from dockerspawner import DockerSpawner
from jupyterhub.app import JupyterHub
app = JupyterHub()
app.load_config_file("/srv/jupyterhub_config.py")
classes = {"JupyterHub": JupyterHub, "Authenticator": CYOLAuthenticator, "CYOLAuthenticator": CYOLAuthenticator,
           "Spawner": DockerSpawner, "DockerSpawner": DockerSpawner}
for section, values in app.config.items():
    known = classes[section].class_trait_names(config=True) if section in classes else []
    print(*(f"{section}.{key}" for key in values if key not in known))
"""

class Attendee:
    def __init__(self, url: str, name: str, password: str):
        self.url, self.name, self.password = url.rstrip("/"), name, password
        self.http = requests.Session()

    def login(self, token: str = "") -> bool:
        page = self.http.get(f"{self.url}/hub/login", timeout=30)
        xsrf = re.search(r'name="_xsrf" value="([^"]+)"', page.text).group(1)
        r = self.http.post(f"{self.url}/hub/login", timeout=60, allow_redirects=False,
                           data={"_xsrf": xsrf, "username": self.name, "password": self.password, "otp": token})
        return r.status_code == 302

    def api(self, method: str, path: str, **kw) -> requests.Response:
        headers = {"X-XSRFToken": self.http.cookies.get("_xsrf", "")}
        return self.http.request(method, f"{self.url}/hub/api{path}", headers=headers, timeout=60, **kw)

    def start(self) -> None:
        r = self.api("POST", f"/users/{self.name}/server")
        if r.status_code not in (201, 202, 400):  # 400: already running
            raise SystemExit(f"{self.name}: starting the server: {r.status_code} {r.text[:300]}")
        deadline = time.time() + 300
        while time.time() < deadline:
            server = self.api("GET", f"/users/{self.name}").json().get("servers", {}).get("", {})
            if server.get("ready"):
                return
            time.sleep(2)
        raise SystemExit(f"{self.name}: the server did not become ready")

    def stop(self) -> None:
        self.api("DELETE", f"/users/{self.name}/server")
        deadline = time.time() + 120
        while time.time() < deadline:
            gone = not self.api("GET", f"/users/{self.name}").json().get("servers")
            # The hub calls a server stopped a moment before DockerSpawner has
            # removed its container; a start in between finds the name taken.
            if gone and not subprocess.run(["docker", "ps", "-aq", "-f", f"name=^{self.container.name}$"],
                                           capture_output=True, text=True).stdout.strip():
                return
            time.sleep(2)
        raise SystemExit(f"{self.name}: the server did not stop")

    @property
    def container(self) -> run_all.Container:
        box = run_all.Container.__new__(run_all.Container)
        # DockerSpawner's name for it: usernames here are [a-z0-9-], and its
        # escaping turns each "-" into "-2d".
        box.name = "cactup-" + self.name.replace("-", "-2d")
        return box


def refusals(url: str, token: str) -> list[str]:
    problems = []
    name = f"nobody{secrets.randbelow(10**6)}"
    cases = [
        ("no token", Attendee(url, name, "a good password"), ""),
        ("a wrong token", Attendee(url, name, "a good password"), "harp sailed 3 calm otters!"),
        ("a short password", Attendee(url, name, "short"), token),
    ]
    for what, attendee, tok in cases:
        if attendee.login(tok):
            problems.append(f"login with {what} was accepted")
    return problems


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("--url", default="http://127.0.0.1:8000")
    parser.add_argument("--shared", required=True, help="the hub's shared directory (SHARED_DIR)")
    parser.add_argument("--users", type=int, default=2)
    parser.add_argument("--only", default=None, help="comma-separated notebook numbers")
    parser.add_argument("--out", default=None)
    parser.add_argument("--hub-container", default=None,
                        help="the hub's container: restart it, and check the servers survive")
    args = parser.parse_args()
    out = Path(args.out or Path(__file__).resolve().parent / "out" / f"hub-check-{int(time.time())}")
    token = (Path(args.shared) / "session-token").read_text().strip()
    problems = refusals(args.url, token)
    print(f"{'ok  ' if not problems else 'FAIL'} refusals", flush=True)

    tag = secrets.token_hex(2)
    attendees = [Attendee(args.url, f"check{tag}-{i}", secrets.token_urlsafe(12)) for i in range(args.users)]
    for a in attendees:
        if not a.login(token):
            problems.append(f"{a.name}: sign-up with the session token failed")
    # Back again: the password alone; a wrong one is refused.
    again = Attendee(args.url, attendees[0].name, attendees[0].password)
    if not again.login():
        problems.append("logging in again with the password alone failed")
    if Attendee(args.url, attendees[0].name, "not the password").login(token):
        problems.append("a taken name with the wrong password was accepted")
    print(f"{'ok  ' if not problems else 'FAIL'} sign-up and login", flush=True)
    if problems:
        for p in problems:
            print(p, file=sys.stderr)
        return 1

    with ThreadPoolExecutor(len(attendees)) as pool:
        list(pool.map(Attendee.start, attendees))
    print("ok   servers started", flush=True)
    for a in attendees:
        r = a.container.shell("hostname; nproc; cat /proc/self/status | grep Cpus_allowed_list")
        print(f"     {a.name}: {' | '.join(r.stdout.split(chr(10))[:3])}", flush=True)
    # Docker records a cpuset only where it applies one (rootless Docker
    # without the cpuset controller drops it, with a warning).
    cpusets = [subprocess.run(["docker", "inspect", "-f", "{{.HostConfig.CpusetCpus}}", a.container.name],
                              capture_output=True, text=True).stdout.strip() for a in attendees]
    if all(cpusets):
        if len(set(cpusets)) != len(cpusets):
            problems.append(f"attendees share cores: {cpusets}")
        print(f"{'ok  ' if len(set(cpusets)) == len(cpusets) else 'FAIL'} cpusets {cpusets}", flush=True)
    else:
        print("     (no cpusets: Docker doesn't apply them here)", flush=True)

    listing = subprocess.run(["docker", "exec", attendees[0].container.name, "ls", "/opt/cactup-tutorial/notebooks"],
                             capture_output=True, text=True, check=True).stdout
    notebooks = sorted(n for n in listing.split() if n.endswith(".ipynb"))
    if args.only:
        wanted = {n.zfill(2) if n.isdigit() else n.zfill(3) for n in args.only.split(",")}
        notebooks = [n for n in notebooks if run_all.number(n) in wanted]
    if not notebooks:
        raise SystemExit("no notebooks to run")

    def run_notebooks(a: Attendee) -> list[str]:
        found = []
        (out / a.name).mkdir(parents=True, exist_ok=True)
        for i, nb in enumerate(notebooks):
            started = time.monotonic()
            got = run_all.check(f"{a.name}:", nb, a.container.run(nb, out / a.name), run_all.EXPECT, i > 0)
            found += got
            print(f"{'ok  ' if not got else 'FAIL'} {a.name}: {nb} ({time.monotonic() - started:.0f} s)", flush=True)
        return found

    with ThreadPoolExecutor(len(attendees)) as pool:
        for found in pool.map(run_notebooks, attendees):
            problems += found

    # Every setting jupyterhub_config.py makes is a real one, and a restarted
    # hub leaves the servers running, and picks them up again.
    if args.hub_container:
        found = subprocess.run(["docker", "exec", "-e", "SECURE_COOKIES=1", "-e", "CPUSET=1", "-e", "GPU=1",
                                args.hub_container, "python3", "-c", UNKNOWN_SETTINGS],
                               capture_output=True, text=True)
        unknown = found.stdout.split() if found.returncode == 0 else [found.stderr.strip()[-300:]]
        problems += [f"jupyterhub_config.py: not a setting: {u}" for u in unknown]
        print(f"{'ok  ' if not unknown else 'FAIL'} the hub knows every setting", flush=True)
        ids = [subprocess.run(["docker", "inspect", "-f", "{{.Id}}", a.container.name],
                              capture_output=True, text=True).stdout.strip() for a in attendees]
        subprocess.run(["docker", "restart", args.hub_container], check=True, capture_output=True)
        deadline = time.time() + 120
        while time.time() < deadline:
            try:
                if requests.get(f"{args.url}/hub/api/", timeout=5).ok:
                    break
            except requests.RequestException:
                pass
            time.sleep(2)
        after = [subprocess.run(["docker", "inspect", "-f", "{{.Id}}", a.container.name],
                                capture_output=True, text=True).stdout.strip() for a in attendees]
        kept = ids == after and all(ids)
        ready = all(a.api("GET", f"/users/{a.name}").json().get("servers", {}).get("", {}).get("ready")
                    for a in attendees)
        if not (kept and ready):
            problems.append(f"a hub restart disturbed the servers (same containers: {kept}, ready: {ready})")
        print(f"{'ok  ' if kept and ready else 'FAIL'} a hub restart leaves the servers running", flush=True)

    # A restart keeps the home and SLURM's job ids.
    first = attendees[0]
    before = first.container.shell("sbatch --parsable --wrap true").stdout.strip()
    marker = first.container.shell("echo kept > ~/hub-check-marker").returncode
    first.stop()
    first.start()
    after = first.container.shell("sbatch --parsable --wrap true").stdout.strip()
    kept = first.container.shell("cat ~/hub-check-marker").stdout.strip()
    if marker != 0 or kept != "kept":
        problems.append("the home did not survive a restart")
    if not (before.isdigit() and after.isdigit() and int(after) > int(before)):
        problems.append(f"SLURM job ids did not keep counting up across a restart ({before} then {after})")
    print(f"{'ok  ' if kept == 'kept' else 'FAIL'} restart keeps the home; job ids {before} then {after}", flush=True)

    print(f"executed notebooks: {out}")
    for p in problems:
        print(p, file=sys.stderr)
    print(f"{len(problems)} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
