"""The persistent PTY shell behind %%shell."""

import os
import signal
import textwrap
import threading
import time

import pytest

from cactup_tutorial.session import COLUMNS, Result, Session


@pytest.fixture
def session(tmp_path):
    s = Session(cwd=str(tmp_path))
    yield s
    s.close()


def run(session, script, **kw):
    out = b""
    for item in session.run(script, **kw):
        if isinstance(item, Result):
            return out.decode(), item
        out += item
    raise AssertionError("no result")


def test_state_carries_across_cells(session, tmp_path):
    run(session, "mkdir sub && cd sub; export GREETING=hello; f() { echo from-function; }")
    out, res = run(session, 'pwd; echo "$GREETING"; f')
    assert res.status == 0
    assert out.split() == [str(tmp_path / "sub"), "hello", "from-function"]
    assert res.cwd == str(tmp_path / "sub")


def test_runs_on_a_terminal_of_known_width(session):
    out, _ = run(session, "[ -t 0 ] && [ -t 1 ] && [ -t 2 ] && echo tty; tput cols; echo $TERM")
    assert out.split() == ["tty", str(COLUMNS), "xterm-256color"]


def test_exit_status_and_multiline_scripts(session):
    out, res = run(session, textwrap.dedent("""\
        for i in 1 2 3; do
          echo line $i
        done
        false
    """))
    assert out.split("\n")[0].strip() == "line 1"
    assert res.status == 1


def test_timeout_interrupts_the_command_not_the_shell(session):
    start = time.monotonic()
    _, res = run(session, "sleep 30", timeout=0.5)
    assert time.monotonic() - start < 5
    assert res.timed_out and res.interrupted
    assert res.status == 130
    out, res = run(session, "echo alive")
    assert out.strip() == "alive" and res.status == 0


def test_interrupts_go_to_the_command(session, tmp_path):
    # A command that, like cactup, treats the first Ctrl-C as "wind down" and
    # keeps going until a second one.
    script = tmp_path / "twostage.py"
    script.write_text(textwrap.dedent("""\
        import signal, sys, time
        count = 0
        def on_int(*_):
            global count
            count += 1
            print(f"interrupt {count}", flush=True)
            if count == 2:
                sys.exit(7)
        signal.signal(signal.SIGINT, on_int)
        print("ready", flush=True)
        while True:
            time.sleep(0.05)
    """))
    items = session.run(f"python3 {script}")
    out = b""
    sent = 0
    for item in items:
        if isinstance(item, Result):
            res = item
            break
        out += item
        if b"ready" in out and sent == 0:
            session.interrupt()
            sent = 1
        if b"interrupt 1" in out and sent == 1:
            session.interrupt()
            sent = 2
    assert b"interrupt 1" in out and b"interrupt 2" in out
    assert res.status == 7 and res.interrupted
    assert run(session, "echo ok")[0].strip() == "ok"


def test_third_interrupt_kills_the_foreground_job(session):
    # A child shell that ignores Ctrl-C (so only the kill can stop it), and
    # that is already the terminal's foreground job when it says so.
    items = session.run('bash -c "trap \'\' INT; echo ready; exec sleep 30"')
    for item in items:
        if isinstance(item, Result):
            res = item
            break
        if b"ready" in item:
            for _ in range(3):
                session.interrupt()
    assert res.interrupted and res.status != 0


def test_keyboard_interrupt_while_reading_is_forwarded(session):
    # The kernel delivers a Jupyter interrupt as SIGINT to itself, which
    # Python raises as KeyboardInterrupt wherever it happens to be.
    timer = threading.Timer(0.5, lambda: os.kill(os.getpid(), signal.SIGINT))
    timer.start()
    try:
        _, res = run(session, "sleep 30")
    finally:
        timer.cancel()
    assert res.interrupted and res.status == 130


def test_exit_respawns_a_fresh_shell_in_the_same_directory(session, tmp_path):
    run(session, "cd /")
    out, res = run(session, "echo bye; exit 3")
    assert res.exited and res.status == 3
    out, res = run(session, "pwd")
    assert out.strip() == "/" and res.status == 0


def test_an_abandoned_cell_does_not_leak_into_the_next(session):
    # The trailing sleep keeps "late" and the marker in separate reads.
    items = session.run("echo first; sleep 0.5; echo late; sleep 0.5")
    next(items)
    items.close()
    out, res = run(session, "echo second")
    assert "late" not in out and out.strip() == "second"


def test_output_that_looks_like_a_marker_prefix_passes_through(session):
    out, res = run(session, r"printf 'a\033]777;not-ours\007b\n'")
    assert res.status == 0
    assert "a" in out and "b" in out


def test_a_closed_session_leaves_no_files_behind(tmp_path):
    s = Session(cwd=str(tmp_path))
    first = s._tmpdir
    run(s, "exit 3")
    run(s, "true")  # the shell exited, so this one runs in a new shell
    second = s._tmpdir
    assert not os.path.exists(first)
    s.close()
    assert not os.path.exists(second)


