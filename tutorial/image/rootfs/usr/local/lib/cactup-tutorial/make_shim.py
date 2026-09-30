"""The tutorial image's make shim (see "Build replay" in tutorial/README.md).

/usr/local/bin/make runs this for every top-level make. A make that is not a
step of a cactup build script (its parent is not the `/bin/sh` running an
attempt's `build-script`) only gets the restore marker's bookkeeping and
becomes /usr/bin/make. For a build script's steps it decides, at the config
step, whether the attempt matches a precomputed build (a *bake*); if so, the
config, clean, build and utils steps replay the bake's recorded output while a
detached copier stages the bake's tree, which the build step swaps in.

With CACTUP_TUTORIAL_SHIM=record (the bake container), every step runs the real
make and its output is recorded, with timings, for a bake.
"""

from __future__ import annotations

# First, before anything slow to import: _signal is the bare module (the
# signal module imports enum and more), and os is already loaded.
import _signal
import os

# A make whose caller ignores Ctrl-C (a `make &` from a script) must go on
# ignoring it, whatever this process does meanwhile.
SIGINT_IGNORED = _signal.getsignal(_signal.SIGINT) == _signal.SIG_IGN
if __name__ == "__main__" and not SIGINT_IGNORED:
    # Until main() has decided what to do, Ctrl-C ends this make quietly, as
    # it would a real make that has started nothing yet (not with a
    # traceback).
    _signal.signal(_signal.SIGINT, lambda *_: os._exit(130))

import signal  # noqa: E402
import base64
import calendar
import hashlib
import json
import re
import selectors
import shutil
import subprocess
import sys
import time
import tomllib
from pathlib import Path

REAL_MAKE = os.environ.get("CACTUP_TUTORIAL_REAL_MAKE", "/usr/bin/make")
BAKES = Path(os.environ.get("CACTUP_TUTORIAL_BAKES", "/opt/cactup-bakes"))
# The shim's own files in configs/NAME (dot-entries, which Cactus's
# realclean never touches), named for what make does, not for the tutorial.
MARKER = ".make-state"
SESSION = ".make-session"
STAGING = ".make-staging"
TRASH = ".make-trash-"
# Bumped whenever the harvest's layout changes: a cached bake of another
# format is baked again.
FORMAT = 7
STEPS = ("config", "clean", "build", "utils")
# A replay's longest silence, in seconds, and the fraction of the replay
# the build step's output must still have to go when the copy has to catch up.
MAX_GAP = 1.5
POLL = 0.05
# Silences shrink by a power of their length, not in proportion, so a long
# compile shrinks far more than the gaps between configure's quick checks,
# which would otherwise all scroll past in a blink.
GAP_POWER = 0.6


class ShimError(Exception):
    """A failure the shim reports as make would, then exits non-zero."""


# -- processes -------------------------------------------------------------------


def start_time(pid: int) -> int | None:
    """A process's start time (clock ticks since boot), or None if it is gone.

    Together with the pid it names one process: a pid can be reused, a
    (pid, start time) pair cannot.
    """
    try:
        stat = Path(f"/proc/{pid}/stat").read_text()
    except OSError:
        return None
    # The command name is in parentheses and may itself contain spaces.
    fields = stat[stat.rindex(")") + 2 :].split()
    # A zombie has exited: only its parent hasn't reaped it yet.
    if fields[0] in ("Z", "X"):
        return None
    return int(fields[19])


def alive(ident: list[int] | tuple[int, int] | None) -> bool:
    return bool(ident) and start_time(ident[0]) == ident[1]


def build_shell() -> tuple[tuple[int, int], Path] | None:
    """The build-script shell that ran this make, and the script's path."""
    ppid = os.getppid()
    try:
        argv = Path(f"/proc/{ppid}/cmdline").read_bytes().split(b"\0")[:-1]
    except OSError:
        return None
    if len(argv) != 2 or not argv[0].endswith(b"sh"):
        return None
    script = Path(os.fsdecode(argv[1]))
    if script.name != "build-script" or script.parent.parent.name != ".cactup-builds":
        return None
    started = start_time(ppid)
    if started is None:
        return None
    return (ppid, started), script


# -- make's command line ---------------------------------------------------------


def targets(args: list[str]) -> list[str]:
    """The goals on a make command line (not options or variable assignments)."""
    out = []
    skip = False
    for a in args:
        if skip:
            skip = False
            continue
        if a in ("-j", "-l", "-C", "-f", "-I", "-o", "-W", "--jobs", "--load-average"):
            # Options that take their value as the next argument (-j's is optional
            # and numeric).
            skip = a not in ("-j", "--jobs")
            continue
        if a.startswith("-") or "=" in a:
            continue
        if a.isdigit():
            continue
        out.append(a)
    return out


def step_of(target: str, name: str) -> str | None:
    for step, suffix in (("config", "-config"), ("clean", "-clean"), ("realclean", "-realclean"),
                         ("utils", "-utils"), ("build", "")):
        if target == name + suffix:
            return step
    return None


# -- the restore marker ----------------------------------------------------------


def read_marker(cfg: Path) -> dict:
    try:
        return json.loads((cfg / MARKER).read_text())
    except (OSError, ValueError):
        return {}


