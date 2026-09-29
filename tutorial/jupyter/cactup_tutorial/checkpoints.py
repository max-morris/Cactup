"""Editor checkpoints kept out of the files' own directories.

JupyterLab saves a checkpoint of every file it opens, and the stock
`FileCheckpoints` writes it next to the file, in `.ipynb_checkpoints/`. The
tutorial sends attendees into thorn source trees, where a new file changes
what cactup sees in the thorn (and whether a precomputed build still
matches), so checkpoints go to a tree of their own instead, mirroring the
server's directory layout so two files with the same name never share one.
"""

from __future__ import annotations

import os
import shutil

from jupyter_core.utils import ensure_dir_exists
from jupyter_server.services.contents.filecheckpoints import AsyncFileCheckpoints, FileCheckpoints
from tornado.web import HTTPError
from traitlets import HasTraits, Unicode, default


class _OutOfTree(HasTraits):
    checkpoint_root = Unicode(
        config=True,
        help="Directory under which checkpoints are kept, in the server's directory layout.",
    )

    @default("checkpoint_root")
    def _default_checkpoint_root(self):
        data = os.environ.get("XDG_DATA_HOME") or os.path.expanduser("~/.local/share")
        return os.path.join(data, "jupyter", "checkpoints")

    def _mirror(self, path):
        """`path` (an API path) under the checkpoint root.

        The contents manager's own check keeps a path inside the server root;
        this one keeps it inside the checkpoint root, which a `..` would leave.
        """
        parts = [p for p in path.strip("/").split("/") if p]
        if any(p in (".", "..") for p in parts):
            raise HTTPError(404, f"No such file or directory: {path}")
        return os.path.join(self.checkpoint_root, *parts)

    def checkpoint_path(self, checkpoint_id, path):
        parent, name = ("/" + path.strip("/")).rsplit("/", 1)
        basename, ext = os.path.splitext(name)
        self._get_os_path(path=parent)  # 404 outside the server root
        cp_dir = self._mirror(parent)
        with self.perm_to_403():
            ensure_dir_exists(cp_dir)
        return os.path.join(cp_dir, f"{basename}-{checkpoint_id}{ext}")

    # A renamed or deleted directory takes its files' checkpoints with it. The
    # stock methods only handle a file's own checkpoints: beside the file they
    # moved with the directory anyway, but here they would be orphaned, and a
    # new file at the old path would list the old file's checkpoint.

    def _move_tree(self, old_path, new_path):
        old, new = self._mirror(old_path), self._mirror(new_path)
        if os.path.isdir(old):
            with self.perm_to_403():
                ensure_dir_exists(os.path.dirname(new))
                if os.path.isdir(new):
                    shutil.rmtree(new)
                os.rename(old, new)

    def _remove_tree(self, path):
        tree = self._mirror(path)
        if path.strip("/") and os.path.isdir(tree):
            with self.perm_to_403():
                shutil.rmtree(tree)


class OutOfTreeCheckpoints(_OutOfTree, FileCheckpoints):
    """For the synchronous `FileContentsManager`."""

    def rename_all_checkpoints(self, old_path, new_path):
        super().rename_all_checkpoints(old_path, new_path)
        self._move_tree(old_path, new_path)

    def delete_all_checkpoints(self, path):
        super().delete_all_checkpoints(path)
        self._remove_tree(path)


class AsyncOutOfTreeCheckpoints(_OutOfTree, AsyncFileCheckpoints):
    """For the asynchronous contents managers the server uses by default."""

    async def rename_all_checkpoints(self, old_path, new_path):
        await super().rename_all_checkpoints(old_path, new_path)
        self._move_tree(old_path, new_path)

    async def delete_all_checkpoints(self, path):
        await super().delete_all_checkpoints(path)
        self._remove_tree(path)
