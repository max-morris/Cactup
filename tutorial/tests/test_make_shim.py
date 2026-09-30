"""The make shim and the bake harvest, against a fake make and a fake Cactus
tree laid out the way cactup lays out a build attempt."""

from __future__ import annotations

import calendar
import json
import os
import re
import signal
import subprocess
import sys
import textwrap
import time
from pathlib import Path

import pytest

HERE = Path(__file__).resolve().parent
SHIM_DIR = HERE.parent / "image" / "rootfs" / "usr" / "local" / "lib" / "cactup-tutorial"
sys.path.insert(0, str(SHIM_DIR))
sys.path.insert(0, str(HERE.parent / "bake"))

import bake  # noqa: E402
import make_shim  # noqa: E402

NAME = "demo"

FAKE_MAKE = r'''#!/usr/bin/env python3
"""A stand-in for /usr/bin/make in a Cactus tree: a few lines of output per
step, with pauses, and the files each step makes."""
import os, signal, sys, time
from pathlib import Path
def interrupted(*_):
    sys.stderr.write("make: *** [Makefile:99: " + goal + "] Interrupt\n")
    sys.exit(130)
log = Path(os.environ["CACTUP_TUTORIAL_FAKE_MAKE_LOG"])
with log.open("a") as f:
    f.write(" ".join(sys.argv[1:]) + "\n")
goal = [a for a in sys.argv[1:] if not a.startswith("-") and "=" not in a][0]
opts = [a for a in sys.argv[1:] if a.startswith("options=")]
name = goal.split("-")[0]
signal.signal(signal.SIGINT, interrupted)
cfg = Path("configs") / name
pause = float(os.environ.get("CACTUP_TUTORIAL_FAKE_MAKE_PAUSE", "0.1"))
def say(line, err=False):
    (sys.stderr if err else sys.stdout).write(line + "\n")
    (sys.stderr if err else sys.stdout).flush()
    time.sleep(pause)
if goal.endswith("-config"):
    say("Configuring with " + opts[0])
    say(f"  Setting auth to 'fifo:/tmp/GMfifo{os.getpid()} --'")
    cfg.mkdir(parents=True, exist_ok=True)
    (cfg / "config-info").write_text(
        "# CONFIG-DATE    : " + time.strftime("%a %b %e %H:%M:%S %Y", time.gmtime()) + " (GMT)\n"
        + "# CONFIG-OPTIONS :\n" + opts[0] + f"\nauth=fifo:/tmp/GMfifo{os.getpid()} --\n")
    (cfg / "config-data").mkdir(parents=True, exist_ok=True)
    (cfg / "config-data" / "make.config.defn").write_text("defn\n")
    (cfg / "bindings").mkdir(exist_ok=True)
    (cfg / "bindings" / "b.h").write_text("b\n")
    say("warning: something configure-ish", err=True)
    say("Done configuring")
elif goal.endswith("-realclean"):
    import shutil
    for d in ("build", "lib", "scratch", "bindings"):
        shutil.rmtree(cfg / d, ignore_errors=True)
    say("Deleting all object files")
elif goal.endswith("-clean"):
    import shutil
    shutil.rmtree(cfg / "build", ignore_errors=True)
    # Cactus's clean removes every *.o under the config, datestamp.o included.
    (cfg / "datestamp.o").unlink(missing_ok=True)
    say("Cleaning " + name)
elif goal.endswith("-utils"):
    (Path("exe") / name).mkdir(parents=True, exist_ok=True)
    (Path("exe") / name / "util").write_text("#!/bin/sh\necho util\n")
    os.chmod(Path("exe") / name / "util", 0o755)
    say("Building utilities for " + name)
else:
    for thorn in ("A", "B"):
        d = cfg / "build" / thorn
        d.mkdir(parents=True, exist_ok=True)
        src = Path("arrangements") / thorn / "x.c"
        if not (d / "x.o").exists() or src.stat().st_mtime > (d / "x.o").stat().st_mtime:
            say(f"COMPILING {thorn}/x.c")
            (d / "x.o").write_text(src.read_text())
    (cfg / "lib").mkdir(exist_ok=True)
    (cfg / "lib" / "libthorns.a").write_text("lib\n")
    (cfg / "scratch" / "external").mkdir(parents=True, exist_ok=True)
    (cfg / "scratch" / "external" / "libext.so").write_text("ext\n")
    (cfg / "scratch" / "done").mkdir(exist_ok=True)
    (cfg / "scratch" / "done" / "Ext").write_text(time.strftime("%a %b %e %H:%M:%S UTC %Y\n", time.gmtime()))
    # A header unpacked from an archive with its own, older date.
    (cfg / "scratch" / "external" / "old.h").write_text("old\n")
    os.utime(cfg / "scratch" / "external" / "old.h", (1634639336, 1634639336))
    # The shape Cactus's makefile really gives CCTK_COMPILE_DATETIME.
    stamp = time.strftime("%b %e %Y\0%H:%M:%S\0%Y-%m-%dT%H:%M0T%H:%M:%S%z\0").encode()
    (cfg / "datestamp.o").write_bytes(b"\x7fELF..." + stamp)
    Path("exe").mkdir(exist_ok=True)
    exe = Path("exe") / f"cactus_{name}"
    tmp = exe.with_suffix(".new")
    tmp.write_bytes(b"#!/bin/sh\necho cactus " + name.encode() + b"\n# " + stamp + b"\n")
    os.chmod(tmp, 0o755)
    os.replace(tmp, exe)
    say("Linking " + str(exe))
'''


