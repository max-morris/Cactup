"""Bake the tutorial's precomputed builds (see "Bakes" in tutorial/README.md).

Runs as root inside a bake container: `tutorial/image/build.sh` starts the
tutorial image with `--hostname cactup-tutorial`, an empty volume for
/home/cactus, this directory at /bake and the host's bake cache at /bakes.
Installs and builds run as the tutorial user, exactly as an attendee's would;
only the harvest into /bakes runs as root (which, under rootless Docker, is
the host user who owns the cache).

    python3 /bake/bake.py B1 [B2a ...]

For each bake it installs what the bake needs from the mirrors, does what the
notebook does to it first (`prepare`: notebook 3's thornlist edit and refetch
for B2b; each step runs once, and they accumulate, so bakes run in the order
the notebooks build them), runs the bake's `cactup build` once as a probe that
only learns the build's fingerprint, and, unless the cache already has that
bake for this toolchain, runs the build for real with the make shim recording,
and harvests the result into /bakes/<toolchain>/<fingerprint>/. It writes the
fingerprints the image needs to /bakes/<toolchain>/wanted.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import re
import shutil
import signal
import stat
import subprocess
import sys
import tempfile
import threading
import time
import tomllib
from datetime import datetime
from pathlib import Path

# The image's shim; in a checkout (the tests), the one beside this directory.
sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "image/rootfs/usr/local/lib/cactup-tutorial"))
sys.path.insert(0, "/usr/local/lib/cactup-tutorial")
import make_shim  # noqa: E402

USER = "cactus"
HOME = Path("/home/cactus")
THORNLISTS = Path("/opt/cactup-tutorial/thornlists")
UPDATE_ROOT = Path("/opt/cactup-tutorial/update-root")
RECORD = HOME / ".cactup-tutorial-record"

# Each bake: what to install (once, whichever bake needs it first), what to
# do to it first (in order, once), then the build to bake, as a notebook runs
# them.
ET_MP = ["cactup", "install", "--thornlist", str(THORNLISTS / "tutorial.th"), "--alias", "et-mp",
         "--symlink-name", "et-mp", "--silent"]
BAKES = {
    "B1": {
        "install": [["cactup", "install", "ET_2026_05_v0", "--silent"]],
        "build": ["cactup", "build", "tutorial", "--thornlist", str(THORNLISTS / "tutorial.th")],
        "root": "Cactus",
        "config": "tutorial",
        "kind": "full",
    },
    # Notebook 3: et-mp's tutorial config before and after the refetch to the
    # mixed-precision forks.
    "B2a": {
        "install": [ET_MP],
        "build": ["cactup", "build", "tutorial", "-I", "et-mp"],
        "root": "et-mp",
        "config": "tutorial",
        "kind": "partial",
    },
    "B2b": {
        "install": [ET_MP],
        "prepare": [["python3", str(Path(__file__).resolve().parent / "forks.py")],
                    ["cactup", "inst", "refetch", "--overwrite", "flesh,CarpetX", "-I", "et-mp"]],
        "build": ["cactup", "build", "tutorial", "-I", "et-mp"],
        "root": "et-mp",
        "config": "tutorial",
        "kind": "partial",
    },
    # Notebook 4b: the GPU variant, in its own installation whose CarpetX has
    # the fix nvcc needs (tutorial-gpu.th), baked in a container with the CUDA
    # toolkit mounted (the image has none); and the debug build.
    "B3": {
        "install": [["cactup", "install", "--thornlist", str(THORNLISTS / "tutorial-gpu.th"), "--alias",
                     "et-gpu", "--symlink-name", "et-gpu", "--silent"]],
        "build": ["cactup", "build", "tutorial-gpu", "--variant", "gpu", "-I", "et-gpu"],
        "root": "et-gpu",
        "config": "tutorial-gpu",
        "kind": "partial",
        "cuda": True,
    },
    "B4": {
        "install": [["cactup", "install", "ET_2026_05_v0", "--silent"]],
        "build": ["cactup", "build", "tutorial-debug", "--debug", "--thornlist", str(THORNLISTS / "tutorial.th"),
                  "-I", "ET_2026_05_v0"],
        "root": "Cactus",
        "config": "tutorial-debug",
        "kind": "partial",
    },
    # Notebook 5: the tutorial thornlist again, built in the machine's
    # `pinned` universe (under taskset), before the notebook edits anything.
    "B5": {
        "install": [["cactup", "install", "ET_2026_05_v0", "--silent"]],
        "build": ["cactup", "build", "tutorial-pinned", "--universe", "pinned", "--thornlist",
                  str(THORNLISTS / "tutorial.th"), "-I", "ET_2026_05_v0"],
        "root": "Cactus",
        "config": "tutorial-pinned",
        "kind": "partial",
    },
    # Notebook 7: the tutorial thornlist plus Silo, HDF5 and
    # TerminationTrigger, for checkpoints and a graceful stop.
    "B6": {
        "install": [["cactup", "install", "ET_2026_05_v0", "--silent"]],
        "build": ["cactup", "build", "tutorial-ckpt", "--thornlist", str(THORNLISTS / "tutorial-ckpt.th"),
                  "-I", "ET_2026_05_v0"],
        "root": "Cactus",
        "config": "tutorial-ckpt",
        "kind": "partial",
    },
}


def log(msg: str) -> None:
    print(f"bake: {msg}", flush=True)


def drop_to_user() -> list[str]:
    """The prefix that runs a command as the tutorial user (when not root, as
    in the tests: as whoever runs this)."""
    return ["setpriv", f"--reuid={USER}", f"--regid={USER}", "--init-groups"] if os.geteuid() == 0 else []


def as_user(cmd: list[str], env: dict | None = None, check: bool = True,
            transcript: Path | None = None) -> int:
    full_env = {
        **{k: v for k, v in os.environ.items() if k.startswith("CACTUP_TUTORIAL_")},
        "HOME": str(HOME),
        "USER": USER,
        "LOGNAME": USER,
        "PATH": f"{HOME}/.cactup/bin:/usr/local/bin:/usr/bin:/bin",
        "LANG": "C.UTF-8",
        "LC_ALL": "C.UTF-8",
        **(env or {}),
    }
    drop = drop_to_user()
    if transcript is None:
        status = subprocess.run([*drop, *cmd], env=full_env, cwd=HOME, stdin=subprocess.DEVNULL).returncode
    else:
        # Shown as it runs, and kept: cactup's whole output, stdout and stderr
        # apart, for comparing with a replayed build.
        proc = subprocess.Popen([*drop, *cmd], env=full_env, cwd=HOME, stdin=subprocess.DEVNULL,
                                stdout=subprocess.PIPE, stderr=subprocess.PIPE)

        def copy(src, name: str, dst) -> None:
            with open(transcript.with_suffix(name), "wb") as f:
                for chunk in iter(lambda: src.read1(65536), b""):
                    f.write(chunk)
                    dst.write(chunk)
                    dst.flush()

        threads = [threading.Thread(target=copy, args=(proc.stdout, ".out", sys.stdout.buffer)),
                   threading.Thread(target=copy, args=(proc.stderr, ".err", sys.stderr.buffer))]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        status = proc.wait()
    if check and status != 0:
        raise SystemExit(f"bake: {' '.join(cmd)} failed ({status})")
    return status


def toolchain() -> str:
    """The toolchain's identity: every package and version in this container."""
    pkgs = subprocess.run(["dpkg-query", "-W"], capture_output=True, text=True, check=True).stdout
    return hashlib.sha256(pkgs.encode()).hexdigest()[:12]