def write_json(path: Path, data: dict) -> None:
    tmp = path.with_name(f".{path.name}.{os.getpid()}.tmp")
    tmp.write_text(json.dumps(data))
    os.replace(tmp, path)


def write_marker(cfg: Path, state: str, bake: str) -> None:
    write_json(cfg / MARKER, {"state": state, "bake": bake})


def bookkeeping(cactus_root: Path, goals: list[str]) -> None:
    """What every passed-through make does to the restore markers.

    A clean or realclean empties the tree, so it is no longer a bake's; a
    configure or build on a pristine tree makes it *built on*.
    """
    for goal in goals:
        for suffix in ("-realclean", "-clean"):
            if goal.endswith(suffix):
                try:
                    (cactus_root / "configs" / goal[: -len(suffix)] / MARKER).unlink()
                except OSError:
                    pass
                break
        else:
            name = goal[: -len("-config")] if goal.endswith("-config") else goal
            cfg = cactus_root / "configs" / name
            marker = read_marker(cfg)
            if marker.get("state") == "pristine":
                write_marker(cfg, "built-on", marker["bake"])


def reset_signals() -> None:
    """The signal dispositions make would have had without this process in
    between: Python ignores SIGPIPE and SIGXFSZ, and an exec keeps a signal
    ignored, so a make started from here (and every recipe it runs) would
    otherwise ignore them too."""
    signal.signal(signal.SIGPIPE, signal.SIG_DFL)
    signal.signal(signal.SIGXFSZ, signal.SIG_DFL)
    signal.signal(signal.SIGINT, signal.SIG_IGN if SIGINT_IGNORED else signal.SIG_DFL)


def exec_make(args: list[str]) -> None:
    sys.stdout.flush()
    sys.stderr.flush()
    reset_signals()
    # argv[0] "make", as typed: Cactus's makefiles print $(MAKE) ("Use make
    # NAME to build the configuration"), and recursive makes, which run the
    # shim's sh front again, go straight to /usr/bin/make from there.
    os.execv(REAL_MAKE, ["make", *args])


# -- the fingerprint -------------------------------------------------------------


def normalize_thornlist(text: str) -> list:
    """A thornlist as the set of what it fetches and builds.

    Comments (a disabled thorn is one), blank lines, whitespace and the order
    of thorns within a component don't matter; directives and thorns do.
    """
    header: list[str] = []
    components: list[dict] = []
    current: dict | None = None
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].strip()
        if not line:
            continue
        if line.startswith("!"):
            key, _, value = line.partition("=")
            key, value = key.strip(), " ".join(value.split())
            if key == "!TARGET":
                current = {"directives": [], "thorns": []}
                components.append(current)
            if current is None:
                header.append(f"{key}={value}")
            else:
                current["directives"].append(f"{key}={value}")
        else:
            entry = " ".join(line.split())
            if current is None:
                header.append(entry)
            else:
                current["thorns"].append(entry)
    for c in components:
        c["thorns"].sort()
    components.sort(key=lambda c: json.dumps(c, sort_keys=True))
    return [header, components]


def fingerprint_doc(attempt: Path) -> dict:
    """What decides a build's product, as cactup recorded it for this attempt.

    Only fields of build.toml, and the attempt's option file and thornlist
    with the attempt's directory taken out. The global .cactupignore's text is
    left out on purpose: the shapes already reflect what it exempts.
    """
    meta = tomllib.loads((attempt / "build.toml").read_text())
    cm = meta["config-meta"]
    here = str(attempt)
    options = (attempt / "cactup-optionlist.cfg").read_text().replace(here, "@ATTEMPT@")
    thornlist = (attempt / "cactup-thornlist.th").read_text().replace(here, "@ATTEMPT@")
    return {
        "config": meta["config"],
        "cactus-root": meta["cactus-root"],
        "machine": meta["machine"],
        "build-env": meta.get("build-env", ""),
        "flags": cm.get("flags", {}),
        "sources": cm.get("sources"),
        "thorn-shapes": cm.get("thorn-shapes"),
        "thorn-providers": cm.get("thorn-providers"),
        "options": options,
        "thornlist": normalize_thornlist(thornlist),
    }


def fingerprint(doc: dict) -> str:
    blob = json.dumps(doc, sort_keys=True, separators=(",", ":")).encode()
    return hashlib.sha256(blob).hexdigest()[:24]


def find_bake(fp: str) -> Path | None:
    bake = BAKES / fp
    return bake if (bake / "bake.json").is_file() else None


def bakes_of(root: str) -> list[str]:
    """The configs precomputed for this Cactus root."""
    names = set()
    try:
        for bake in BAKES.iterdir():
            try:
                fp = json.loads((bake / "bake.json").read_text())["fingerprint"]
            except (OSError, ValueError, KeyError):
                continue
            if fp["cactus-root"] == root:
                names.add(fp["config"])
    except OSError:
        pass
    return sorted(names)