class Tree:
    """A fake Cactus root with the attempt directories cactup would write."""

    def __init__(self, tmp: Path):
        self.tmp = tmp
        self.root = tmp / "Cactus"
        for thorn in ("A", "B"):
            (self.root / "arrangements" / thorn).mkdir(parents=True)
            (self.root / "arrangements" / thorn / "x.c").write_text(f"int {thorn.lower()};\n")
        self.cfg = self.root / "configs" / NAME
        self.bin = tmp / "bin"
        self.bin.mkdir()
        fake = tmp / "fake-make"
        fake.write_text(FAKE_MAKE)
        fake.chmod(0o755)
        (self.bin / "make").write_text(f'#!/bin/sh\nexec {sys.executable} -I {SHIM_DIR / "make_shim.py"} "$@"\n')
        (self.bin / "make").chmod(0o755)
        self.bakes = tmp / "bakes"
        self.bakes.mkdir()
        self.log = tmp / "make.log"
        self.env = {
            **os.environ,
            "PATH": f"{self.bin}:{os.environ['PATH']}",
            "CACTUP_TUTORIAL_REAL_MAKE": str(fake),
            "CACTUP_TUTORIAL_BAKES": str(self.bakes),
            "CACTUP_TUTORIAL_BUILD_SECONDS": "1.5",
            "CACTUP_TUTORIAL_FAKE_MAKE_LOG": str(self.log),
        }
        self.env.pop("MAKELEVEL", None)
        self.next_id = 0
        self.sources = {"repo": "abc123"}
        self.thornlist = "# a comment\n!TARGET = $ARR\n!URL = u\nA/A\nB/B\n"

    def attempt(self, full: bool = True, clean: bool = False, realclean: bool = True) -> Path:
        """Prepare a build attempt as cactup's `prepare` does."""
        att = self.cfg / ".cactup-builds" / f"{self.next_id:04d}"
        self.next_id += 1
        att.mkdir(parents=True)
        (att / "cactup-optionlist.cfg").write_text(f"CC = gcc\n# staged in {att}\n")
        (att / "cactup-thornlist.th").write_text(self.thornlist)
        shapes = {t: str(sorted(p.name for p in (self.root / "arrangements" / t).iterdir()))
                  for t in ("A", "B")}
        (att / "build.toml").write_text(textwrap.dedent(f"""\
            config = "{NAME}"
            machine = "m"
            cactus-root = "{self.root}"
            full-rebuild = {"true" if full else "false"}
            build-env = ""

            [config-meta]
            name = "{NAME}"

            [config-meta.flags]
            debug = false

            [config-meta.sources]
            {chr(10).join(f'{k} = "{v}"' for k, v in self.sources.items())}

            [config-meta.thorn-shapes]
            A = "{shapes['A']}"
            B = "{shapes['B']}"

            [config-meta.thorn-providers]
            A = "arrangements/A"
            B = "arrangements/B"

            [timestamps]
            started = "{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())}"
            """))
        steps = []
        if full and realclean and (self.cfg / "config-data").exists():
            steps.append(f"make -j4 {NAME}-realclean")
        steps.append(f"echo yes | make -j4 {NAME}-config options='{att}/cactup-optionlist.cfg' "
                     f"THORNLIST='{att}/cactup-thornlist.th'")
        if clean:
            steps.append(f"make -j4 {NAME}-clean")
        steps += [f"make -j4 {NAME}", f"make -j4 {NAME}-utils"]
        script = att / "build-script"
        script.write_text(f"#!/bin/sh\nset -e\ncd '{self.root}'\n" + "\n".join(steps) + "\n")
        script.chmod(0o755)
        return att

    def run(self, att: Path, **env) -> subprocess.CompletedProcess:
        return subprocess.run(["/bin/sh", "-c", f"'{att / 'build-script'}'"], env={**self.env, **env},
                              capture_output=True, cwd=self.root)

    def make_calls(self) -> list[str]:
        calls = self.log.read_text().splitlines() if self.log.exists() else []
        self.log.unlink(missing_ok=True)
        return calls

    def bake(self) -> tuple[Path, subprocess.CompletedProcess]:
        """Record a from-scratch build and harvest it, as the bake container does."""
        att = self.attempt()
        record = self.tmp / "record"
        rec = self.run(att, CACTUP_TUTORIAL_SHIM="record", CACTUP_TUTORIAL_RECORD=str(record))
        assert rec.returncode == 0, rec.stderr
        spec = {"id": "B1", "root": str(self.root), "config": NAME, "kind": "full"}
        out = self.bakes / "tmp"
        bake.HOME = self.tmp
        saved = dict(os.environ)
        os.environ.update({k: v for k, v in self.env.items() if k.startswith("CACTUP_TUTORIAL_")})
        # The harvest's interrupts land while each step's make still runs.
        os.environ["CACTUP_TUTORIAL_FAKE_MAKE_PAUSE"] = "0.4"
        os.environ["CACTUP_TUTORIAL_INTERRUPT_AFTER"] = "0.2"
        try:
            bake.harvest(spec, record / NAME / att.name, out)
        finally:
            os.environ.clear()
            os.environ.update(saved)
        fp = make_shim.fingerprint(json.loads((out / "bake.json").read_text())["fingerprint"])
        os.rename(out, self.bakes / fp)
        self.make_calls()
        return self.bakes / fp, rec

    def wipe(self) -> None:
        """What a new attendee has: no config, no executable."""
        subprocess.run(["rm", "-rf", str(self.cfg), str(self.root / "exe")], check=True)
        self.next_id = 0


