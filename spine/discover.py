"""Machine discovery for spine (Cactus workstation at LSU CCT).

Replaces simfactory's `aliaspattern = ^spine.cct.lsu.edu$` (regex verbatim,
including the unescaped dots).
"""
import re

_PATTERN = re.compile(r"^spine.cct.lsu.edu$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
