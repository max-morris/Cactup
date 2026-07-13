"""Machine discovery for FUCHS (CSC Frankfurt).

Replaces simfactory's `aliaspattern = ^login[\\d]+(\\.cm\\.cluster)?$`.
"""
import re

_PATTERN = re.compile(r"^login[\d]+(\.cm\.cluster)?$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