@pytest.fixture
def tree(tmp_path):
    return Tree(tmp_path)


def staging_dirs(cfg: Path) -> list[str]:
    return [p.name for p in cfg.iterdir() if p.name.startswith((make_shim.STAGING, make_shim.TRASH))]


def wait_no_staging(cfg: Path, seconds: float = 5) -> bool:
    deadline = time.time() + seconds
    while staging_dirs(cfg) and time.time() < deadline:
        time.sleep(0.05)
    return not staging_dirs(cfg)


# -- pure parts ------------------------------------------------------------------


def test_targets_skips_options_and_assignments():
    assert make_shim.targets(["-j4", "demo-config", "options=/x", "THORNLIST=/y"]) == ["demo-config"]
    assert make_shim.targets(["-j", "4", "demo"]) == ["demo"]
    assert make_shim.targets(["-C", "dir", "all"]) == ["all"]


def test_step_of():
    assert [make_shim.step_of(t, "demo") for t in
            ("demo-config", "demo", "demo-utils", "demo-clean", "demo-realclean", "other")] == \
        ["config", "build", "utils", "clean", "realclean", None]


def test_thornlist_normalization_ignores_comments_whitespace_and_order():
    a = "!CRL_VERSION = 1.0\n# comment\n!TARGET = $ARR\n!URL   = u  # trailing\nX/A\nX/B\n"
    b = "!CRL_VERSION=1.0\n\n!TARGET=$ARR\n!URL = u\n# X/C disabled\nX/B\n   X/A\n"
    assert make_shim.normalize_thornlist(a) == make_shim.normalize_thornlist(b)
    c = a + "X/C\n"
    assert make_shim.normalize_thornlist(a) != make_shim.normalize_thornlist(c)
    d = a.replace("!URL   = u", "!URL = v")
    assert make_shim.normalize_thornlist(a) != make_shim.normalize_thornlist(d)


