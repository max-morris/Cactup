"""Discovery for the built-in `generic` machine.

`generic` is never auto-matched: it is the zero-match fallback for unrecognized
hosts (design §4.3/§4.6) and the base template for `cactup machine create`
(§4.7). It is selectable only as that fallback or via `--machine generic`.

cactup passes the resolved local hostname; this machine ignores it.
"""


def is_machine(hostname: str) -> bool:
    return False
