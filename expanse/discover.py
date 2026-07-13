"""Machine discovery for Expanse (SDSC).

Replaces simfactory's `aliaspattern = ^login0[12](\\.expanse\\.sdsc\\.edu)$`
(regex verbatim — note the mandatory-but-parenthesized domain group).
"""
import re

_PATTERN = re.compile(r"^login0[12](\.expanse\.sdsc\.edu)$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
