"""Machine discovery for Wheeler (Caltech).

Replaces simfactory's `aliaspattern = ^wheeler(\\.caltech\\.edu|\\.wheeler\\.local)?$`.
"""
import re

_PATTERN = re.compile(r"^wheeler(\.caltech\.edu|\.wheeler\.local)?$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