def set_up_home() -> None:
    """A bake-only home: cactup (the current build, as notebook 1 leaves it),
    tips off, and the skeleton's global .cactupignore, so shapes are computed
    as in an attendee's home."""
    shutil.chown(HOME, USER, USER)
    if (HOME / ".cactup" / "bin" / "cactup").exists():
        return
    current = json.loads((UPDATE_ROOT / "latest.json").read_text())["build"]
    binary = next(UPDATE_ROOT.glob(f"*/cactup-{current}"))
    as_user(["mkdir", "-p", f"{HOME}/.cactup/bin"])
    as_user(["cp", str(binary), f"{HOME}/.cactup/bin/cactup"])
    as_user(["cactup", "knob", "wisdom-frequency", "off"])
    as_user(["cp", "/opt/cactup-tutorial/skel/.cactup/cactupignore", f"{HOME}/.cactup/cactupignore"])


def cactus_root(spec: dict) -> Path:
    return HOME / spec["root"]


def remove_config(spec: dict) -> None:
    root = cactus_root(spec).resolve()
    as_user(["rm", "-rf", str(root / "configs" / spec["config"]), str(root / "exe" / f"cactus_{spec['config']}"),
             str(root / "exe" / spec["config"]), str(RECORD)])


def probe(spec: dict) -> dict:
    """Run the bake's build up to the config step, which records the
    fingerprint and stops."""
    remove_config(spec)
    as_user(spec["build"], env={"CACTUP_TUTORIAL_SHIM": "probe", "CACTUP_TUTORIAL_RECORD": str(RECORD)},
            check=False)
    found = sorted(RECORD.glob(f"{spec['config']}/*/fingerprint.json"))
    if not found:
        raise SystemExit("bake: the probe build recorded no fingerprint")
    doc = json.loads(found[-1].read_text())
    remove_config(spec)
    return doc