def whole_build_differs(doc: dict, info: dict) -> list[str]:
    """What sets this build apart from the precomputed one of the same config
    as a whole, rather than by edits to its sources."""
    other = info["fingerprint"]
    out = []
    if doc["thornlist"] != other["thornlist"]:
        path = info.get("thornlist-path")
        out.append("Its thornlist is not the one the precomputed build used"
                   + (f" ({path}); to build that, pass --thornlist {path}." if path else "."))
    if doc["options"] != other["options"]:
        out.append("Its optionlist is not the one the precomputed build used.")
    if doc["flags"] != other["flags"]:
        changed = sorted(k for k in set(doc["flags"]) | set(other["flags"])
                         if doc["flags"].get(k) != other["flags"].get(k))
        out.append("Its build flags differ from the precomputed build's: " + ", ".join(changed) + ".")
    if doc["build-env"] != other["build-env"]:
        out.append("Its build environment (the machine's or universe's setup) differs.")
    return out


def nearest_bake(doc: dict) -> tuple[dict, list[str], list[str]] | None:
    """The bake of the same config closest to this attempt, and what differs:
    the repositories with edits (or at other commits) and the thorns whose
    shape or provider differs."""
    best = None
    try:
        bakes = sorted(BAKES.iterdir())
    except OSError:
        return None
    for bake in bakes:
        try:
            info = json.loads((bake / "bake.json").read_text())
        except (OSError, ValueError):
            continue
        other = info["fingerprint"]
        if (other["config"], other["cactus-root"]) != (doc["config"], doc["cactus-root"]):
            continue
        # Only what both builds have: a repository or thorn one of them
        # lacks is a thornlist difference, which whole_build_differs names.
        mine, theirs = doc.get("sources") or {}, other.get("sources") or {}
        repos = sorted(r for r in set(mine) & set(theirs) if mine[r] != theirs[r])
        thorns = sorted(
            t
            for field in ("thorn-shapes", "thorn-providers")
            for t in set(doc.get(field) or {}) & set(other.get(field) or {})
            if doc[field][t] != other[field][t]
        )
        thorns = sorted(set(thorns))
        # A bake of the same build with a few edits beats one of another
        # thornlist (or optionlist...) that happens to share more.
        score = (bool(whole_build_differs(doc, info)), len(repos) + len(thorns))
        if best is None or score < best[0]:
            best = (score, info, repos, thorns)
    return None if best is None else best[1:]


# -- recordings ------------------------------------------------------------------


def read_recording(path: Path) -> tuple[list[tuple[float, int, bytes]], float, int]:
    """A step's recorded output events, its duration and its exit status."""
    events, end, status = [], 0.0, 0
    for line in path.read_text().splitlines():
        rec = json.loads(line)
        if "exit" in rec:
            end, status = rec["t"], rec["exit"]
        else:
            events.append((rec["t"], rec["fd"], base64.b64decode(rec["data"])))
    return events, end, status


def schedule(steps: list[tuple[str, list[tuple[float, int, bytes]], float]], seconds: float,
             max_gap: float = MAX_GAP, power: float = GAP_POWER) -> dict[str, tuple[list[float], float]]:
    """When to replay each event, compressed so the whole replay takes about
    `seconds`, keeping the order and shape of the recording's timing (each
    silence scaled by a power of its length) but never staying silent for
    more than `max_gap`.

    Returns, per step, each event's planned time and the step's planned end,
    in seconds from the start of the replay.
    """
    gaps: list[float] = []
    for _, events, end in steps:
        prev = 0.0
        for t, _, _ in events:
            gaps.append(max(0.0, t - prev) ** power)
            prev = t
        gaps.append(max(0.0, end - prev) ** power)
    total = sum(gaps)
    if total <= 0:
        scaled = [0.0] * len(gaps)
    else:
        scaled = [g * seconds / total for g in gaps]
        # Cap the silences, then stretch what is left uncapped to make up the
        # time; a few rounds settle it.
        for _ in range(8):
            capped = [min(g, max_gap) for g in scaled]
            short = seconds - sum(capped)
            free = sum(g for g in capped if g < max_gap)
            if short <= 1e-6 or free <= 0:
                scaled = capped
                break
            scaled = [g if g >= max_gap else g * (1 + short / free) for g in capped]
        scaled = [min(g, max_gap) for g in scaled]
    out: dict[str, tuple[list[float], float]] = {}
    clock = 0.0
    it = iter(scaled)
    for name, events, _ in steps:
        times = []
        for _ in events:
            clock += next(it)
            times.append(clock)
        clock += next(it)
        out[name] = (times, clock)
    return out


# -- the copier ------------------------------------------------------------------


def manifest(bake: Path) -> dict:
    return json.loads((bake / "manifest.json").read_text())


def map_mtime(mtime: float, lo: float, hi: float, start: float, end: float) -> float:
    """Map the bake's mtimes from its build, [lo, hi], monotonically into
    [start, end]. A file older than the build (a header an archive unpacked
    with its own date, say) keeps its mtime, as a real build leaves it."""
    if mtime < lo:
        return mtime
    if hi <= lo:
        return end
    return start + (mtime - lo) * (end - start) / (hi - lo)


def stream_replace(fin, fout, swaps: dict[bytes, bytes], progress) -> None:
    """Copy fin to fout replacing each key of `swaps` (all the same length as
    their values), even across read boundaries."""
    keep = max((len(k) for k in swaps), default=1) - 1
    # Longest first: a shorter key (the time) may be part of a longer one
    # (the ISO date and time).
    order = sorted(swaps, key=len, reverse=True)
    tail = b""
    while chunk := fin.read(1 << 20):
        buf = tail + chunk
        for old in order:
            buf = buf.replace(old, swaps[old])
        if keep:
            fout.write(buf[:-keep])
            tail = buf[-keep:]
        else:
            fout.write(buf)
        # Counted once written, never before.
        progress(len(chunk))
    fout.write(tail)


