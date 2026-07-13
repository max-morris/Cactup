"""Machine discovery for et-cuda (CarpetX CUDA-in-Singularity).

Replaces simfactory's `aliaspattern = ^et-cuda`. The hostname is synthetic
(the machine is normally reached with --machine=et-cuda), but the prefix
pattern is ported faithfully.
"""
import re

_PATTERN = re.compile(r"^et-cuda")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
