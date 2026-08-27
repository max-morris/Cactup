"""Machine discovery for omnia (AMD MI210 workstation).

Replaces simfactory's `aliaspattern = ^omnia$`.
"""
import re

_PATTERN = re.compile(r"^omnia$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
