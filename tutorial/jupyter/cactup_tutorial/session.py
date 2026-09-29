"""A persistent bash session on a pseudo-terminal.

Each kernel owns one `Session`. Cells are sourced into the same shell, so the
working directory, exported variables and functions carry over from cell to
cell, the way they do in a terminal. The shell runs on a PTY because the
programs the tutorial drives behave differently without one: cactup draws
progress bars and checks for updates only when stderr is a terminal, and
colors come out only on a TTY.

The shell is interactive (`bash -i`) purely for job control. That puts every
command in its own process group and makes it the terminal's foreground
group, so a Ctrl-C written to the PTY reaches the running command and not
the shell, exactly as in a terminal: the command decides what an interrupt
means, and the session survives it.
"""

from __future__ import annotations

import contextlib
import errno
import fcntl
import os
import pty
import re
import select
import shlex
import shutil
import signal
import struct
import subprocess
import tempfile
import termios
import threading
import time
from collections.abc import Iterator
from dataclasses import dataclass

COLUMNS = 110
ROWS = 40

# The shell reports the end of every cell with this private OSC sequence,
# carrying the exit status, the cell's number and the working directory. The
# number is set by the cell's own file, not counted from prompts: a Ctrl-C
# that reaches the shell while it is idle prints a prompt too, and that
# marker carries 0. The number tells a cell's own marker apart from a stale
# one left by an earlier cell whose output was abandoned mid-read.
#
# A cell also announces that it has started. Without readline, bash reads the
# terminal a character at a time, and a Ctrl-C that lands while it is reading
# a command line discards only the part read so far: the rest still runs, as
# a different command. So a Ctrl-C is never sent to an idle shell that is
# about to read a cell's line; it waits for the cell's start marker. A
# Ctrl-C can still reach an idle shell (one sent just as a cell ended), and
# bash only notices it when the next character arrives, which it discards:
# so every line the session types starts with an empty line for that
# character to be (its prompt carries 0).
#
# Both markers are stripped from the output, so the user never sees them. A
# Ctrl-C that lands while bash prints a marker can cut it short (bash then
# prints it whole again), so the patterns are strict, and a cut-off piece of
# one (ended by a newline or another escape) is dropped rather than shown.
_START_RE = re.compile(rb"\x1b\]777;cactup-start;(\d+)\x07")
_DONE_RE = re.compile(rb"\x1b\]777;cactup-done;(\d+);(\d+);([^\x07\x1b]*)\x07")
_MARKER_PREFIX = b"\x1b]777;cactup-"

# How long an interrupt may wait for a cell to start before the shell is
# taken to be stuck and killed.
_HELD_GRACE = 3.0


def _fragment_pattern() -> re.Pattern[bytes]:
    # Any leading piece of a marker, ended by a line break or another escape
    # instead of the marker's own terminator: "\x1b", "\x1b]77", ...,
    # "\x1b]777;cactup-done;13".
    inner = rb"[^\x07\x1b\r\n]*"
    for ch in reversed(_MARKER_PREFIX[1:]):
        inner = rb"(?:" + re.escape(bytes([ch])) + inner + rb")?"
    return re.compile(rb"\x1b" + inner + rb"(?=[\r\n\x1b])")


_FRAGMENT_RE = _fragment_pattern()

# Bash runs PROMPT_COMMAND before every prompt, including the one after a
# Ctrl-C has abandoned the rest of a command line, so it is the one place
# the completion marker is guaranteed to be printed. `noflsh` keeps the
# terminal from flushing its queues when it turns a Ctrl-C into SIGINT: that
# flush happens whenever the line discipline gets to the Ctrl-C, which can be
# after a command that just finished has printed its marker, and a flushed
# marker leaves the cell waiting forever. The prompt strings are reset there
# too, and PROMPT_COMMAND is read-only, so a cell that sources a `.bashrc`
# can't bring back a visible prompt or lose the marker (an assignment to it
# prints "readonly variable", and in some forms, like `+=`, also stops the
# rest of that line). The directory is sent
# `%q`-quoted, so no name can break the marker; the reader takes the real one
# from /proc.
_RC = r"""
PS1=''; PS2=''; PS4='+ '
__cactup_pc='__cactup_s=$?; PS1=; PS2=; printf "\033]777;cactup-done;%s;%s;%q\007" "$__cactup_s" "${__cactup_id:-0}" "$PWD"; __cactup_id=0'
PROMPT_COMMAND=$__cactup_pc
readonly PROMPT_COMMAND
__cactup_id=1
unset HISTFILE
set +o history
shopt -s expand_aliases
stty -echo -echoctl noflsh 2>/dev/null
"""


