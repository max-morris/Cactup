"""Machine discovery for db1.hpc.lsu.edu (LSU Deep Bayou).

Replaces simfactory's `aliaspattern = ^db\\d+(\\.hpc\\.lsu\\.edu)?$` (upstream
widened the pattern on 2026-07 to make the domain suffix optional).
"""
import re

_PATTERN = re.compile(r"^db\d+(\.hpc\.lsu\.edu)?$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name (the upstream
    # pattern accepts both).
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