def record(spec: dict) -> Path:
    started = time.monotonic()
    transcript = Path(f"/tmp/bake-{spec['id']}")
    as_user(spec["build"], env={"CACTUP_TUTORIAL_SHIM": "record", "CACTUP_TUTORIAL_RECORD": str(RECORD)},
            transcript=transcript)
    log(f"built {spec['config']} in {time.monotonic() - started:.0f} s")
    rec = sorted(RECORD.glob(f"{spec['config']}/*/fingerprint.json"))[-1].parent
    for suffix in (".out", ".err"):
        shutil.copyfile(transcript.with_suffix(suffix), rec / f"transcript{suffix}")
    return rec


def keep(spec: dict, rel: Path) -> bool:
    """Whether the bake keeps this top-level entry of configs/NAME.

    Never a dot-entry or a cactup-* file: those are cactup's own. A partial
    bake keeps everything but the objects (build/, lib/ and scratch/ except
    scratch/external, which the executable links against).
    """
    first = rel.parts[0]
    if first.startswith(".") or first.startswith("cactup-"):
        return False
    if spec["kind"] == "full":
        return True
    if first in ("build", "lib"):
        return len(rel.parts) == 1
    if first == "scratch":
        return len(rel.parts) == 1 or rel.parts[1] == "external"
    return True


