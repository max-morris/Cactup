"""Machine discovery for SuperMUC-NG (LRZ).

Replaces simfactory's `aliaspattern = ^login\\d\\d.sng.lrz.de$` (regex
verbatim, including the unescaped dots).
"""
import re

_PATTERN = re.compile(r"^login\d\d.sng.lrz.de$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
