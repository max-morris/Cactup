"""Machine discovery for mike (LSU HPC SuperMike, mike2.hpc.lsu.edu).

Replaces simfactory's `aliaspattern = ^mike\\d+(\\.hpc\\.lsu\\.edu|)$`.
"""
import re

_PATTERN = re.compile(r"^mike\d+(\.hpc\.lsu\.edu|)$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name (the upstream
    # pattern accepts both).
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
