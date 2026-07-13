"""Machine discovery for Crux (ALCF, Argonne).

Replaces simfactory's
`aliaspattern = ^crux-uan-\\d*.head.cm[.]crux[.]alcf[.]anl[.]gov$`
(regex verbatim).
"""
import re

_PATTERN = re.compile(r"^crux-uan-\d*.head.cm[.]crux[.]alcf[.]anl[.]gov$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
