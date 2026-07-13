"""Machine discovery for LEONARDO DCGP (CINECA).

Replaces simfactory's
`aliaspattern = ^(r[0-9][0-9][0-9]c[0-9][0-9]s[0-9][0-9])(\\.leonardo\\.cineca\\.it)?$`.
"""
import re

_PATTERN = re.compile(r"^(r[0-9][0-9][0-9]c[0-9][0-9]s[0-9][0-9])(\.leonardo\.cineca\.it)?$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
