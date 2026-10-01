"""Catch-up's restore of the files notebooks edit, and the clean-tree check
notebook 5 runs before building tutorial-pinned, on a fake installation."""

from __future__ import annotations

import importlib.machinery
import importlib.util
import subprocess
from pathlib import Path

import pytest

BIN = Path(__file__).resolve().parents[1] / "image/rootfs/usr/local/bin"


def load(name: str):
    loader = importlib.machinery.SourceFileLoader(name.replace("-", "_"), str(BIN / name))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    module = importlib.util.module_from_spec(spec)
    loader.exec_module(module)
    return module


def git(repo: Path, *args: str) -> None:
    subprocess.run(["git", "-C", str(repo), *args], check=True, capture_output=True)


@pytest.fixture
def install(tmp_path: Path) -> Path:
    """A home with the stock installation laid out as cactup lays it out:
    repositories under repos/, arrangements and src as links into them."""
    home = tmp_path / "home"
    cactus = home / ".cactup/cacti/ET_2026_05_v0/Cactus"
    files = {
        "CarpetX": {"WaveToyX/src/wavetoyx.cxx": "int x;\n", "WaveToyX/param.ccl": "# params\n"},
        "flesh": {"src/main/Banner.c": "/* banner */\n"},
    }
    for repo, contents in files.items():
        root = cactus / "repos" / repo
        for rel, text in contents.items():
            (root / rel).parent.mkdir(parents=True, exist_ok=True)
            (root / rel).write_text(text)
        git(root, "init", "-q")
        git(root, "add", ".")
        git(root, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "-m", "fetched")
    # Each thorn is a link of its own into its repository, as cactup makes them.
    (cactus / "arrangements/CarpetX").mkdir(parents=True)
    (cactus / "arrangements/CarpetX/WaveToyX").symlink_to("../../repos/CarpetX/WaveToyX")
    (cactus / "src").symlink_to("repos/flesh/src")
    (cactus / "configs/tutorial").mkdir(parents=True)
    (cactus / "configs/tutorial/cactup-config.toml").write_text(
        '[sources]\nCarpetX = "x"\nflesh = "y"\n\n'
        '[thorn-providers]\nWaveToyX = "arrangements/CarpetX/WaveToyX"\n')
    heads = {repo: head(cactus / "repos" / repo) for repo in files}
    (cactus.parent / ".cactup").mkdir()
    (cactus.parent / ".cactup/fetch-state.toml").write_text(
        "schema = 1\n" + "".join(f'[repos.{r}]\nurl = "u"\nhead = "{h}"\n' for r, h in heads.items()))
    (home / ".cactup/cactupignore").write_text(".ipynb_checkpoints/\n.*.sw?\n.~*\n")
    return home


def head(repo: Path) -> str:
    return subprocess.run(["git", "-C", str(repo), "rev-parse", "HEAD"], check=True, capture_output=True,
                          text=True).stdout.strip()


def catch_up(home: Path):
    module = load("cactup-tutorial-catch-up")
    module.HOME = home
    module.CACTUP_HOME = home / ".cactup"
    module.SAVED = home / "tutorial-saved"
    return module


def test_a_clean_tree_is_left_alone(install: Path, capsys) -> None:
    catch_up(install).restore_edited()
    assert capsys.readouterr().out == ""
    assert not (install / "tutorial-saved").exists()


def test_edited_files_are_put_back_and_the_attendees_versions_saved(install: Path, capsys) -> None:
    cactus = install / ".cactup/cacti/ET_2026_05_v0/Cactus"
    (cactus / "arrangements/CarpetX/WaveToyX/param.ccl").write_text("# params\nCCTK_INT mine\n")
    (cactus / "src/main/Banner.c").unlink()
    catch_up(install).restore_edited()
    out = capsys.readouterr().out.splitlines()
    assert (cactus / "arrangements/CarpetX/WaveToyX/param.ccl").read_text() == "# params\n"
    assert (cactus / "src/main/Banner.c").read_text() == "/* banner */\n"
    [saved] = (install / "tutorial-saved").iterdir()
    assert (saved / "arrangements/CarpetX/WaveToyX/param.ccl").read_text() == "# params\nCCTK_INT mine\n"
    assert out == [
        "catch-up: put ~/Cactus/arrangements/CarpetX/WaveToyX/param.ccl back to its committed state "
        f"(notebook 5 edits it); your version is at ~/tutorial-saved/{saved.name}/"
        "arrangements/CarpetX/WaveToyX/param.ccl",
        "catch-up: put ~/Cactus/src/main/Banner.c back to its committed state (notebook 5 edits it); "
        "you had deleted it",
    ]


