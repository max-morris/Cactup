"""Editor checkpoints live outside the directories of the files they save."""

import os

import asyncio

import pytest
from tornado.web import HTTPError

from jupyter_server.services.contents.filemanager import AsyncFileContentsManager, FileContentsManager

from cactup_tutorial.checkpoints import AsyncOutOfTreeCheckpoints, OutOfTreeCheckpoints


def manager(tmp_path):
    root = tmp_path / "home"
    root.mkdir()
    cm = FileContentsManager(root_dir=str(root))
    cm.checkpoints = OutOfTreeCheckpoints(
        parent=cm, root_dir=str(root), checkpoint_root=str(tmp_path / "checkpoints")
    )
    return cm, root


def save(cm, path, text):
    cm.save({"type": "file", "format": "text", "content": text}, path)


def test_checkpoints_stay_out_of_source_trees(tmp_path):
    cm, root = manager(tmp_path)
    for thorn in ("CarpetX", "Z4c"):
        os.makedirs(root / "repos" / thorn / "src")
        save(cm, f"repos/{thorn}/src/foo.cc", f"// {thorn}\n")
        cm.create_checkpoint(f"repos/{thorn}/src/foo.cc")

    for dirpath, dirnames, _ in os.walk(root):
        assert ".ipynb_checkpoints" not in dirnames, dirpath
    # Same file name in two thorns: two checkpoints, not one overwritten.
    for thorn in ("CarpetX", "Z4c"):
        cp = tmp_path / "checkpoints" / "repos" / thorn / "src" / "foo-checkpoint.cc"
        assert cp.read_text() == f"// {thorn}\n"


def test_restore_and_delete_use_the_moved_checkpoint(tmp_path):
    cm, root = manager(tmp_path)
    save(cm, "a.par", "old\n")
    cp = cm.create_checkpoint("a.par")
    save(cm, "a.par", "new\n")
    assert [c["id"] for c in cm.list_checkpoints("a.par")] == [cp["id"]]
    cm.restore_checkpoint(cp["id"], "a.par")
    assert (root / "a.par").read_text() == "old\n"
    cm.delete_checkpoint(cp["id"], "a.par")
    assert cm.list_checkpoints("a.par") == []


def test_the_async_class_is_accepted_and_moves_checkpoints(tmp_path):
    # The server's default contents manager is asynchronous and refuses a
    # synchronous checkpoints class at startup.
    root = tmp_path / "home"
    (root / "src").mkdir(parents=True)
    cm = AsyncFileContentsManager(
        root_dir=str(root), checkpoints_class=AsyncOutOfTreeCheckpoints
    )
    cm.checkpoints.checkpoint_root = str(tmp_path / "checkpoints")

    async def go():
        await cm.save({"type": "file", "format": "text", "content": "x\n"}, "src/foo.cc")
        await cm.create_checkpoint("src/foo.cc")

    asyncio.run(go())
    assert not (root / "src" / ".ipynb_checkpoints").exists()
    assert (tmp_path / "checkpoints" / "src" / "foo-checkpoint.cc").read_text() == "x\n"


def test_a_path_cannot_leave_the_checkpoint_root(tmp_path):
    cm, root = manager(tmp_path)
    victim = tmp_path / "victim" / "x-checkpoint.txt"  # what the checkpoint of x.txt is called
    victim.parent.mkdir()
    victim.write_text("keep\n")
    with pytest.raises(HTTPError):
        cm.checkpoints.delete_checkpoint("checkpoint", "../victim/x.txt")
    assert victim.read_text() == "keep\n"


def test_directory_rename_and_delete_take_their_checkpoints_along(tmp_path):
    cm, root = manager(tmp_path)
    os.makedirs(root / "d")
    save(cm, "d/f.cc", "one\n")
    cp = cm.create_checkpoint("d/f.cc")
    cm.rename("d", "e")
    assert [c["id"] for c in cm.list_checkpoints("e/f.cc")] == [cp["id"]]
    cm.restore_checkpoint(cp["id"], "e/f.cc")
    assert (root / "e" / "f.cc").read_text() == "one\n"

    cm.delete("e")
    os.makedirs(root / "e")
    save(cm, "e/f.cc", "a different file\n")
    assert cm.list_checkpoints("e/f.cc") == []


def test_the_async_class_handles_the_whole_life_cycle(tmp_path):
    root = tmp_path / "home"
    (root / "d").mkdir(parents=True)
    cm = AsyncFileContentsManager(root_dir=str(root), checkpoints_class=AsyncOutOfTreeCheckpoints)
    cm.checkpoints.checkpoint_root = str(tmp_path / "checkpoints")

    async def go():
        model = {"type": "file", "format": "text", "content": "old\n"}
        await cm.save(model, "d/f.cc")
        cp = await cm.create_checkpoint("d/f.cc")
        await cm.save({**model, "content": "new\n"}, "d/f.cc")
        await cm.rename("d", "e")
        assert [c["id"] for c in await cm.list_checkpoints("e/f.cc")] == [cp["id"]]
        await cm.restore_checkpoint(cp["id"], "e/f.cc")
        assert (root / "e" / "f.cc").read_text() == "old\n"
        await cm.delete("e")
        assert not (tmp_path / "checkpoints" / "e").exists()

    asyncio.run(go())


def test_mirrored_paths_reject_parent_components(tmp_path):
    cm, root = manager(tmp_path)
    for bad in ("a/../../x", "../x", "a/./b"):
        with pytest.raises(HTTPError):
            cm.checkpoints._mirror(bad)
