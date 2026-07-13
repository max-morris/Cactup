"""Machine discovery for Frontier (OLCF, Oak Ridge).

Replaces simfactory's `aliaspattern = ^login\\d.frontier.olcf.ornl.gov$`
(regex verbatim, including the unescaped dots).
"""
import re

_PATTERN = re.compile(r"^login\d.frontier.olcf.ornl.gov$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