def sources_clean(home: Path, monkeypatch) -> int:
    module = load("cactup-tutorial-sources-clean")
    monkeypatch.setattr(module, "CACTUP_HOME", home / ".cactup")
    monkeypatch.setattr(module, "INSTALL", home / ".cactup/cacti/ET_2026_05_v0")
    monkeypatch.setattr(module, "CACTUS", home / ".cactup/cacti/ET_2026_05_v0/Cactus")
    return module.main()


def test_sources_clean_passes_a_tree_as_fetched(install: Path, monkeypatch, capsys) -> None:
    assert sources_clean(install, monkeypatch) == 0
    assert capsys.readouterr().err == ""


def test_sources_clean_lists_edits_and_fails(install: Path, monkeypatch, capsys) -> None:
    cactus = install / ".cactup/cacti/ET_2026_05_v0/Cactus"
    (cactus / "repos/CarpetX/WaveToyX/src/wavetoyx.cxx").write_text("int y;\n")
    assert sources_clean(install, monkeypatch) == 1
    assert "~/Cactus/repos/CarpetX/WaveToyX/src/wavetoyx.cxx is edited" in capsys.readouterr().err


def test_sources_clean_flags_extra_files_that_change_a_thorns_shape(install: Path, monkeypatch, capsys) -> None:
    thorn = install / ".cactup/cacti/ET_2026_05_v0/Cactus/repos/CarpetX/WaveToyX"
    (thorn / "src/wavetoyx-Copy1.cxx").write_text("int x;\n")
    (thorn / "NOTES").write_text("mine\n")
    # Not part of the shape: editor files the ignore file exempts, and files
    # outside the thorn's top level and src/ (test output, say).
    (thorn / "src/.wavetoyx.cxx.swp").write_text("")
    (thorn / "src/.ipynb_checkpoints").mkdir()
    (thorn / "src/.ipynb_checkpoints/wavetoyx-checkpoint.cxx").write_text("")
    (thorn / "test/out").mkdir(parents=True)
    (thorn / "test/out/result.tsv").write_text("1\n")
    (install / ".cactup/cacti/ET_2026_05_v0/Cactus/repos/flesh/notes.txt").write_text("mine\n")
    assert sources_clean(install, monkeypatch) == 1
    err = capsys.readouterr().err
    assert "WaveToyX/src/wavetoyx-Copy1.cxx is an extra file in thorn WaveToyX" in err
    assert "WaveToyX/NOTES is an extra file" in err
    for name in (".swp", "checkpoint", "result.tsv", "notes.txt"):
        assert name not in err


def test_sources_clean_flags_a_repository_off_its_fetched_commit(install: Path, monkeypatch, capsys) -> None:
    repo = install / ".cactup/cacti/ET_2026_05_v0/Cactus/repos/flesh"
    git(repo, "-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "mine")
    assert sources_clean(install, monkeypatch) == 1
    assert "~/Cactus/repos/flesh is on commit" in capsys.readouterr().err


def test_the_active_config_must_be_tutorial_itself() -> None:
    module = load("cactup-tutorial-catch-up")
    assert module.active_is_tutorial("- tutorial [built 2026-10-01 09:57] (active)\n")
    assert not module.active_is_tutorial("- tutorial [built 2026-10-01 09:57]\n"
                                         "- tutorial-pinned [built 2026-10-01 10:02] (active)\n")
