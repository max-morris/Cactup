"""Machine discovery for Perlmutter Phase 1 (NERSC).

Replaces simfactory's
`aliaspattern = ^login\\d\\d[.]chn[.]perlmutter[.]nersc[.]gov$`.
"""
import re

_PATTERN = re.compile(r"^login\d\d[.]chn[.]perlmutter[.]nersc[.]gov$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