def test_schedule_compresses_to_target_and_caps_silences():
    # Hundreds of lines, with a long silence (a big file compiling) in both.
    config = [(t / 10, 1, b"x\n") for t in range(1, 100)] + [(400.0, 1, b"x\n")]
    build = [(t, 2, b"y\n") for t in range(1, 600)] + [(1250.0, 1, b"z\n")]
    plan = make_shim.schedule([("config", config, 401), ("build", build, 1300)], 30)
    times, end = plan["build"]
    assert abs(end - 30) < 0.5
    flat = plan["config"][0] + [plan["config"][1]] + times + [end]
    gaps = [b - a for a, b in zip([0.0] + flat, flat)]
    assert max(gaps) <= make_shim.MAX_GAP + 1e-9
    assert flat == sorted(flat)


def test_mtime_mapping_is_monotonic_and_in_range():
    lo, hi = 1000.0, 5000.0
    mapped = [make_shim.map_mtime(t, lo, hi, 10.0, 40.0) for t in (1000.0, 2000.0, 4999.0, 5000.0)]
    assert mapped == sorted(mapped) and mapped[0] == 10.0 and mapped[-1] == 40.0


# -- record, harvest, replay -----------------------------------------------------


def test_a_bake_replays_exactly_and_restores_the_tree(tree):
    bake_dir, recorded = tree.bake()
    assert (bake_dir / "tree" / "build" / "A" / "x.o").exists()
    assert not (bake_dir / "tree" / ".cactup-builds").exists()
    assert (bake_dir / "clean.rec").exists()
    for step in ("config", "build", "utils", "clean"):
        assert "Interrupt" in (bake_dir / f"interrupt-{step}.txt").read_text(), step
    tree.wipe()

    att = tree.attempt()
    started = time.monotonic()
    got = tree.run(att)
    elapsed = time.monotonic() - started
    assert got.returncode == 0, got.stderr
    assert tree.make_calls() == [], "a hit runs no real make"
    # The same output, with the bake's attempt directory rewritten to this
    # one's, and make's jobserver fifo named after this make.
    fifo = re.compile(rb"GMfifo\d+")
    assert fifo.sub(b"GMfifo", got.stdout) == fifo.sub(b"GMfifo", recorded.stdout)
    assert got.stderr == recorded.stderr
    assert fifo.search(got.stdout).group() != fifo.search(recorded.stdout).group()
    assert 1.0 < elapsed < 6
    assert (tree.cfg / "build" / "B" / "x.o").read_text() == "int b;\n"
    assert (tree.root / "exe" / f"cactus_{NAME}").stat().st_mode & 0o111
    assert (tree.root / "exe" / NAME / "util").exists()
    assert json.loads((tree.cfg / make_shim.MARKER).read_text())["state"] == "pristine"
    assert not (tree.cfg / make_shim.SESSION).exists()
    assert wait_no_staging(tree.cfg)
    # The banner's compile date and time are this build's, not the bake's.
    exe_bytes = (tree.root / "exe" / f"cactus_{NAME}").read_bytes()
    baked = json.loads((bake_dir / "bake.json").read_text())["compile-stamp"]
    assert baked["time"].encode() not in exe_bytes or baked["time"] == time.strftime("%H:%M:%S")
    assert set(baked) == {"date", "time", "datetime"}, baked
    assert baked["datetime"].encode() not in exe_bytes or baked["time"] == time.strftime("%H:%M:%S")
    assert time.strftime("%b %e %Y").encode() in exe_bytes
    assert time.strftime("%b %e %Y").encode() in (tree.cfg / "datestamp.o").read_bytes()
    # A file older than the bake's build keeps its date; the rest spread out
    # over the replay instead of piling onto one instant.
    assert (tree.cfg / "scratch" / "external" / "old.h").stat().st_mtime == 1634639336
    assert (tree.cfg / "build" / "A" / "x.o").stat().st_mtime < (tree.cfg / "build" / "B" / "x.o").stat().st_mtime
    # Objects are newer than the sources, and the executable newest of all.
    obj = (tree.cfg / "build" / "A" / "x.o").stat().st_mtime
    assert obj >= (tree.root / "arrangements" / "A" / "x.c").stat().st_mtime
    assert (tree.root / "exe" / f"cactus_{NAME}").stat().st_mtime >= obj


