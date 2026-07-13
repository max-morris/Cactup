"""Machine discovery for Nibi GPU nodes (SHARCNET).

Replaces simfactory's `aliaspattern = ^l\\d.nibi.sharcnet$` (regex verbatim,
including the unescaped dots).
"""
import re

_PATTERN = re.compile(r"^l\d.nibi.sharcnet$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the domain-qualified name; only the supplied
    # hostname is tested (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