@dataclass
class Result:
    status: int
    cwd: str
    interrupted: bool
    timed_out: bool
    exited: bool = False  # the shell itself exited (the cell ran `exit`)


class SessionDied(RuntimeError):
    pass


class Session:
    """One bash process on a PTY, fed cells one at a time."""

    def __init__(self, cwd: str | None = None, env: dict[str, str] | None = None):
        self.cwd = cwd or os.path.expanduser("~")
        self._env = env
        self.pid = -1
        self.fd = -1
        self._seq = 0
        self._done = 0  # the last cell whose completion marker has been read
        self._started = 0  # the last cell whose start marker has been read
        self._interrupts = 0
        self._held = 0  # interrupts waiting for the cell to start
        self._setting_up = False  # a shell is starting or a cell being written
        self._held_at: float | None = None  # when the oldest held interrupt came
        # Guards holding an interrupt against the reader releasing held ones,
        # which can run on different threads. Reentrant: a KeyboardInterrupt
        # can land inside the reader's locked section and call interrupt().
        self._hold_lock = threading.RLock()
        self._tmpdir = ""
        with self._forwarding_sigint():
            self._setting_up = True
            try:
                self._spawn()
            finally:
                self._setting_up = False

    # -- lifecycle -------------------------------------------------------

    def _spawn(self) -> None:
        env = dict(os.environ if self._env is None else self._env)
        env.update(
            TERM="xterm-256color",
            COLUMNS=str(COLUMNS),
            LINES=str(ROWS),
            PAGER="cat",
            GIT_PAGER="cat",
            SYSTEMD_PAGER="",
            # A cell can't answer git's credential or host-key prompts; a
            # mistyped repository URL should fail at once, not wait forever.
            GIT_TERMINAL_PROMPT="0",
            GCM_INTERACTIVE="never",
        )
        if "GIT_SSH_COMMAND" not in env and not _git_config("core.sshCommand", env):
            env["GIT_SSH_COMMAND"] = "ssh -o BatchMode=yes"
        # The notebook server's token has no business in the cells.
        for name in ("JUPYTER_TOKEN", "CACTUP_TUTORIAL_TOKEN"):
            env.pop(name, None)
        # The kernel's own interrupt handling is Python's; the child must start
        # with default signal dispositions or Ctrl-C would be ignored in it.
        pid, fd = pty.fork()
        if pid == 0:  # child
            try:
                os.chdir(self.cwd)
            except OSError:
                pass
            for sig in (signal.SIGINT, signal.SIGQUIT, signal.SIGTERM, signal.SIGPIPE):
                signal.signal(sig, signal.SIG_DFL)
            os.execvpe(
                "bash", ["bash", "--noprofile", "--norc", "--noediting", "-i"], env
            )
        self.pid, self.fd = pid, fd
        self._seq = 0
        # The rc and cell files live in a private directory that goes away with
        # the shell (`close`), and a respawned shell gets a new one.
        self._tmpdir = tempfile.mkdtemp(prefix="cactup-shell-")
        fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLUMNS, 0, 0))
        rc = os.path.join(self._tmpdir, "rc.sh")
        with open(rc, "w") as f:
            f.write(_RC)
        # Everything bash prints while starting up (job-control notices and the
        # like) is discarded up to the first completion marker.
        self._write(f"\nsource {_quote(rc)}\n".encode())
        self._done = self._started = 0
        self._interrupts = 0
        for item in self._read_until_done(1, interruptible=False, startup_timeout=15):
            if isinstance(item, Result) and item.exited:
                raise SessionDied("the shell exited while starting")
        self._seq = 1

    def alive(self) -> bool:
        if self.pid <= 0:
            return False
        try:
            pid, _ = os.waitpid(self.pid, os.WNOHANG)
        except ChildProcessError:
            return False
        return pid == 0

    def close(self) -> None:
        if self.pid > 0:
            try:
                os.killpg(self.pid, signal.SIGHUP)
            except OSError:
                pass
            try:
                os.waitpid(self.pid, 0)
            except OSError:
                pass
        if self.fd >= 0:
            try:
                os.close(self.fd)
            except OSError:
                pass
        self.pid, self.fd = -1, -1
        if self._tmpdir:
            shutil.rmtree(self._tmpdir, ignore_errors=True)
            self._tmpdir = ""

    # -- running cells ---------------------------------------------------

    def run(
        self,
        script: str,
        timeout: float | None = None,
        stdin: str | None = None,
        tick: bool = False,
    ) -> Iterator[bytes | Result]:
        """Run `script`, yielding raw output chunks and finally a `Result`.

        With `tick`, an empty chunk also comes every so often while the
        command is quiet, so a display can catch up on output it has not
        drawn yet (the last line before a pause, such as a prompt).

        A `KeyboardInterrupt` (the kernel turns a Jupyter interrupt into one)
        that arrives while this iterator is reading is forwarded to the
        running command as a Ctrl-C, and reading continues. A caller that
        catches one elsewhere while consuming should call `interrupt()` and
        keep iterating. Each interrupt sends another Ctrl-C; the third kills
        the command's process group outright.
        """
        self._interrupts = 0
        if stdin and not stdin.endswith("\n"):
            # The terminal hands a program only whole lines.
            stdin += "\n"
        # While a cell runs, a Jupyter interrupt (SIGINT) is forwarded to it
        # directly, never raised as a KeyboardInterrupt in the middle of this
        # bookkeeping. One that comes while the shell starts or the cell's
        # files are written is held like any other before the cell's start
        # marker, and cancels the cell.
        with self._forwarding_sigint():
            self._setting_up = True
            try:
                if not self.alive():
                    self.close()
                    self._spawn()
                if not self._tmpdir or self.fd < 0:
                    raise SessionDied("the shell is not running")
                self._seq += 1
                # Files per cell, so a cell whose line the shell has not read
                # yet never picks up the next cell's text. The body is a file
                # of its own, so the shell's error messages give the cell's
                # own line numbers. Input for the cell that it never read
                # would be run by the shell as the next command line, so it is
                # drained, also when the cell is canceled before it starts.
                # A Ctrl-C while the cell runs skips the drain, and input it
                # never read then still reaches the shell; notebooks only
                # pass answers (a `y`) this way, which at worst print "command
                # not found".
                seq = self._seq
                body = self._cell_path(seq, "body")
                with open(body, "w") as f:
                    f.write(script)
                    if not script.endswith("\n"):
                        f.write("\n")
                drain = "while read -r -t 0.05 _; do :; done; " if stdin else ""
                with open(self._cell_path(seq), "w") as f:
                    f.write(
                        f"__cactup_id={seq}; [ -e {_quote(self._cell_path(seq, 'cancel'))} ] "
                        f"&& {{ {drain}return 130; }}; "
                        f"printf '\\033]777;cactup-start;{seq}\\007'; "
                        f"source {_quote(body)}; __cactup_st=$?; {drain}return $__cactup_st\n"
                    )
                if self._held:
                    self._cancel(seq)
                self._write(f"\nsource {_quote(self._cell_path(seq))}\n".encode())
                if stdin:
                    self._write(stdin.encode())
            except BaseException:
                # This cell never got going: an interrupt held for it must
                # not cancel the next one.
                with self._hold_lock:
                    self._held = 0
                    self._held_at = None
                raise
            finally:
                self._setting_up = False

            deadline = time.monotonic() + timeout if timeout else None
            finished = False
            try:
                for item in self._read_until_done(seq, interruptible=True, deadline=deadline, tick=tick):
                    if isinstance(item, Result):
                        with self._hold_lock:
                            item.interrupted = self._interrupts > 0 or self._held > 0
                            self._held = 0
                            self._held_at = None
                        self.cwd = item.cwd or self.cwd
                        finished = True
                        yield item
                        return
                    yield item
            finally:
                if not finished:
                    # Closed before its result (the caller gave up on it): an
                    # interrupt held for this cell must not cancel the next.
                    with self._hold_lock:
                        self._held = 0
                        self._held_at = None

    def _cell_path(self, seq: int, part: str = "cell") -> str:
        return os.path.join(self._tmpdir, f"{part}-{seq}.sh")

    def _cancel(self, seq: int) -> None:
        if self._tmpdir:
            try:
                open(self._cell_path(seq, "cancel"), "w").close()
            except OSError:
                pass

    @contextlib.contextmanager
    def _forwarding_sigint(self):
        """Turn a Jupyter interrupt (SIGINT) into `interrupt()`, for a while.

        Only the main thread receives signals, so elsewhere this does nothing.
        """
        if threading.current_thread() is not threading.main_thread():
            yield
            return

        def forward(signum, frame):
            self.interrupt()

        previous = signal.signal(signal.SIGINT, forward)
        try:
            yield
        finally:
            signal.signal(signal.SIGINT, previous)

    def interrupt(self) -> None:
        """Forward one interrupt to the running command."""
        if self.fd < 0:
            return  # the shell has exited; the next cell starts a new one
        with self._hold_lock:
            # "Pending": this cell's line is sent and nothing has ended it
            # yet. Once it has ended (even without starting, when canceled),
            # an interrupt is an idle-shell Ctrl-C, not one to hold.
            if self._setting_up or (self._started < self._seq and self._done == self._seq - 1):
                # A shell is starting, or it is idle or reading this cell's
                # line: hold it until the cell starts (see the note on the
                # markers above).
                self._held += 1
                if self._held_at is None:
                    self._held_at = time.monotonic()
                self._cancel(self._seq)
                return
        self._interrupts += 1
        if self._interrupts <= 2:
            self._write(b"\x03")
        else:
            self._kill_foreground()

    # -- plumbing --------------------------------------------------------

    def _write(self, data: bytes) -> None:
        while data:
            try:
                n = os.write(self.fd, data)
            except InterruptedError:
                continue
            data = data[n:]

    def _kill_foreground(self) -> None:
        try:
            pgrp = os.tcgetpgrp(self.fd)
        except OSError:
            return
        # The shell itself in the foreground means the command is a builtin,
        # or replaced the shell (`exec bash`, which never prints a marker):
        # kill the shell, and the next cell gets a fresh one.
        target = self.pid if pgrp == self.pid else pgrp
        if target > 0:
            try:
                os.killpg(target, signal.SIGKILL)
            except OSError:
                pass

    def _read_until_done(
        self,
        seq: int,
        interruptible: bool,
        deadline: float | None = None,
        startup_timeout: float | None = None,
        tick: bool = False,
    ) -> Iterator[bytes | Result]:
        buf = b""
        start = time.monotonic()
        timed_out = False
        while True:
            try:
                now = time.monotonic()
                if deadline is not None and now > deadline:
                    # A timeout is an interrupt the user didn't have to send;
                    # escalate every few seconds until the command gives up.
                    timed_out = True
                    self.interrupt()
                    deadline = now + 3
                if startup_timeout is not None and now - start > startup_timeout:
                    raise SessionDied("the shell did not start")
                held_at = self._held_at
                # (Not while a shell starts: the startup timeout covers that.)
                if (
                    held_at is not None
                    and startup_timeout is None
                    and now - held_at > _HELD_GRACE
                    and self.pid > 0
                ):
                    # An interrupt has waited for a cell that never started:
                    # the shell is stuck somewhere no Ctrl-C reaches. Kill it;
                    # the cell ends as "the shell exited" and the next starts
                    # a fresh one.
                    self._held_at = None
                    try:
                        os.killpg(self.pid, signal.SIGKILL)
                    except OSError:
                        pass
                ready, _, _ = select.select([self.fd], [], [], 0.05)
                if not ready:
                    if tick:
                        yield b""
                    continue
                try:
                    chunk = os.read(self.fd, 65536)
                except OSError as e:
                    if e.errno != errno.EIO:  # EIO: the shell exited
                        raise
                    chunk = b""
                if not chunk:
                    if buf and self._done >= seq - 1:
                        yield buf
                    status = self._reap()
                    yield Result(status=status, cwd=self.cwd, interrupted=False,
                                 timed_out=timed_out, exited=True)
                    return
                buf = _FRAGMENT_RE.sub(b"", buf + chunk)
                while True:
                    m = _next_marker(buf)
                    if not m:
                        break
                    before, buf = buf[: m.start()], buf[m.end():]
                    is_start = m.re is _START_RE
                    cell = int(m.group(1 if is_start else 2))
                    if is_start:
                        # Only whitespace before a cell starts is the line
                        # break the shell prints for a Ctrl-C that reached it
                        # while idle (it can come in a read of its own).
                        if before.strip() and self._done >= seq - 1:
                            yield before
                        with self._hold_lock:
                            self._started = max(self._started, cell)
                            held = 0
                            if cell == seq:
                                held, self._held = self._held, 0
                                self._held_at = None
                        # Two at most: a third would kill whatever is in the
                        # foreground, and that is the shell itself right now.
                        for _ in range(min(held, 2)):
                            self.interrupt()
                        continue
                    if cell == 0:
                        # A prompt no cell asked for: a Ctrl-C reached the
                        # idle shell after a cell ended. What came before it
                        # is ordinary output, unless it is only the line
                        # break the shell prints for that Ctrl-C.
                        if before.strip() and self._done >= seq - 1:
                            yield before
                        continue
                    self._done = max(self._done, cell)
                    if self._tmpdir:
                        for part in ("cell", "body", "cancel"):
                            try:
                                os.unlink(self._cell_path(cell, part))
                            except OSError:
                                pass
                    if cell < seq:
                        # A stale marker from an abandoned cell: whatever came
                        # before it belongs to that cell, not this one.
                        continue
                    if before:
                        yield before
                    yield Result(
                        status=int(m.group(1)),
                        cwd=self._shell_cwd(m.group(3)),
                        interrupted=False,
                        timed_out=timed_out,
                    )
                    return
                # Hold back anything that could be the start of a marker split
                # across reads; everything before it is safe to hand out.
                keep = _partial_marker_suffix(buf)
                out, buf = buf[: len(buf) - keep], buf[len(buf) - keep :]
                # Until an abandoned cell's marker arrives, everything read
                # is still that cell's output, however it was split across
                # reads: the shell runs this cell only after that one ends.
                if out and self._done >= seq - 1:
                    if self._started < seq and not out.strip():
                        # Keep it until the start marker decides (see above).
                        buf = out + buf
                        continue
                    yield out
            except KeyboardInterrupt:
                if not interruptible:
                    raise
                self.interrupt()

    def _shell_cwd(self, quoted: bytes) -> str:
        """The shell's directory, as `$PWD` names it (a symlink stays one).

        The marker carries `$PWD` `%q`-quoted, so any name survives the trip;
        it is decoded here with shlex, and checked against /proc, which
        always has the real directory.
        """
        try:
            [pwd] = shlex.split(quoted.decode(errors="surrogateescape"))
        except ValueError:
            pwd = ""
        try:
            real = os.readlink(f"/proc/{self.pid}/cwd")
        except OSError:
            return pwd or self.cwd
        try:
            if pwd and os.path.samefile(pwd, f"/proc/{self.pid}/cwd"):
                return pwd
        except OSError:
            pass
        return real.removesuffix(" (deleted)")

    def _reap(self) -> int:
        status = -1
        if self.pid > 0:
            try:
                _, raw = os.waitpid(self.pid, 0)
                status = os.waitstatus_to_exitcode(raw)
            except ChildProcessError:
                pass
            self.pid = -1
        self.close()
        return status


def _git_config(key: str, env: dict[str, str]) -> str:
    try:
        out = subprocess.run(
            ["git", "config", "--get", key], env=env, capture_output=True, text=True, timeout=5
        )
    except (OSError, subprocess.SubprocessError):
        return ""
    return out.stdout.strip() if out.returncode == 0 else ""


def _next_marker(buf: bytes) -> re.Match[bytes] | None:
    found = [m for m in (_START_RE.search(buf), _DONE_RE.search(buf)) if m]
    return min(found, key=lambda m: m.start()) if found else None


def _partial_marker_suffix(buf: bytes) -> int:
    start = buf.rfind(b"\x1b")
    if start < 0:
        return 0
    tail = buf[start:]
    if _MARKER_PREFIX.startswith(tail[: len(_MARKER_PREFIX)]) and b"\x07" not in tail:
        return len(tail)
    return 0


def _quote(s: str) -> str:
    return "'" + s.replace("'", "'\\''") + "'"
