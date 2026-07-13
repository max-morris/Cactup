"""Machine discovery for hal2 (Raspberry Pi at LSU CCT).

Replaces simfactory's `aliaspattern = ^hal2.cct.lsu.edu$` (regex verbatim,
including the unescaped dots).
"""
import re

_PATTERN = re.compile(r"^hal2.cct.lsu.edu$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