def harvest(spec: dict, rec: Path, out: Path, cuda: str | None = None) -> None:
    """Copy the config's tree, executables and recordings into `out`, and
    record what a clean prints on it."""
    root = cactus_root(spec).resolve()
    name = spec["config"]
    cfg = root / "configs" / name
    doc = json.loads((rec / "fingerprint.json").read_text())
    tree = out / "tree"
    tree.mkdir(parents=True)
    entries = []
    total = 0
    lo, hi = None, None
    for dirpath, dirnames, filenames in os.walk(cfg):
        here = Path(dirpath)
        rel_dir = here.relative_to(cfg)
        dirnames.sort()
        kept_dirs = []
        for d in dirnames:
            rel = rel_dir / d
            if not keep(spec, rel):
                continue
            src = here / d
            if src.is_symlink():
                filenames.append(d)
                continue
            kept_dirs.append(d)
            st = src.lstat()
            (tree / rel).mkdir()
            shutil.copystat(src, tree / rel)
            entries.append({"path": str(rel), "type": "dir", "mode": st.st_mode & 0o7777, "mtime": st.st_mtime})
        dirnames[:] = kept_dirs
        for f in sorted(filenames):
            rel = rel_dir / f
            if not keep(spec, rel):
                continue
            src = here / f
            st = src.lstat()
            if src.is_symlink():
                target = os.readlink(src)
                os.symlink(target, tree / rel)
                entries.append({"path": str(rel), "type": "link", "target": target, "mtime": st.st_mtime})
            else:
                copy_nofollow(src, tree / rel)
                total += st.st_size
                entries.append({"path": str(rel), "type": "file", "mode": st.st_mode & 0o7777,
                                "mtime": st.st_mtime, "size": st.st_size})
            lo = st.st_mtime if lo is None else min(lo, st.st_mtime)
            hi = st.st_mtime if hi is None else max(hi, st.st_mtime)
    # The build's start, from cactup's record: files older than it (headers
    # an archive unpacked with their own dates) keep their mtimes on restore.
    build_toml = tomllib.loads((rec_attempt(root, name, rec) / "build.toml").read_text())
    started = build_toml["timestamps"]
    start = datetime.fromisoformat(str(started.get("started") or started["created"]).replace("Z", "+00:00"))
    (out / "manifest.json").write_text(json.dumps(
        {"entries": entries, "bytes": total, "mtime-min": lo or 0, "mtime-max": hi or 0,
         "build-start": min(start.timestamp(), hi or start.timestamp())}))
    exe = out / "exe"
    exe.mkdir()
    copy_nofollow(root / "exe" / f"cactus_{name}", exe / f"cactus_{name}")
    utils = root / "exe" / name
    if utils.is_dir() and not utils.is_symlink():
        (exe / name).mkdir()
        for f in sorted(utils.iterdir()):
            copy_nofollow(f, exe / name / f.name)
    for step in ("config", "build", "utils"):
        copy_nofollow(rec / f"{step}.rec", out / f"{step}.rec")
    for name_ in ("transcript.out", "transcript.err"):
        if (rec / name_).exists():
            copy_nofollow(rec / name_, out / name_)
    check_libraries(root, name, cfg)
    attempt = rec_attempt(root, name, rec)
    after = float(os.environ.get("CACTUP_TUTORIAL_INTERRUPT_AFTER", "1.5"))

    def record_interrupt(step: str, goal: list[str]) -> None:
        """What make prints when Ctrl-C stops this step, for a replay stopped
        the same way: a real make, interrupted a moment in. Some steps are over
        in a fraction of a second, so it tries again sooner and sooner until
        the interrupt lands while make still runs."""
        tmp = Path(tempfile.gettempdir()) / f"bake-interrupt-{step}.txt"
        try:
            for delay in (after, after / 5, after / 30, after / 100, after / 300, *[after / 1000] * 6):
                tmp.unlink(missing_ok=True)
                as_user([sys.executable, "-I", __file__, "--interrupt", str(root), str(tmp), str(delay), "-j4",
                         *goal], check=False)
                if tmp.is_file() and tmp.read_text():
                    shutil.copyfile(tmp, out / f"interrupt-{step}.txt")
                    return
            log(f"no interrupt message recorded for {step} (it always finished first)")
        finally:
            tmp.unlink(missing_ok=True)

    # The baked tree is harvested already, so nothing here can lose anything.
    # The utils step first, on the finished tree (on a cleaned one it has
    # nothing to do); then the clean a replayed `--clean` step prints, with
    # its real timing; then the other steps, which on the cleaned tree run
    # long enough to interrupt.
    record_interrupt("utils", [f"{name}-utils"])
    as_user(["sh", "-c", f"cd {root} && CACTUP_TUTORIAL_RECORD={rec.parent.parent} "
             f"python3 -I {make_shim.__file__} --record-clean {name} {rec.name}"])
    copy_nofollow(rec / "clean.rec", out / "clean.rec")
    record_interrupt("build", [name])
    record_interrupt("config", [f"{name}-config", f"options={attempt}/cactup-optionlist.cfg",
                                f"THORNLIST={attempt}/cactup-thornlist.th"])
    record_interrupt("clean", [f"{name}-clean"])

    (out / "bake.json").write_text(json.dumps({
        "format": make_shim.FORMAT,
        # From the harvested copy: the clean above removed the live one.
        "compile-stamp": compile_stamp(tree / "datestamp.o"),
        "id": spec["id"],
        "kind": spec["kind"],
        "cuda": cuda,
        "attempt": str(rec_attempt(root, name, rec)),
        "thornlist-path": build_toml["config-meta"].get("thornlist", ""),
        "fingerprint": doc,
        "baked": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }, indent=1))
    # Readable by everyone: attendees restore from the image's copy.
    subprocess.run(["chmod", "-R", "a+rX,go-w", str(out)], check=True)


