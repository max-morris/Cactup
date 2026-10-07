#!/usr/bin/env python3
"""Check that every cactup command the tutorial runs still parses.

    tutorial/tools/check-cactup-usage.py --cactup PATH
    tutorial/tools/check-cactup-usage.py --image cactup-tutorial:tutorial

It collects every cactup invocation in the notebooks (shell cells, and the
argument lists Python cells hand to subprocess) and in the image's scripts
(catch-up and friends), then asks the given cactup build, through `--help`,
whether each subcommand still exists and still takes each option used. It
runs no command for real, so it takes seconds: run it against a new cactup
before moving tutorial/cactup.pin (see tutorial/UPDATING.md), long before the
hour-long notebook run.

It checks names only: a renamed or removed subcommand or option, or one that
moved to another subcommand. What a command prints, and what it does, is the
notebook run's to check.
"""

from __future__ import annotations

import argparse
import ast
import json
import re
import shlex
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

TUTORIAL = Path(__file__).resolve().parents[1]
NOTEBOOKS = TUTORIAL / "notebooks"
SCRIPTS = TUTORIAL / "image" / "rootfs" / "usr" / "local" / "bin"
TARGET = "x86_64-unknown-linux-musl"
# Shell words that end a command in a cell line.
OPERATORS = {";", "&&", "||", "|", "&", "(", ")", ">", ">>", "<", "2>&1", "2>", "&>", "|&"}


@dataclass
class Use:
    where: str
    argv: list[str]


@dataclass
class Help:
    commands: dict[str, str] = field(default_factory=dict)  # name or alias -> name
    options: dict[str, bool] = field(default_factory=dict)  # flag -> takes a value
    requires_command: bool = False  # usage says <COMMAND>: a subcommand must follow


def shell_uses(text: str, where: str) -> list[Use]:
    """cactup invocations on the lines of a shell cell (continuations joined)."""
    uses = []
    for n, line in enumerate(text.replace("\\\n", " ").splitlines(), 1):
        if "cactup" not in line or line.lstrip().startswith("#"):
            continue
        try:
            lex = shlex.shlex(line, posix=True, punctuation_chars=True)
            lex.whitespace_split = True
            tokens = list(lex)
        except ValueError:
            continue
        for i, token in enumerate(tokens):
            if token != "cactup":
                continue
            argv = []
            rest = tokens[i + 1:]
            for j, t in enumerate(rest):
                if t in OPERATORS or t.startswith((">", "<", "&", "|", ";")):
                    break
                # A file-descriptor number before a redirection (`2>&1`).
                if t.isdigit() and j + 1 < len(rest) and rest[j + 1].startswith((">", "<")):
                    break
                argv.append(t)
            uses.append(Use(f"{where}:{n}", argv))
    return uses


def python_uses(source: str, where: str) -> list[Use]:
    """cactup invocations in Python: argument lists that start with "cactup"
    or str(CACTUP), and calls of a function named cactup (catch-up's helper).
    Anything that isn't a string literal stands for a value, as "X"."""
    try:
        tree = ast.parse(source)
    except SyntaxError:
        return []
    uses = []

    def words(nodes: list[ast.expr]) -> list[str]:
        out = []
        for node in nodes:
            if isinstance(node, ast.Starred):
                continue
            out.append(node.value if isinstance(node, ast.Constant) and isinstance(node.value, str) else "X")
        return out

    for node in ast.walk(tree):
        if isinstance(node, ast.List) and node.elts:
            first = node.elts[0]
            is_cactup = (isinstance(first, ast.Constant) and first.value == "cactup") or (
                isinstance(first, ast.Call) and isinstance(first.func, ast.Name) and first.func.id == "str"
                and first.args and isinstance(first.args[0], ast.Name) and first.args[0].id == "CACTUP")
            if is_cactup:
                uses.append(Use(f"{where}:{node.lineno}", words(node.elts[1:])))
        elif isinstance(node, ast.Call) and isinstance(node.func, ast.Name) and node.func.id == "cactup":
            uses.append(Use(f"{where}:{node.lineno}", words(node.args)))
    return sorted(uses, key=lambda u: int(u.where.rsplit(":", 1)[1]))


def notebook_uses(path: Path) -> list[Use]:
    """The code cells of a jupytext MyST notebook: %%shell cells as shell,
    the rest as Python."""
    uses = []
    text = path.read_text()
    for m in re.finditer(r"^```\{code-cell\}[^\n]*\n(.*?)^```", text, re.M | re.S):
        cell = m.group(1)
        first_line = text.count("\n", 0, m.start(1)) + 1
        if cell.startswith("%%shell"):
            for use in shell_uses(cell, path.name):
                where, n = use.where.rsplit(":", 1)
                uses.append(Use(f"{where}:{first_line + int(n) - 1}", use.argv))
        elif not cell.lstrip().startswith("%"):
            for use in python_uses(cell, path.name):
                where, n = use.where.rsplit(":", 1)
                uses.append(Use(f"{where}:{first_line + int(n) - 1}", use.argv))
    return uses


