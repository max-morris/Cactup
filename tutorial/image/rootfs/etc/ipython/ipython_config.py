# Every kernel in the tutorial image loads the tutorial's magics:
# %%shell, %%file and %show.
c.InteractiveShellApp.extensions = ["cactup_tutorial"]  # noqa: F821
