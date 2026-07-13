"""Machine discovery for QB2 (LONI).

Replaces simfactory's `aliaspattern = ^qb([0-9]?)(\\.loni\\.org)?$`.
"""
import re

_PATTERN = re.compile(r"^qb([0-9]?)(\.loni\.org)?$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