def bake_executables(bake: Path) -> list[Path]:
    """The bake's executable and utils, relative to its exe/ directory."""
    exe = bake / "exe"
    return sorted(p.relative_to(exe) for p in exe.rglob("*") if p.is_file())


def stamp_swaps(baked: dict, when: float) -> dict[bytes, bytes]:
    """What to rewrite in datestamp.o and the executable so that the compile
    date and time Cactus reports (`__DATE__`, `__TIME__`, and the ISO string
    the flesh's makefile passes as CCTK_COMPILE_DATETIME) read `when`."""
    t = time.localtime(when)
    date, clock = time.strftime("%b %e %Y", t), time.strftime("%H:%M:%S", t)
    swaps = {}
    if "date" in baked:
        swaps[baked["date"]] = date
    if "time" in baked:
        swaps[baked["time"]] = clock
    if "datetime" in baked and "time" in baked:
        # Rewritten piece by piece in one pass over the baked string, keeping
        # whatever shape the makefile gave it: its date, then each of its
        # times (to the second, or to the minute).
        old_date = re.match(r"\d{4}-\d\d-\d\d", baked["datetime"])
        pieces = {baked["time"]: clock, baked["time"][:5]: clock[:5]}
        if old_date:
            new_date = time.strftime("%Y-%m-%d", t)
            pieces[old_date.group()] = new_date
            # gcc stores the string as two overlapping 16-byte constants, the
            # second starting with the date's last digit ("0T15:25:06"): that
            # digit is the new date's too, or it comes back at run time.
            pieces[old_date.group()[-1] + "T" + baked["time"]] = new_date[-1] + "T" + clock
        pattern = "|".join(re.escape(k) for k in sorted(pieces, key=len, reverse=True))
        swaps[baked["datetime"]] = re.sub(pattern, lambda m: pieces[m.group()], baked["datetime"])
    return {k.encode(): v.encode() for k, v in swaps.items() if len(k.encode()) == len(v.encode())}


# Text files in a config's tree that record the build they came from: the
# configure step's summary (its date, the options and thornlist it was given,
# make's jobserver) and ExternalLibraries' "done" stamps (the date each
# library was built).
RECORDS = re.compile(r"^(config-info|scratch/done/[^/]+)$")
DATE = re.compile(r"(?P<day>[A-Z][a-z]{2} [A-Z][a-z]{2} [ \d]\d \d\d:\d\d:\d\d)(?P<zone> [A-Z]{3,4})? (?P<year>\d{4})")


def rewrite_record(text: str, attempts: tuple[str, str], fifo: int, lo: float, hi: float,
                   start: float, end: float) -> str:
    """A record of the bake's build, as this replay's build would have
    written it: its attempt directory, its jobserver fifo, and each date from
    the bake's build mapped as the mtimes are (older dates are left alone)."""

    def date(m: re.Match) -> str:
        try:
            when = calendar.timegm(time.strptime(f"{m['day']} {m['year']}", "%a %b %d %H:%M:%S %Y"))
        except ValueError:
            return m.group()
        if not lo - 1 <= when <= hi + 1:
            return m.group()
        t = time.gmtime(map_mtime(max(when, lo), lo, hi, start, end))
        return time.strftime("%a %b %e %H:%M:%S", t) + (m["zone"] or "") + time.strftime(" %Y", t)

    text = text.replace(attempts[0], attempts[1])
    text = re.sub(r"GMfifo\d+", f"GMfifo{fifo}", text)
    return DATE.sub(date, text)