def test_rewrites_the_attempt_directory_in_replayed_output(tree):
    tree.bake()
    tree.wipe()
    tree.next_id = 3
    att = tree.attempt()
    got = tree.run(att)
    assert got.returncode == 0, got.stderr
    assert f"{att}/cactup-optionlist.cfg" in got.stdout.decode()
    assert "/0000/" not in got.stdout.decode()
    # The configure summary and the libraries' stamps are this build's too.
    info = (tree.cfg / "config-info").read_text()
    assert f"{att}/cactup-optionlist.cfg" in info and "/0000/" not in info
    assert re.search(r"GMfifo\d+", info).group() == re.search(r"GMfifo\d+", got.stdout.decode()).group()
    for text in (info, (tree.cfg / "scratch" / "done" / "Ext").read_text()):
        m = make_shim.DATE.search(text)
        when = calendar.timegm(time.strptime(f"{m['day']} {m['year']}", "%a %b %d %H:%M:%S %Y"))
        assert abs(when - time.time()) < 30, text


def test_an_incremental_build_after_a_restore_is_real_and_small(tree):
    tree.bake()
    tree.wipe()
    assert tree.run(tree.attempt()).returncode == 0
    tree.make_calls()
    # Edit a thorn: the repository is modified, so no bake matches.
    time.sleep(0.02)
    (tree.root / "arrangements" / "A" / "x.c").write_text("int a2;\n")
    tree.sources = {"repo": "abc123+1mod@1"}
    got = tree.run(tree.attempt(full=False))
    assert got.returncode == 0, got.stderr
    assert "not precomputed" not in got.stderr.decode()
    assert "COMPILING A/x.c" in got.stdout.decode()
    assert "COMPILING B/x.c" not in got.stdout.decode()
    assert any(c.endswith(f"{NAME}") for c in tree.make_calls())
    assert json.loads((tree.cfg / make_shim.MARKER).read_text())["state"] == "built-on"


def test_a_from_scratch_miss_says_what_differs(tree):
    tree.bake()
    tree.wipe()
    tree.sources = {"repo": "abc123+1mod@1"}
    (tree.root / "arrangements" / "B" / "extra.orig").write_text("")
    got = tree.run(tree.attempt())
    assert got.returncode == 0
    err = got.stderr.decode()
    assert "not precomputed" in err and "repo" in err and "        B" in err
    assert tree.make_calls(), "a miss runs the real make"


