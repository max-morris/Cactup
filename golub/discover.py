"""Machine discovery for golub (Illinois Campus Cluster).

Replaces simfactory's
`aliaspattern = ^(golubh\\d|cc-login\\d)\\.campuscluster\\.illinois\\.edu$`.
"""
import re

_PATTERN = re.compile(r"^(golubh\d|cc-login\d)\.campuscluster\.illinois\.edu$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
