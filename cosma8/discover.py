"""Machine discovery for COSMA8 (Durham/EPCC).

Replaces simfactory's `aliaspattern = ^ln[1-4]$` (the bare COSMA login-node
names).
"""
import re

_PATTERN = re.compile(r"^ln[1-4]$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