def test_a_pristine_tree_that_is_up_to_date_passes_through(tree):
    tree.bake()
    tree.wipe()
    assert tree.run(tree.attempt()).returncode == 0
    tree.make_calls()
    got = tree.run(tree.attempt(full=False))
    assert got.returncode == 0
    assert len(tree.make_calls()) == 3, "config, build and utils run for real"
    assert "COMPILING" not in got.stdout.decode()


def test_a_clean_build_replays_the_recorded_clean(tree):
    tree.bake()
    tree.wipe()
    assert tree.run(tree.attempt()).returncode == 0
    tree.make_calls()
    got = tree.run(tree.attempt(full=False, clean=True))
    assert got.returncode == 0, got.stderr
    assert f"Cleaning {NAME}" in got.stdout.decode()
    assert tree.make_calls() == []
    assert (tree.cfg / "build" / "A" / "x.o").exists()


def test_a_real_clean_removes_the_marker(tree):
    tree.bake()
    tree.wipe()
    assert tree.run(tree.attempt()).returncode == 0
    subprocess.run(["make", f"{NAME}-clean"], cwd=tree.root, env=tree.env, check=True, capture_output=True)
    assert not (tree.cfg / make_shim.MARKER).exists()


def test_the_executable_is_replaced_not_overwritten(tree):
    tree.bake()
    tree.wipe()
    assert tree.run(tree.attempt()).returncode == 0
    exe = tree.root / "exe" / f"cactus_{NAME}"
    link = tree.tmp / "sim-copy"
    os.link(exe, link)
    subprocess.run(["rm", "-rf", str(tree.cfg / "build")], check=True)
    assert tree.run(tree.attempt()).returncode == 0
    assert os.stat(link).st_ino != exe.stat().st_ino
    assert link.read_text().startswith("#!/bin/sh")


def test_an_interrupted_replay_leaves_the_tree_alone_and_cleans_up(tree):
    tree.bake()
    tree.wipe()
    att = tree.attempt()
    proc = subprocess.Popen(["/bin/sh", "-c", f"'{att / 'build-script'}'"], env={
        **tree.env, "CACTUP_TUTORIAL_BUILD_SECONDS": "20"}, cwd=tree.root,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)
    time.sleep(1.5)
    os.killpg(proc.pid, signal.SIGINT)
    out, err = proc.communicate(timeout=10)
    assert proc.returncode != 0
    # make's own message, as the bake recorded it by interrupting a real make.
    assert b"make: *** [Makefile:99: demo] Interrupt" in err or \
        f"make: *** [Makefile:99: {NAME}-config] Interrupt".encode() in err
    assert not (tree.cfg / "build" / "A" / "x.o").exists()
    assert wait_no_staging(tree.cfg)
    # And the next attempt restores normally.
    got = tree.run(tree.attempt())
    assert got.returncode == 0, got.stderr
    assert (tree.cfg / "build" / "A" / "x.o").exists()


def test_a_make_outside_a_build_script_just_runs(tree):
    (tree.cfg).mkdir(parents=True)
    got = subprocess.run(["make", "-j2", f"{NAME}-config", "options=/x"], cwd=tree.root, env=tree.env,
                         capture_output=True)
    assert got.returncode == 0
    assert tree.make_calls() == [f"-j2 {NAME}-config options=/x"]


def replay_in_background(tree: Tree, att: Path, seconds: str) -> subprocess.Popen:
    return subprocess.Popen(["/bin/sh", "-c", f"'{att / 'build-script'}'"], env={
        **tree.env, "CACTUP_TUTORIAL_BUILD_SECONDS": seconds}, cwd=tree.root,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, start_new_session=True)


def copier_of(tree: Tree) -> list[int]:
    for _ in range(100):
        try:
            return json.loads((tree.cfg / make_shim.SESSION).read_text())["copier"]
        except (OSError, ValueError, KeyError):
            time.sleep(0.05)
    raise AssertionError("no session")


