"""Machine discovery for Sciama (ICG Portsmouth).

Replaces simfactory's `aliaspattern = login1.prv.sciama.cluster` (unanchored,
regex verbatim). The old ini kept an earlier pattern commented out:
`^((login6.)?sciama)(\\.icg\\.port\\.ac\\.uk)?$`.
"""
import re

_PATTERN = re.compile(r"login1.prv.sciama.cluster")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
