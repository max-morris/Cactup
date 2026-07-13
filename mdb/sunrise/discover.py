"""Machine discovery for Sunrise (Stockholm University).

Replaces simfactory's `aliaspattern = ^sol-login(\\.fysik\\.su\\.se)?$`.
"""
import re

_PATTERN = re.compile(r"^sol-login(\.fysik\.su\.se)?$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
