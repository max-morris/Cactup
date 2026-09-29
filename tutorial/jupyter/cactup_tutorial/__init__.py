"""IPython extension for the Cactup tutorial notebooks.

Loaded automatically in the tutorial image (see `ipython_config.py`); load it
by hand with `%load_ext cactup_tutorial`.
"""

__version__ = "0.1.0"


def load_ipython_extension(ipython):
    from .magics import TutorialMagics

    ipython.register_magics(TutorialMagics)


def _jupyter_labextension_paths():
    return [{"src": "labextension", "dest": "cactup-tutorial"}]
