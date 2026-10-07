"""check-cactup-usage.py finds the cactup commands the tutorial runs."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

TOOL = Path(__file__).resolve().parents[1] / "tools" / "check-cactup-usage.py"


def load():
    spec = importlib.util.spec_from_file_location("check_cactup_usage", TOOL)
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def test_shell_lines_split_at_operators_and_keep_quoted_values() -> None:
    m = load()
    uses = m.shell_uses(
        "while squeue -h -n q1 | grep -q .; do sleep 1; done\n"
        'cactup sim submit k1 ~/p.par -K lab-note="set for one command" --overwrite 2>&1 | grep Hello\n'
        "cactup-tutorial-catch-up 5\n"
        "cactup -I et-gpu build tutorial-gpu \\\n  --variant gpu\n",
        "nb.md")
    assert [u.argv for u in uses] == [
        ["sim", "submit", "k1", "~/p.par", "-K", "lab-note=set for one command", "--overwrite"],
        ["-I", "et-gpu", "build", "tutorial-gpu", "--variant", "gpu"],
    ]
    assert uses[0].where == "nb.md:2"


def test_python_argument_lists_and_the_catch_up_helper() -> None:
    m = load()
    uses = m.python_uses(
        'cmd = ["cactup", "sim", "show", sim, "--output-dir", "--restart-id", str(restart)]\n'
        'proc = subprocess.Popen([str(CACTUP), "-I", RELEASE, "build", "tutorial"])\n'
        'cactup("machine", "create", MYLAB, "--no-discover")\n',
        "script")
    assert [u.argv for u in uses] == [
        ["sim", "show", "X", "--output-dir", "--restart-id", "X"],
        ["-I", "X", "build", "tutorial"],
        ["machine", "create", "X", "--no-discover"],
    ]


def test_the_tutorial_has_cactup_commands_to_check() -> None:
    m = load()
    uses = m.collect()
    assert len(uses) > 100
    assert any(u.argv[:2] == ["sim", "submit"] for u in uses)
