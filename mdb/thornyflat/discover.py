"""Machine discovery for Thorny Flat (WVU).

Replaces simfactory's `aliaspattern = ^trcis\\d\\d\\d\\.hpc\\.wvu\\.edu$`.
"""
import re

_PATTERN = re.compile(r"^trcis\d\d\d\.hpc\.wvu\.edu$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