def test_a_stale_copier_cannot_touch_the_next_attempts_staging(tree):
    bake_dir, _ = tree.bake()
    tree.wipe()
    # A file the copier blocks on, as on a very slow disk.
    fifo = bake_dir / "tree" / "config-data" / "slow"
    os.mkfifo(fifo)
    manifest = json.loads((bake_dir / "manifest.json").read_text())
    at = next(i for i, e in enumerate(manifest["entries"]) if e["path"] == "config-data") + 1
    manifest["entries"].insert(at, {"path": "config-data/slow", "type": "file", "mode": 0o644,
                                    "mtime": manifest["mtime-max"], "size": 0})
    (bake_dir / "manifest.json").write_text(json.dumps(manifest))
    first = replay_in_background(tree, tree.attempt(), "20")
    stale = copier_of(tree)
    time.sleep(0.5)
    os.killpg(first.pid, signal.SIGINT)
    first.communicate(timeout=10)
    assert make_shim.alive(stale), "the copier is still blocked"
    # The disk is fast again; the attendee builds again at once.
    fifo.unlink()
    fifo.write_text("")
    got = tree.run(tree.attempt())
    assert got.returncode == 0, got.stderr
    assert not make_shim.alive(stale), "the next config step stops the old copier"
    assert (tree.cfg / "build" / "A" / "x.o").exists()
    assert wait_no_staging(tree.cfg)


def test_a_copier_that_dies_fails_the_build_instead_of_hanging(tree):
    bake_dir, _ = tree.bake()
    tree.wipe()
    fifo = bake_dir / "tree" / "config-data" / "slow"
    os.mkfifo(fifo)
    manifest = json.loads((bake_dir / "manifest.json").read_text())
    at = next(i for i, e in enumerate(manifest["entries"]) if e["path"] == "config-data") + 1
    manifest["entries"].insert(at, {"path": "config-data/slow", "type": "file", "mode": 0o644,
                                    "mtime": manifest["mtime-max"], "size": 0})
    (bake_dir / "manifest.json").write_text(json.dumps(manifest))
    # A long config step, so the copier dies while its parent (the config
    # step's shim) is still running and hasn't reaped it: a zombie.
    proc = replay_in_background(tree, tree.attempt(), "20")
    os.kill(copier_of(tree)[0], signal.SIGKILL)
    out, err = proc.communicate(timeout=30)
    assert proc.returncode != 0
    assert b"the copy stopped" in err
    assert wait_no_staging(tree.cfg)


def test_a_failed_copy_says_why_and_leaves_no_staging(tree):
    bake_dir, _ = tree.bake()
    tree.wipe()
    (bake_dir / "tree" / "build" / "B" / "x.o").chmod(0)
    got = tree.run(tree.attempt())
    assert got.returncode != 0
    assert b"Permission denied" in got.stderr, got.stderr
    assert wait_no_staging(tree.cfg)
    assert not (tree.cfg / "build" / "A" / "x.o").exists(), "nothing was swapped in"


def test_a_missing_executable_on_a_pristine_tree_is_relinked_for_real(tree):
    tree.bake()
    tree.wipe()
    assert tree.run(tree.attempt()).returncode == 0
    tree.make_calls()
    (tree.root / "exe" / f"cactus_{NAME}").unlink()
    # cactup calls the config incomplete: a full rebuild, but no realclean.
    got = tree.run(tree.attempt(full=True, realclean=False))
    assert got.returncode == 0, got.stderr
    assert any(c.endswith(f" {NAME}") for c in tree.make_calls()), "a real make relinks"
    assert (tree.root / "exe" / f"cactus_{NAME}").exists()