def copier(staging: Path, bake: Path, shell: tuple[int, int], start: float, end: float,
           attempt: str = "", fifo: int = 0) -> int:
    """Stage `bake`'s tree and executables in `staging` (this session's own
    staging directory in the config), then wait.

    Runs detached from the build (its own session, output in the staging
    directory's log). Progress goes to `progress` in the staging directory,
    which the replay paces itself on. It keeps watching the build-script
    shell: once that is gone it removes the staging directory (the build step
    renames it away once it has swapped the tree in). A failed copy removes
    what it copied at once, and the rest once the replay has read the error.
    """
    tree = staging / "tree"
    progress = staging / "progress"
    m = manifest(bake)
    info = json.loads((bake / "bake.json").read_text())
    executables = bake_executables(bake)
    total = max(1, m["bytes"] + sum((bake / "exe" / p).stat().st_size for p in executables))
    lo, hi = m["build-start"], m["mtime-max"]
    done = 0
    last = 0.0
    # The compile date and time Cactus reports (datestamp.o, linked last):
    # the end of this replay's build step, not the bake's.
    swaps = stamp_swaps(info.get("compile-stamp") or {}, end)

    def advance(n: int) -> None:
        nonlocal done, last
        done += n
        now = time.monotonic()
        if now - last > 0.05:
            report("copying")
            last = now

    def copy_file(src: Path, dst: Path) -> None:
        with open(src, "rb") as fin, open(dst, "wb") as fout:
            if swaps and (src.name == "datestamp.o" or src.parent.name == "exe"):
                stream_replace(fin, fout, swaps, advance)
            else:
                while chunk := fin.read(1 << 20):
                    fout.write(chunk)
                    advance(len(chunk))

    def report(state: str, error: str = "") -> None:
        write_json(progress, {"state": state, "done": done, "total": total, "error": error})

    def gone() -> bool:
        return not alive(shell)

    def forget_session() -> None:
        # A replay stopped early leaves its session file; once the build
        # that owned it is gone, so is the session (the next config step
        # would clear it anyway).
        session = staging.parent / SESSION
        try:
            if json.loads(session.read_text()).get("copier") == [os.getpid(), start_time(os.getpid())]:
                session.unlink()
        except (OSError, ValueError):
            pass

    try:
        report("copying")
        dirs = []
        for entry in m["entries"]:
            if gone():
                shutil.rmtree(staging, ignore_errors=True)
                forget_session()
                return 0
            path, kind = entry["path"], entry["type"]
            dst = tree / path
            src = bake / "tree" / path
            if kind == "dir":
                dst.mkdir(parents=True, exist_ok=True)
                dirs.append((dst, entry))
            elif kind == "link":
                os.symlink(entry["target"], dst)
                os.utime(dst, (time.time(), map_mtime(entry["mtime"], lo, hi, start, end)),
                         follow_symlinks=False)
            else:
                if attempt and RECORDS.match(path):
                    text = rewrite_record(src.read_text(errors="surrogateescape"), (info["attempt"], attempt),
                                          fifo, lo, hi, start, end)
                    dst.write_text(text, errors="surrogateescape")
                    advance(entry.get("size", 0))
                else:
                    copy_file(src, dst)
                os.chmod(dst, entry["mode"])
                mt = map_mtime(entry["mtime"], lo, hi, start, end)
                os.utime(dst, (mt, mt))
        for dst, entry in reversed(dirs):
            os.chmod(dst, entry["mode"])
            mt = map_mtime(entry["mtime"], lo, hi, start, end)
            os.utime(dst, (mt, mt))
        # The executables go last: the build step renames them into exe/.
        for rel in executables:
            if gone():
                shutil.rmtree(staging, ignore_errors=True)
                forget_session()
                return 0
            dst = staging / "exe" / rel
            dst.parent.mkdir(parents=True, exist_ok=True)
            copy_file(bake / "exe" / rel, dst)
            os.chmod(dst, (bake / "exe" / rel).stat().st_mode & 0o7777)
        done = total
        report("done")
    except Exception as e:  # reported to the replay, which stops the build
        print(f"copy failed: {e!r}", file=sys.stderr)
        shutil.rmtree(tree, ignore_errors=True)
        shutil.rmtree(staging / "exe", ignore_errors=True)
        try:
            report("error", f"{e.strerror or e}" if isinstance(e, OSError) else str(e))
        except OSError:
            pass
    while staging.exists():
        if gone():
            shutil.rmtree(staging, ignore_errors=True)
            break
        time.sleep(0.2)
    if gone():
        forget_session()
    return 0


def start_copier(staging: Path, bake: Path, shell: tuple[int, int], start: float, end: float,
                 attempt: str, fifo: int) -> list[int]:
    staging.mkdir()
    (staging / "tree").mkdir()
    write_json(staging / "progress", {"state": "starting", "done": 0, "total": 1, "error": ""})
    log = open(staging / "copier.log", "wb")
    proc = subprocess.Popen(
        [sys.executable, "-I", __file__, "--copier", str(staging), str(bake),
         str(shell[0]), str(shell[1]), repr(start), repr(end), attempt, str(fifo)],
        stdin=subprocess.DEVNULL, stdout=log, stderr=log, start_new_session=True, close_fds=True,
    )
    log.close()
    started = None
    for _ in range(100):
        started = start_time(proc.pid)
        if started is not None:
            break
        time.sleep(0.01)
    return [proc.pid, started or 0]


def remove_later(path: Path) -> None:
    """Delete a directory in the background: a full tree takes a while."""
    subprocess.Popen(["rm", "-rf", "--", str(path)], stdin=subprocess.DEVNULL,
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                     start_new_session=True, close_fds=True)


def discard(cfg: Path, path: Path) -> None:
    """Move a directory aside at once and delete it in the background."""
    trash = cfg / f"{TRASH}{os.getpid()}-{time.time_ns()}"
    try:
        os.rename(path, trash)
    except OSError:
        return
    remove_later(trash)


def clear_leftovers(cfg: Path) -> None:
    """Stop and remove what an earlier replay of this config left behind:
    its copier, if it is still running, and any staging directory."""
    try:
        old = json.loads((cfg / SESSION).read_text())
    except (OSError, ValueError):
        old = {}
    if alive(old.get("copier")):
        try:
            os.kill(old["copier"][0], signal.SIGKILL)
        except OSError:
            pass
    try:
        (cfg / SESSION).unlink()
    except OSError:
        pass
    for entry in cfg.iterdir():
        if entry.name.startswith(STAGING):
            discard(cfg, entry)
        elif entry.name.startswith(TRASH):
            remove_later(entry)


# -- replay ----------------------------------------------------------------------


