"""Machine discovery for slurmjupyter (CCT at LSU).

Replaces simfactory's `aliaspattern = ^slurm(node\\d|jupyter)$`.
"""
import re

_PATTERN = re.compile(r"^slurm(node\d|jupyter)$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
