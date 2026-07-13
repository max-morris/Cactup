"""Machine discovery for et-juphub (ET JupyterHub container).

The old aliaspattern was `^generic\\.some\\.where$` — the placeholder meaning
"not discoverable" (it can never match a real host). Discovery is therefore
disabled; reach this machine with --machine=et-juphub.
"""


def is_machine(hostname: str) -> bool:
    return False