class Replay:
    """One step of a replayed build."""

    # How long the copy may make no progress at all before the replay gives up.
    STALL = 120.0

    def __init__(self, cfg: Path, session: dict):
        self.cfg = cfg
        self.session = session
        self.bake = BAKES / session["bake"]
        self.info = json.loads((self.bake / "bake.json").read_text())
        self.staging = cfg / session["staging"] if session.get("staging") else None
        self.progress = (-1, time.monotonic())

    def recordings(self) -> list[tuple[str, list, float]]:
        out = []
        for step in self.session["steps"]:
            events, end, _ = read_recording(self.bake / f"{step}.rec")
            out.append((step, events, end))
        return out

    def fail(self, reason: str) -> None:
        if self.staging is not None:
            discard(self.cfg, self.staging)
        raise ShimError(f"restoring the precomputed build failed: {reason}")

    def copy_fraction(self) -> float:
        """How far the copy has got, 1.0 when there is no copy (or no more)."""
        if not self.session.get("restore") or self.session.get("swapped"):
            return 1.0
        try:
            p = json.loads((self.staging / "progress").read_text())
        except (OSError, ValueError):
            p = None
        if p is None:
            self.fail("the staging directory is gone")
        if p["state"] == "error":
            self.fail(p["error"])
        if p["state"] == "done":
            return 1.0
        now = time.monotonic()
        # Short of "done", never all of it: the last file may still lack its
        # mode, and the executables are staged last.
        if p["done"] != self.progress[0]:
            self.progress = (p["done"], now)
        if not alive(self.session["copier"]):
            self.fail("the copy stopped")
        if now - self.progress[1] > self.STALL:
            self.fail(f"the copy made no progress for {self.STALL:.0f} s")
        return min(p["done"] / p["total"], 0.999)

    def play(self, step: str, before: tuple[bytes, object] | None = None) -> int:
        """Replay a step's recorded output; `before` is (text, action): the
        action runs just before the first event containing the text."""
        plan = schedule(self.recordings(), self.session["seconds"])
        # The copy has to be done by the end of the build step's replay.
        copy_end = plan["build"][1] if "build" in plan else 1.0
        events, _, status = read_recording(self.bake / f"{step}.rec")
        times, end = plan[step]
        t0 = self.session["t0"]
        old, new = self.info["attempt"].encode(), self.session["attempt"].encode()
        # make's jobserver fifo is named after the top-level make's pid.
        fifo = f"GMfifo{os.getpid()}".encode()
        for (_, fd, data), at in zip(events, times):
            self.wait_until(t0 + at, at / copy_end if copy_end else 1.0)
            if before is not None and before[0] in data:
                before[1]()
                before = None
            os.write(fd, re.sub(rb"GMfifo\d+", fifo, data.replace(old, new)))
        self.wait_until(t0 + end, end / copy_end if copy_end else 1.0)
        return status

    def wait_until(self, when: float, fraction: float) -> None:
        fraction = min(1.0, fraction)
        while True:
            copied = self.copy_fraction() if fraction > 0 else 1.0
            now = time.time()
            if now >= when and copied >= fraction:
                return
            time.sleep(min(POLL, max(0.0, when - now)) if copied >= fraction else POLL)

    def swap(self, end: float) -> None:
        """Swap the staged tree into the config and rename the staged
        executables into exe/. Each replaces the old file by a rename, never
        by writing into it: simulations hard-link the executable, and
        overwriting the shared inode would change theirs too."""
        staging = self.staging
        tree, old = staging / "tree", staging / "old"
        try:
            old.mkdir(exist_ok=True)
            write_marker(self.cfg, "restoring", self.session["bake"])
            for entry in sorted(tree.iterdir()):
                live = self.cfg / entry.name
                if live.exists() or live.is_symlink():
                    os.rename(live, old / entry.name)
                os.rename(entry, live)
            exe = Path(self.info["fingerprint"]["cactus-root"]) / "exe"
            # Newer than every restored file, whose mtimes reach `end`.
            newest = max(time.time(), end)
            for rel in bake_executables(self.bake):
                (exe / rel).parent.mkdir(parents=True, exist_ok=True)
                os.replace(staging / "exe" / rel, exe / rel)
                os.utime(exe / rel, (newest, newest))
        except OSError as e:
            raise ShimError(f"restoring the precomputed build failed: {e.strerror or e}") from e
        discard(self.cfg, staging)

    def place_missing_executables(self) -> None:
        """Without a restore (the tree is already this bake's), put back an
        executable that has gone missing, through a temporary file."""
        exe = Path(self.info["fingerprint"]["cactus-root"]) / "exe"
        for rel in bake_executables(self.bake):
            dst = exe / rel
            if dst.exists():
                continue
            dst.parent.mkdir(parents=True, exist_ok=True)
            tmp = dst.with_name(f".{dst.name}.{os.getpid()}.tmp")
            shutil.copyfile(self.bake / "exe" / rel, tmp)
            os.chmod(tmp, (self.bake / "exe" / rel).stat().st_mode & 0o7777)
            os.replace(tmp, dst)


def load_session(cfg: Path, shell: tuple[int, int]) -> dict | None:
    try:
        session = json.loads((cfg / SESSION).read_text())
    except (OSError, ValueError):
        return None
    return session if tuple(session["shell"]) == tuple(shell) else None


def has_objects(cfg: Path) -> bool:
    for _, _, files in os.walk(cfg / "build"):
        if any(f.endswith(".o") for f in files):
            return True
    return False


