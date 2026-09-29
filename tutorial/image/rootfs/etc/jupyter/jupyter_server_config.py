# Notebook server settings shared by the solo and JupyterHub modes.
import os

c = get_config()  # noqa: F821

# The solo-mode token comes in the environment (not on the command line); take
# it out, so kernels and everything they run don't inherit it.
_token = os.environ.pop("JUPYTER_TOKEN", None)
os.environ.pop("CACTUP_TUTORIAL_TOKEN", None)
if _token:
    c.IdentityProvider.token = _token

c.ServerApp.root_dir = "/home/cactus"
c.ServerApp.default_url = "/lab/tree/tutorial/01-intro.ipynb"
# A terminal is part of the tutorial (the follow view in notebook 7).
c.ServerApp.terminals_enabled = True
c.ServerApp.terminado_settings = {"shell_command": ["/bin/bash", "-l"]}
# cactup keeps its state in hidden directories (~/.cactup, .cactup-builds),
# and the notebooks send attendees into them on purpose.
c.ContentsManager.allow_hidden = True
# Checkpoints go under ~/.local/share/jupyter/checkpoints, not next to each
# file: a stray file in a thorn's source tree changes what cactup sees there.
c.AsyncFileContentsManager.checkpoints_class = "cactup_tutorial.checkpoints.AsyncOutOfTreeCheckpoints"
