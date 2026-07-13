"""Machine discovery for hedges (workstation at Belmont).

Replaces simfactory's `aliaspattern = ^hedges\\.belmont\\.edu$`.
"""
import re

_PATTERN = re.compile(r"^hedges\.belmont\.edu$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