def copy_nofollow(src: Path, dst: Path) -> None:
    """Copy a regular file with its mode and times, refusing a symlink: the
    harvest runs as root over files the tutorial user wrote."""
    fd = os.open(src, os.O_RDONLY | os.O_NOFOLLOW)
    with os.fdopen(fd, "rb") as fin:
        st = os.fstat(fin.fileno())
        if not stat.S_ISREG(st.st_mode):
            raise SystemExit(f"bake: {src} is not a regular file")
        with open(dst, "wb") as fout:
            shutil.copyfileobj(fin, fout, 1 << 20)
    os.chmod(dst, st.st_mode & 0o7777)
    os.utime(dst, ns=(st.st_atime_ns, st.st_mtime_ns))


def check_libraries(root: Path, name: str, cfg: Path) -> None:
    """Every library the executable links must resolve, and only to the
    image's own libraries or the config's scratch/external, which the bake
    keeps: nothing a bake container had that an attendee's lacks."""
    # As the tutorial user, like everything else that runs what it built
    # (ldd may run the executable's own interpreter).
    result = subprocess.run([*drop_to_user(), "ldd", str(root / "exe" / f"cactus_{name}")],
                            capture_output=True, text=True)
    out = result.stdout + result.stderr
    if "not a dynamic executable" in out:
        return
    if result.returncode != 0:
        raise SystemExit(f"bake: ldd failed on the executable:\n{out}")
    allowed = ("/lib/", "/lib64/", "/usr/lib/", "/usr/lib64/", f"{cfg}/scratch/external/")
    bad = []
    for line in out.splitlines():
        line = line.strip()
        if "not found" in line:
            bad.append(line)
        elif "=>" in line:
            path = line.split("=>", 1)[1].strip().split(" (", 1)[0]
            if path and not path.startswith(allowed):
                bad.append(line)
    if bad:
        raise SystemExit("bake: the executable links libraries a restore wouldn't have:\n  " + "\n  ".join(bad))


def compile_stamp(datestamp: Path) -> dict:
    """The __DATE__ and __TIME__ Cactus compiled into datestamp.o (and the
    executable's banner), if there is exactly one of each."""
    try:
        data = datestamp.read_bytes()
    except OSError:
        return {}
    dates = set(re.findall(rb"(?<![\w ])([A-Z][a-z]{2} [ 0-3]\d \d{4})\0", data))
    times = set(re.findall(rb"(?<![\w:])(\d\d:\d\d:\d\d)\0", data))
    # CCTK_COMPILE_DATETIME, the flesh makefile's ISO date and time.
    stamps = set(re.findall(rb"(?<![\w-])(\d{4}-\d\d-\d\dT[0-9T:+-]+)\0", data))
    if len(dates) != 1 or len(times) != 1:
        return {}
    out = {"date": dates.pop().decode(), "time": times.pop().decode()}
    if len(stamps) == 1:
        out["datetime"] = stamps.pop().decode()
    return out


def rec_attempt(root: Path, name: str, rec: Path) -> Path:
    return root / "configs" / name / ".cactup-builds" / rec.name


def interrupt(root: str, out: str, delay: float, args: list[str]) -> int:
    """Run a real make, stop it as Ctrl-C would (SIGINT to its whole process
    group) a moment in, and keep make's messages about the interrupt."""
    proc = subprocess.Popen(["make", *args], executable=make_shim.REAL_MAKE, cwd=root, stdin=subprocess.PIPE,
                            stdout=subprocess.DEVNULL, stderr=subprocess.PIPE, start_new_session=True)
    proc.stdin.write(b"yes\n")
    proc.stdin.close()
    time.sleep(delay)
    try:
        os.killpg(proc.pid, signal.SIGINT)
    except ProcessLookupError:
        pass
    err = proc.stderr.read().decode()
    proc.wait()
    lines = [line for line in err.splitlines(keepends=True)
             if re.match(r"make(\[\d+\])?: \*\*\* ", line) and ("Interrupt" in line or "Deleting" in line)]
    Path(out).write_text("".join(lines))
    return 0


