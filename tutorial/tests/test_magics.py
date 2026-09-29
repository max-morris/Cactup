"""The magics, end to end in a real kernel."""

import os
import time

import nbformat
import pytest
from nbclient import NotebookClient
from nbclient.exceptions import CellExecutionError


def execute(tmp_path, *sources, strict=False):
    nb = nbformat.v4.new_notebook()
    nb.cells = [nbformat.v4.new_code_cell("%load_ext cactup_tutorial")]
    nb.cells += [nbformat.v4.new_code_cell(s) for s in sources]
    env = dict(os.environ)
    env["CACTUP_TUTORIAL_STRICT"] = "1" if strict else "0"
    client = NotebookClient(nb, timeout=60, kernel_name="python3", resources={"metadata": {"path": str(tmp_path)}})
    old = os.environ.copy()
    os.environ.update(env)
    try:
        client.execute()
    finally:
        os.environ.clear()
        os.environ.update(old)
    return nb.cells[1:]


def plain(cell):
    return "\n".join(o["data"]["text/plain"] for o in cell.outputs if "data" in o)


def test_shell_file_show_round_trip(tmp_path):
    cells = execute(
        tmp_path,
        "%%shell\nmkdir -p work && cd work && pwd",
        "%%file notes.par\nCarpetX::ncells_x = 64\n",
        "%show notes.par",
        "import os; print(os.getcwd())",
    )
    assert plain(cells[0]).strip().endswith("/work")
    # %%file resolved its relative path against the terminal's directory.
    assert (tmp_path / "work" / "notes.par").read_text() == "CarpetX::ncells_x = 64\n"
    assert "Wrote" in cells[1].outputs[0]["data"]["text/html"]
    assert "cactup-show" in cells[2].outputs[0]["data"]["text/html"]
    # Python cells follow the terminal's directory too.
    assert cells[3].outputs[0]["text"].strip() == str(tmp_path / "work")


def test_status_line_and_strict_mode(tmp_path):
    cells = execute(tmp_path, "%%shell\nexit_code() { return 4; }; exit_code")
    assert "exit status 4" in plain(cells[0])
    with pytest.raises(CellExecutionError):
        execute(tmp_path, "%%shell\nfalse", strict=True)
    execute(tmp_path, "%%shell --expect-fail\nfalse", strict=True)
    with pytest.raises(CellExecutionError):
        execute(tmp_path, "%%shell --expect-fail\ntrue", strict=True)


def test_line_form(tmp_path):
    cells = execute(tmp_path, "%shell echo one-liner")
    assert plain(cells[0]).strip() == "one-liner"


def test_output_just_before_a_pause_is_drawn_promptly(monkeypatch):
    # A prompt printed right after other output, then a quiet wait: the
    # display must show it within a refresh or two, not when the cell ends.
    from IPython.testing.globalipapp import start_ipython

    from cactup_tutorial import magics

    ip = start_ipython()
    frames = []
    start = time.monotonic()

    class Handle:
        def update(self, bundle, raw=True):
            frames.append((time.monotonic() - start, bundle["text/plain"]))

    def fake_display(bundle, raw=True, display_id=False):
        return Handle()

    monkeypatch.setattr(magics, "display", fake_display)
    ip.register_magics(magics.TutorialMagics)
    start = time.monotonic()
    ip.run_cell_magic("shell", "--quiet", "echo a; sleep 0.3; echo b; sleep 0.02; printf 'Continue? '; sleep 1.5")
    shown = [t for t, text in frames if "Continue?" in text]
    assert shown and shown[0] < 0.9, frames


def test_stdin_argument_loses_its_quotes():
    from cactup_tutorial.magics import _unescape

    assert _unescape("'y\\n'") == "y\n"
    assert _unescape('"yes please\\n"') == "yes please\n"
    assert _unescape("y") == "y"
