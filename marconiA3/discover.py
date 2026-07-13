"""Machine discovery for MARCONI A3 (CINECA).

Replaces simfactory's
`aliaspattern = ^(r[0-9][0-9][0-9]c[0-9][0-9]s[0-9][0-9])(\\.marconi\\.cineca\\.it)?$`.
Upstream also documented the sibling patterns (not ported — only A3 exists in
the MDB):
  marconiA1: ^(r000u[0-9][0-9]l[0-9][0-9])(\\.marconi\\.cineca\\.it)?$
  marconiA2: ^(r[0-9][0-9][0-9]c[0-9][0-9]s[0-9][0-9])(\\.marconi\\.cineca\\.it)?$
"""
import re

_PATTERN = re.compile(r"^(r[0-9][0-9][0-9]c[0-9][0-9]s[0-9][0-9])(\.marconi\.cineca\.it)?$")


def is_machine(hostname: str) -> bool:
    # Match the supplied hostname or its short form, so discovery works
    # whether cactup resolved a FQDN or a bare node name.
    return bool(_PATTERN.search(hostname) or _PATTERN.search(hostname.split(".")[0]))
