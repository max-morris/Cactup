"""Machine discovery for SuperMIC (LSU HPC).

Replaces simfactory's `aliaspattern = ^smic[12](\\.hpc\\.lsu\\.edu)?$`.
"""
import re

_PATTERN = re.compile(r"^smic[12](\.hpc\.lsu\.edu)?$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