def collect() -> list[Use]:
    uses = []
    for nb in sorted(NOTEBOOKS.glob("*.md")):
        uses += notebook_uses(nb)
    for script in sorted(SCRIPTS.iterdir()):
        if not script.is_file():
            continue
        head = script.read_text(errors="replace")
        if head.startswith("#!") and "python" in head.splitlines()[0]:
            uses += python_uses(head, script.name)
        elif head.startswith("#!"):
            uses += shell_uses(head, script.name)
    return uses


HELP_CACHE: dict[tuple[str, ...], Help | None] = {}


def help_for(cactup: str, path: tuple[str, ...]) -> Help | None:
    """What `cactup PATH --help` says the subcommand takes; None if PATH is
    not a subcommand."""
    if path in HELP_CACHE:
        return HELP_CACHE[path]
    proc = subprocess.run([cactup, *path, "--help"], capture_output=True, text=True,
                          env={"PATH": "/usr/bin:/bin", "NO_COLOR": "1", "HOME": tempfile.gettempdir()})
    if proc.returncode != 0:
        HELP_CACHE[path] = None
        return None
    h = Help()
    section = None
    for line in proc.stdout.splitlines():
        if line.startswith("Usage:"):
            h.requires_command = "<COMMAND>" in line
            continue
        if re.match(r"^\S.*:$", line):
            section = line[:-1].lower()
            continue
        if section == "commands":
            m = re.match(r"^  (\S+)\s*(.*)$", line)
            if m and m.group(1) != "help":
                name = m.group(1)
                h.commands[name] = name
                aliases = re.search(r"\[aliases?: ([^\]]+)\]", m.group(2))
                for alias in (aliases.group(1).split(",") if aliases else []):
                    h.commands[alias.strip()] = name
        if section and "option" in section and line.lstrip().startswith("-"):
            # "  -I, --installation <ALIAS>": the flags, then a value if any.
            spec = line.strip().split("  ")[0]
            takes_value = "<" in spec
            for flag in re.findall(r"(?<![\w-])(--[a-z][a-z0-9-]*|-[A-Za-z])(?![\w-])", spec):
                h.options[flag] = takes_value
    HELP_CACHE[path] = h
    return h


def check(cactup: str, use: Use) -> list[str]:
    problems = []
    path: tuple[str, ...] = ()
    h = help_for(cactup, path)
    positional = False
    skip = False
    for word in use.argv:
        if skip:  # the value of the option before it
            skip = False
            continue
        if word.startswith("-") and word != "-":
            flag = word.split("=", 1)[0]
            if flag not in h.options:
                problems.append(f"{use.where}: `cactup {' '.join(path)}` has no option {flag}")
            else:
                skip = h.options[flag] and "=" not in word
            continue
        if positional or word == "X":
            positional = True
            continue
        target = h.commands.get(word)
        if target is None and h.commands:
            # Maybe a hidden alias: ask.
            sub = help_for(cactup, path + (word,))
            if sub is not None and word not in ("help",):
                path, h = path + (word,), sub
                continue
        if target is not None:
            path = path + (target,)
            h = help_for(cactup, path)
            if h is None:
                problems.append(f"{use.where}: `cactup {' '.join(path)}` is not a subcommand")
                break
            continue
        if h.requires_command:
            problems.append(f"{use.where}: `cactup {' '.join(path)}` has no subcommand {word}")
            break
        # A positional argument: from here on nothing is a subcommand.
        positional = True
    return problems


def binary_from_image(image: str, out: Path) -> Path:
    latest = json.loads(subprocess.run(
        ["docker", "run", "--rm", "--entrypoint", "cat", image, "/opt/cactup-tutorial/update-root/latest.json"],
        capture_output=True, text=True, check=True).stdout)
    name = f"/opt/cactup-tutorial/update-root/{TARGET}/cactup-{latest['build']}"
    cid = subprocess.run(["docker", "create", image], capture_output=True, text=True, check=True).stdout.strip()
    try:
        subprocess.run(["docker", "cp", f"{cid}:{name}", str(out)], check=True, capture_output=True)
    finally:
        subprocess.run(["docker", "rm", cid], capture_output=True)
    out.chmod(0o755)
    return out


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    which = parser.add_mutually_exclusive_group(required=True)
    which.add_argument("--cactup", help="a cactup binary to check against")
    which.add_argument("--image", help="a tutorial image: check against its current cactup build")
    parser.add_argument("--list", action="store_true", help="also print every invocation found")
    args = parser.parse_args()
    with tempfile.TemporaryDirectory() as tmp:
        cactup = args.cactup or str(binary_from_image(args.image, Path(tmp) / "cactup"))
        version = subprocess.run([cactup, "--version"], capture_output=True, text=True).stdout.strip()
        uses = collect()
        problems = []
        for use in uses:
            if args.list:
                print(f"{use.where}: cactup {' '.join(use.argv)}")
            problems += check(cactup, use)
    for p in dict.fromkeys(problems):
        print(p)
    print(f"{len(uses)} cactup invocations checked against {version}: {len(set(problems))} problem(s)")
    return 1 if problems else 0


if __name__ == "__main__":
    sys.exit(main())
