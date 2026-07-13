"""Machine discovery for Tianhe-1A (NSCC Tianjin).

Replaces simfactory's `aliaspattern = ^ln[123]$` (the bare login-node names;
note this overlaps with cosma8's ^ln[1-4]$ — as it did upstream).
"""
import re

_PATTERN = re.compile(r"^ln[123]$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