def cuda_toolkit(root: Path = Path("/usr/local")) -> Path | None:
    """The CUDA toolkit mounted into this bake container, if any."""
    found = sorted(root.glob("cuda-*/version.json"))
    return found[0].parent if found else None


def cuda_version(root: Path = Path("/usr/local")) -> str | None:
    """Its version, which a GPU bake records: a bake made with another is
    baked again."""
    toolkit = cuda_toolkit(root)
    for path in [toolkit / "version.json"] if toolkit else []:
        try:
            return json.loads(path.read_text())["cuda"]["version"]
        except (OSError, ValueError, KeyError):
            continue
    return None


def use_cuda() -> str:
    """Point /usr/local/cuda, where the gpu optionlist looks, at the mounted
    toolkit; its version."""
    version = cuda_version()
    if version is None:
        raise SystemExit("bake: a GPU bake needs the CUDA toolkit mounted (see build.sh)")
    link = Path("/usr/local/cuda")
    if not link.exists():
        link.symlink_to(cuda_toolkit().name)
    return version


def cached_format(bake: Path) -> bool:
    try:
        return json.loads((bake / "bake.json").read_text()).get("format") == make_shim.FORMAT
    except (OSError, ValueError):
        return False


def cached(bake: Path, cuda: str | None) -> bool:
    try:
        info = json.loads((bake / "bake.json").read_text())
    except (OSError, ValueError):
        return False
    return info.get("format") == make_shim.FORMAT and info.get("cuda") == cuda


def main() -> int:
    if sys.argv[1:2] == ["--interrupt"]:
        return interrupt(sys.argv[2], sys.argv[3], float(sys.argv[4]), sys.argv[5:])
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("bakes", nargs="+", choices=sorted(BAKES))
    parser.add_argument("--cache", default="/bakes")
    parser.add_argument("--wanted", default="wanted",
                        help="file (in the toolchain's cache directory) listing the fingerprints the image needs")
    args = parser.parse_args()
    # The `pinned` universe's taskset reads the container's CPUs from here;
    # the entrypoint exports it, but a bake container doesn't run that.
    if "CACTUP_TUTORIAL_CPUS" not in os.environ:
        status = Path("/proc/self/status").read_text()
        os.environ["CACTUP_TUTORIAL_CPUS"] = re.search(r"^Cpus_allowed_list:\s*(\S+)", status, re.M).group(1)
    set_up_home()
    tool = toolchain()
    cache = Path(args.cache) / tool
    cache.mkdir(parents=True, exist_ok=True)
    wanted = []
    installed: set[str] = set()
    order = list(BAKES)
    if [b for b in order if b in args.bakes] != args.bakes:
        raise SystemExit(f"bake: bakes run in the notebooks' order: {' '.join(order)}")
    for bake_id in args.bakes:
        spec = {**BAKES[bake_id], "id": bake_id}
        cuda = use_cuda() if spec.get("cuda") else None
        for cmd in spec["install"] + spec.get("prepare", []):
            key = json.dumps(cmd)
            if key not in installed:
                as_user(cmd)
                installed.add(key)
        fp = make_shim.fingerprint(probe(spec))
        wanted.append(fp)
        if cached(cache / fp, cuda):
            log(f"{bake_id} ({fp}) is cached for toolchain {tool}")
            continue
        if (cache / fp).exists():
            why = "made with another CUDA toolkit" if cuda and cached_format(cache / fp) else "of an older format"
            log(f"{bake_id} ({fp}): the cached bake is {why}; baking again")
            shutil.rmtree(cache / fp)
        log(f"{bake_id} ({fp}): building")
        rec = record(spec)
        tmp = cache / f".{fp}.tmp"
        shutil.rmtree(tmp, ignore_errors=True)
        harvest(spec, rec, tmp, cuda)
        os.rename(tmp, cache / fp)
        log(f"{bake_id} ({fp}) baked")
    (cache / args.wanted).write_text("".join(f"{fp}\n" for fp in wanted))
    (Path(args.cache) / "last-toolchain").write_text(f"{tool}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
