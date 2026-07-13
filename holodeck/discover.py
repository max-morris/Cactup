"""Machine discovery for holodeck (AEI Hannover).

Replaces simfactory's `aliaspattern = ^holodeck\\d{1,2}`.
"""
import re

_PATTERN = re.compile(r"^holodeck\d{1,2}")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
