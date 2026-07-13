"""Machine discovery for qbc.loni.org (LSU Queen Bee 3).

Replaces simfactory's `aliaspattern = ^qbc\\d+.loni.org$` (regex verbatim,
including the unescaped dots).
"""
import re

_PATTERN = re.compile(r"^qbc\d+.loni.org$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
