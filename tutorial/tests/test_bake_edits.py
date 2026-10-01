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
