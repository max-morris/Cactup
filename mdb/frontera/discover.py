"""Machine discovery for Frontera (TACC).

Replaces simfactory's
`aliaspattern = ^login[1234]\\.frontera\\.tacc\\.utexas\\.edu$`.
"""
import re

_PATTERN = re.compile(r"^login[1234]\.frontera\.tacc\.utexas\.edu$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
