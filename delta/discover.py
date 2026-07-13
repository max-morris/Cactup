"""Machine discovery for Delta (NCSA).

Replaces simfactory's
`aliaspattern = ^dt-login0[1-9]\\.delta\\.ncsa\\.illinois\\.edu$`.
"""
import re

_PATTERN = re.compile(r"^dt-login0[1-9]\.delta\.ncsa\.illinois\.edu$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