def cleans(script: Path, name: str) -> bool:
    """Whether the build script runs NAME-clean (cactup's --clean)."""
    return bool(re.search(rf"\s{re.escape(name)}-clean\s*$", script.read_text(), re.M))


def from_scratch(meta: dict, script: Path, cfg: Path, name: str) -> bool:
    """Whether this attempt compiles everything: cactup decided on a full
    rebuild, the script cleans before building, or there are no objects."""
    return bool(meta.get("full-rebuild")) or cleans(script, name) or not has_objects(cfg)


def config_step(args: list[str], shell: tuple[int, int], script: Path, attempt: Path,
                meta: dict, cfg: Path) -> dict | None:
    """Decide the attempt's fate (see the README's decision list); returns
    the session, having written it, only when it replays."""
    name = meta["config"]
    clear_leftovers(cfg)
    doc = fingerprint_doc(attempt)
    fp = fingerprint(doc)
    bake = find_bake(fp)
    scratch = from_scratch(meta, script, cfg, name)
    marker = read_marker(cfg)
    if bake is None:
        if scratch:
            miss_note(doc, meta)
        return None
    if marker.get("state") == "restoring":
        restore = True
    elif marker.get("state") == "pristine" and marker.get("bake") == fp:
        if not scratch:
            return None
        if cleans(script, name):
            # --clean on the bake's tree: replay the clean and the build it
            # would take; the tree is already what they would leave.
            restore = not has_objects(cfg)
        elif has_objects(cfg):
            # A full rebuild without a realclean (cactup found the config
            # incomplete: its executable is gone, say): a real make only
            # relinks, so let it.
            return None
        else:
            restore = True
    elif scratch:
        restore = True
    else:
        return None
    steps = ["config"]
    if cleans(script, name):
        steps.append("clean")
    steps += ["build", "utils"]
    seconds = float(os.environ.get("CACTUP_TUTORIAL_BUILD_SECONDS", "45"))
    session = {
        "shell": list(shell),
        "bake": fp,
        "attempt": str(attempt),
        "restore": restore,
        "steps": steps,
        "seconds": seconds,
        "t0": time.time(),
        "copier": None,
        "staging": None,
    }
    if restore:
        replay = Replay(cfg, session)
        plan = schedule(replay.recordings(), seconds)
        session["staging"] = f"{STAGING}-{os.getpid()}-{time.time_ns()}"
        session["build-end"] = session["t0"] + plan["build"][1]
        # The restored mtimes fall between now and the end of the build step's
        # replay: after every source, in the order the real build left them.
        # The fifo is the config step's: this process, the make that prints it.
        session["copier"] = start_copier(cfg / session["staging"], replay.bake, shell, session["t0"],
                                         session["build-end"], str(attempt), os.getpid())
    write_json(cfg / SESSION, session)
    return session


def miss_note(doc: dict, meta: dict) -> None:
    near = nearest_bake(doc)
    lines = [
        "note: this build was not precomputed for the tutorial, so it compiles",
        "      everything, which takes a long time on this machine.",
    ]
    whole = near is not None and whole_build_differs(doc, near[0])
    if whole:
        # Not a matter of edits: the build itself is another one.
        lines += [f"      {line}" for line in whole]
    elif near is None:
        others = bakes_of(doc["cactus-root"])
        if others:
            lines.append("      Precomputed configs of this installation: " + ", ".join(others) + ".")
        else:
            lines.append("      No config of this installation was precomputed.")
    else:
        _, repos, thorns = near
        if repos:
            lines.append("      Repositories that differ from the precomputed build (see cactup config delta):")
            lines += [f"        {r}" for r in repos]
        if thorns:
            lines.append("      Thorns whose files differ (an extra or missing file under src/ is enough):")
            lines += [f"        {t}" for t in thorns]
        if repos or thorns:
            lines.append("      Reverting these brings the fast build back.")
    print("\n".join(lines), file=sys.stderr, flush=True)


def replay_step(step: str, cfg: Path, session: dict) -> int:
    replay = Replay(cfg, session)
    if step != "build":
        status = replay.play(step)
    else:
        done = []

        def finish() -> None:
            # The tree and the executable are in place by the time make says
            # it has created the executable, as they would be: once the copy
            # is done, all of it.
            if session["restore"]:
                replay.wait_until(time.time(), 1.0)
                replay.swap(session["build-end"])
                session["swapped"] = True
                write_json(cfg / SESSION, session)
            else:
                replay.place_missing_executables()
            write_marker(cfg, "pristine", session["bake"])
            done.append(True)

        status = replay.play(step, (b"Done creating", finish))
        if not done:
            finish()
    if step == "utils":
        try:
            (cfg / SESSION).unlink()
        except OSError:
            pass
    return status


# -- record ----------------------------------------------------------------------


def record_dir(config: str, attempt_name: str) -> Path:
    out = Path(os.environ["CACTUP_TUTORIAL_RECORD"]) / config / attempt_name
    out.mkdir(parents=True, exist_ok=True)
    return out