def test_stamp_swaps_rewrite_every_piece_once():
    baked = {"date": "Sep 30 2026", "time": "14:38:03", "datetime": "2026-09-30T14:380T14:38:03+0000"}
    # 22:14:38 on another day: a naive chained replace would corrupt it, since
    # the new clock contains the old minute.
    when = time.mktime((2026, 10, 1, 22, 14, 38, 0, 0, -1))
    swaps = make_shim.stamp_swaps(baked, when)
    assert swaps[b"Sep 30 2026"] == b"Oct  1 2026"
    assert swaps[b"14:38:03"] == b"22:14:38"
    # The digit before the second T is the date's last (two overlapping stores).
    assert swaps[b"2026-09-30T14:380T14:38:03+0000"] == b"2026-10-01T22:141T22:14:38+0000"
    import io
    out = io.BytesIO()
    make_shim.stream_replace(io.BytesIO(b"x\0" + b"\0".join(swaps) + b"\0"), out, swaps, lambda n: None)
    assert out.getvalue() == b"x\0" + b"\0".join(swaps.values()) + b"\0"


def test_a_miss_with_another_thornlist_says_so_rather_than_list_edits(tree):
    tree.bake()
    tree.wipe()
    tree.thornlist += "!TARGET = $ARR\n!URL = other\nC/C\n"
    tree.sources = {"repo": "abc123", "other": "def456"}
    got = tree.run(tree.attempt())
    err = got.stderr.decode()
    assert "not precomputed" in err and "Its thornlist is not the one" in err, err
    assert "Reverting" not in err and "other" not in err, err


def test_a_passed_through_make_runs_as_make_with_default_signals(monkeypatch):
    calls = []

    def execv(path, argv):
        calls.append((path, argv, signal.getsignal(signal.SIGPIPE), signal.getsignal(signal.SIGXFSZ)))

    monkeypatch.setattr(os, "execv", execv)
    saved = {s: signal.getsignal(s) for s in (signal.SIGPIPE, signal.SIGXFSZ, signal.SIGINT)}
    signal.signal(signal.SIGPIPE, signal.SIG_IGN)
    try:
        make_shim.exec_make(["-j4", "demo"])
    finally:
        for s, h in saved.items():
            signal.signal(s, h)
    # make and its recipes see SIGPIPE and SIGXFSZ as any program would, not
    # as Python leaves them (ignored).
    assert calls == [(make_shim.REAL_MAKE, ["make", "-j4", "demo"], signal.SIG_DFL, signal.SIG_DFL)]


def test_a_recipe_through_the_shim_gets_sigpipe(tree):
    # A real make in the recipe's place: `yes | head -1` is quiet only when
    # yes dies of SIGPIPE, as it does under a plain make.
    fake = tree.tmp / "fake-make"
    fake.write_text("#!/bin/sh\nyes | head -1\n")
    got = subprocess.run(["make", "anything"], cwd=tree.root, env=tree.env, capture_output=True)
    assert got.stdout == b"y\n"
    assert b"Broken pipe" not in got.stderr, got.stderr


def test_the_copy_counts_as_done_only_when_the_copier_says_so(tree, monkeypatch):
    bake_dir, _ = tree.bake()
    monkeypatch.setattr(make_shim, "BAKES", tree.bakes)
    tree.wipe()
    staging = tree.cfg / ".make-staging-x"
    staging.mkdir(parents=True)
    session = {"bake": bake_dir.name, "restore": True, "staging": staging.name,
               "copier": [os.getpid(), make_shim.start_time(os.getpid())]}
    replay = make_shim.Replay(tree.cfg, session)
    make_shim.write_json(staging / "progress", {"state": "copying", "done": 10, "total": 10, "error": ""})
    assert replay.copy_fraction() < 1.0, "every byte counted, but the last mode not yet set"
    make_shim.write_json(staging / "progress", {"state": "done", "done": 10, "total": 10, "error": ""})
    assert replay.copy_fraction() == 1.0


def test_stream_replace_counts_what_it_has_written():
    import io
    out = io.BytesIO()
    seen = []
    make_shim.stream_replace(io.BytesIO(b"x" * (3 << 20)), out, {b"ab": b"cd"},
                             lambda n: seen.append((n, out.tell())))
    assert all(written >= sum(k for k, _ in seen[: i + 1]) - 1 for i, (_, written) in enumerate(seen)), seen
