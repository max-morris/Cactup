"""Machine discovery for Minerva (AEI).

Replaces simfactory's `aliaspattern = ^login0[1-2]\\.cluster$`.
"""
import re

_PATTERN = re.compile(r"^login0[1-2]\.cluster$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the full "loginNN.cluster" name; only the supplied
    # hostname is tested (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