def test_a_ctrl_c_after_the_command_ended_does_not_shift_later_cells(session):
    # The interrupt reaches an idle shell, which prints a prompt no cell asked
    # for; the cells after it must still get their own output and status.
    items = session.run("echo a")
    for item in items:
        break
    time.sleep(0.3)
    session.interrupt()
    for item in items:
        pass
    for word in ("b", "c", "d"):
        out, res = run(session, f"echo {word}")
        assert (out.strip(), res.status) == (word, 0)


def test_an_interrupt_right_after_the_line_is_sent_stops_the_cell(session):
    # The interrupt comes while the shell may still be reading the cell's
    # line; it must reach the cell (not cut the line in two) and stop it.
    before = session._seq
    result = {}

    def consume():
        result["out"], result["res"] = run(session, "echo started; sleep 20; echo finished")

    t = threading.Thread(target=consume)
    t.start()
    while session._seq == before:
        time.sleep(0.0005)
    session.interrupt()
    # A Ctrl-C that reaches the shell between two commands only stops the
    # one after it once that ends, so press again, as a person would.
    t.join(1)
    if t.is_alive():
        session.interrupt()
    t.join(10)
    assert not t.is_alive()
    assert result["res"].interrupted
    assert "finished" not in result["out"]
    out, res = run(session, "echo next")
    assert (out.strip(), res.status) == ("next", 0)


def test_a_cell_that_replaces_the_shell_can_still_be_stopped(session):
    items = session.run("exec bash --norc --noprofile", timeout=1)
    for item in items:
        if isinstance(item, Result):
            res = item
            break
    assert res.exited
    out, res = run(session, "echo alive")
    assert (out.strip(), res.status) == ("alive", 0)


def test_marker_lookalikes_in_the_output_do_not_end_the_cell(session):
    for tail in ("\\033]777;cactup-done;0;", "\\033]777;cactup-done;7;", "\\033]777;cactup-sta"):
        out, res = run(session, f"printf 'x{tail}'; echo; echo after")
        assert res.status == 0 and "after" in out and "\x1b]777" not in out
        out, res = run(session, "echo next")
        assert (out.strip(), res.status) == ("next", 0)


def test_unread_stdin_does_not_run_as_a_command(session):
    out, res = run(session, "echo nothing-read", stdin="y\n")
    assert res.status == 0
    out, res = run(session, "echo next")
    assert (out.strip(), res.status) == ("next", 0)


def test_a_cell_cannot_bring_back_a_prompt_or_lose_the_marker(session):
    run(session, "PS1='visible$ '; PROMPT_COMMAND='true'")
    out, res = run(session, "echo next")
    assert (out.strip(), res.status) == ("next", 0)
    assert "visible" not in out


def test_an_interrupt_held_before_the_cell_starts_cancels_it(session, monkeypatch):
    # Interrupt just before the cell's line is typed: the cell must not run.
    write = session._write

    def interrupt_then_write(data):
        if data.startswith(b"\nsource"):
            session.interrupt()
        write(data)

    monkeypatch.setattr(session, "_write", interrupt_then_write)
    out, res = run(session, "echo should-not-run")
    monkeypatch.setattr(session, "_write", write)
    assert res.status == 130 and res.interrupted and "should-not-run" not in out
    out, res = run(session, "echo next")
    assert (out.strip(), res.status) == ("next", 0)


def test_git_never_prompts_in_a_cell(session):
    out, res = run(session, 'echo "$GIT_TERMINAL_PROMPT $GCM_INTERACTIVE"')
    assert out.split() == ["0", "never"]


def test_an_interrupt_for_a_cell_that_never_starts_restarts_the_shell(session):
    # A shell that can't read its next line (stopped here; in real life,
    # stuck somewhere a Ctrl-C doesn't reach): the held interrupt must not
    # wait forever.
    os.kill(session.pid, signal.SIGSTOP)
    timer = threading.Timer(0.5, session.interrupt)
    timer.start()
    started = time.monotonic()
    try:
        _, res = run(session, "echo never")
    finally:
        timer.cancel()
    assert res.exited and time.monotonic() - started < 10
    out, res = run(session, "echo alive")
    assert (out.strip(), res.status) == ("alive", 0)


def test_a_late_interrupt_after_a_canceled_cell_does_not_cancel_the_next(session, monkeypatch):
    write = session._write

    def interrupt_then_write(data):
        if data.startswith(b"\nsource"):
            session.interrupt()
        write(data)

    monkeypatch.setattr(session, "_write", interrupt_then_write)
    _, res = run(session, "echo canceled")
    monkeypatch.setattr(session, "_write", write)
    assert res.status == 130
    session.interrupt()  # after the cell ended: goes to the idle shell
    time.sleep(0.2)
    out, res = run(session, "echo hello")
    assert (out.strip(), res.status, res.exited) == ("hello", 0, False)


def test_the_notebook_token_does_not_reach_cells(monkeypatch, tmp_path):
    monkeypatch.setenv("JUPYTER_TOKEN", "secret")
    s = Session(cwd=str(tmp_path))
    try:
        out, _ = run(s, 'echo "[${JUPYTER_TOKEN-unset}]"')
    finally:
        s.close()
    assert out.strip() == "[unset]"
