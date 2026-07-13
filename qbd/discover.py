"""Machine discovery for qbd (LSU/LONI Queen Bee 4, qbd504.loni.org).

Replaces simfactory's `aliaspattern = ^qbd\\d+(\\.loni\\.org|)$`.
"""
import re

_PATTERN = re.compile(r"^qbd\d+(\.loni\.org|)$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name (the upstream
    # pattern accepts both).
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