def record_step(args: list[str], step: str, out_dir: Path) -> int:
    """Run the real make, pass its output through and record it with timings,
    in line-sized events (so a path is never split between two)."""
    # (Popen restores SIGPIPE and SIGXFSZ itself; SIGINT as the caller had it.)
    proc = subprocess.Popen(["make", *args], executable=REAL_MAKE, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
                            preexec_fn=reset_signals)
    t0 = time.monotonic()
    sel = selectors.DefaultSelector()
    pending = {1: b"", 2: b""}
    last = {1: t0, 2: t0}
    with open(out_dir / f"{step}.rec", "w") as rec:

        def emit(fd: int, data: bytes) -> None:
            if not data:
                return
            os.write(fd, data)
            rec.write(json.dumps({"t": time.monotonic() - t0, "fd": fd,
                                  "data": base64.b64encode(data).decode()}) + "\n")

        sel.register(proc.stdout, selectors.EVENT_READ, 1)
        sel.register(proc.stderr, selectors.EVENT_READ, 2)
        open_fds = 2
        while open_fds:
            for key, _ in sel.select(timeout=0.2):
                fd = key.data
                chunk = os.read(key.fileobj.fileno(), 65536)
                if not chunk:
                    sel.unregister(key.fileobj)
                    open_fds -= 1
                    emit(fd, pending[fd])
                    pending[fd] = b""
                    continue
                buf = pending[fd] + chunk
                cut = buf.rfind(b"\n") + 1
                emit(fd, buf[:cut])
                pending[fd] = buf[cut:]
                last[fd] = time.monotonic()
            now = time.monotonic()
            for fd in (1, 2):
                if pending[fd] and now - last[fd] > 0.3:
                    emit(fd, pending[fd])
                    pending[fd] = b""
        status = proc.wait()
        rec.write(json.dumps({"t": time.monotonic() - t0, "exit": status}) + "\n")
    return status


# -- main ------------------------------------------------------------------------


def interrupted(step: str, goal: str, session: dict | None) -> int:
    """What make prints when Ctrl-C stops it: the bake recorded it for each
    step, interrupting a real make on the baked tree."""
    text = None
    if session is not None:
        try:
            info = json.loads((BAKES / session["bake"] / "bake.json").read_text())
            text = (BAKES / session["bake"] / f"interrupt-{step}.txt").read_text()
            text = text.replace(info["attempt"], session["attempt"])
            # Only the top-level make's own line: the sub-makes' name the
            # targets that were running when the bake was interrupted, not
            # those on the screen now. When make printed no line of its own
            # (it can happen), what it did print.
            top = "".join(line for line in text.splitlines(keepends=True) if line.startswith("make: "))
            text = top or text or None
        except (OSError, ValueError, KeyError):
            text = None
    sys.stderr.write(text or f"make: *** [{goal}] Interrupt\n")
    sys.stderr.flush()
    return 130


def main(args: list[str]) -> int:
    if args[:1] == ["--copier"]:
        staging, bake, pid, started, start, end, attempt, fifo = args[1:]
        return copier(Path(staging), Path(bake), (int(pid), int(started)), float(start), float(end),
                      attempt, int(fifo))
    if args[:1] == ["--record-clean"]:
        # The bake driver: what `make NAME-clean` prints, for a replayed --clean.
        name, attempt_name = args[1:]
        return record_step([f"{name}-clean"], "clean", record_dir(name, attempt_name))
    goals = targets(args)
    found = build_shell()
    if found is None or len(goals) != 1:
        bookkeeping(Path.cwd(), goals)
        exec_make(args)
    shell, script = found
    attempt = script.parent
    try:
        meta = tomllib.loads((attempt / "build.toml").read_text())
    except (OSError, tomllib.TOMLDecodeError):
        bookkeeping(Path.cwd(), goals)
        exec_make(args)
    name = meta["config"]
    step = step_of(goals[0], name)
    if step is None or step == "realclean":
        bookkeeping(Path.cwd(), goals)
        exec_make(args)
    mode = os.environ.get("CACTUP_TUTORIAL_SHIM")
    if mode in ("record", "probe"):
        # The bake container: learn the fingerprint (probe stops there), or
        # build for real and record every step.
        out_dir = record_dir(name, attempt.name)
        if step == "config":
            write_json(out_dir / "fingerprint.json", fingerprint_doc(attempt))
            if mode == "probe":
                print("make: probe: fingerprint recorded, stopping", file=sys.stderr)
                return 3
        return record_step(args, step, out_dir)
    cfg = Path(meta["cactus-root"]) / "configs" / name
    session = None
    # From here a Ctrl-C is make's to report (see interrupted()).
    if not SIGINT_IGNORED:
        signal.signal(signal.SIGINT, signal.default_int_handler)
    try:
        if step == "config":
            session = config_step(args, shell, script, attempt, meta, cfg)
        else:
            session = load_session(cfg, shell)
        if session is None:
            bookkeeping(Path.cwd(), goals)
            exec_make(args)
        return replay_step(step, cfg, session)
    except KeyboardInterrupt:
        # The build is over: its session goes, unless its copier is still
        # running (the next config step must be able to stop it, and the
        # copier removes the session itself once the build-script shell has
        # gone).
        if session is not None and load_session(cfg, shell) is not None and not alive(session.get("copier")):
            try:
                (cfg / SESSION).unlink()
            except OSError:
                pass
        return interrupted(step, goals[0], session)
    except ShimError as e:
        print(f"make: *** {e}", file=sys.stderr, flush=True)
        return 2


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
