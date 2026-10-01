"""The bakes do to their installations what the notebooks do, or the build
replay misses: these check the copies against each other."""

from __future__ import annotations

import ast
import re
from pathlib import Path

TUTORIAL = Path(__file__).resolve().parent.parent


def edits_in(source: str) -> list:
    """The `edits = [...]` list in a piece of Python source."""
    tree = ast.parse(source)
    for node in ast.walk(tree):
        if isinstance(node, ast.Assign) and any(getattr(t, "id", None) == "edits" for t in node.targets):
            return ast.literal_eval(node.value)
    raise AssertionError("no edits list")


def notebook_cell(name: str, marker: str) -> str:
    text = (TUTORIAL / "notebooks" / name).read_text()
    for cell in re.findall(r"```\{code-cell\} ipython3\n(.*?)```", text, re.S):
        if marker in cell:
            return cell
    raise AssertionError(f"no cell with {marker!r} in {name}")


def test_notebook_3s_fork_edit_is_the_bakes():
    notebook = edits_in(notebook_cell("03-forks-and-refetch.md", "edits = ["))
    bake = edits_in((TUTORIAL / "bake" / "forks.py").read_text())
    assert notebook == bake


def test_the_fork_edit_applies_to_the_tutorial_thornlist():
    text = (TUTORIAL / "thornlists" / "tutorial.th").read_text()
    for old, new in edits_in((TUTORIAL / "bake" / "forks.py").read_text()):
        assert text.count(old) == 1, old
        assert new not in text, new


def test_a_gpu_bake_is_cached_only_for_the_same_toolkit(tmp_path):
    import json
    import sys

    sys.path.insert(0, str(TUTORIAL / "bake"))
    import bake

    entry = tmp_path / "fp"
    entry.mkdir()
    (entry / "bake.json").write_text(json.dumps({"format": bake.make_shim.FORMAT, "cuda": "13.4.2"}))
    assert bake.cached(entry, "13.4.2")
    assert not bake.cached(entry, "13.5.0")
    assert bake.cached_format(entry)
    (entry / "bake.json").write_text(json.dumps({"format": bake.make_shim.FORMAT}))
    assert bake.cached(entry, None), "a CPU bake carries no toolkit"


def test_the_mounted_toolkit_and_its_version(tmp_path):
    import json
    import sys

    sys.path.insert(0, str(TUTORIAL / "bake"))
    import bake

    assert bake.cuda_version(tmp_path) is None
    (tmp_path / "cuda-13.4").mkdir()
    (tmp_path / "cuda-13.4" / "version.json").write_text(json.dumps({"cuda": {"version": "13.4.2"}}))
    assert bake.cuda_toolkit(tmp_path).name == "cuda-13.4"
    assert bake.cuda_version(tmp_path) == "13.4.2"
