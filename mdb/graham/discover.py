"""Machine discovery for Graham (Compute Canada).

Replaces simfactory's `aliaspattern = ^(gra-login\\d+|gra\\d+)`.
"""
import re

_PATTERN = re.compile(r"^(gra-login\d+|gra\d+)")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
