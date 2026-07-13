"""Machine discovery for Anvil (ACCESS cluster at Purdue).

Replaces simfactory's
`aliaspattern = ^(login0[0-7]|a[0-7]{3})\\.anvil\\.rcac\\.purdue\\.edu$`.
"""
import re

_PATTERN = re.compile(r"^(login0[0-7]|a[0-7]{3})\.anvil\.rcac\.purdue\.edu$")


def is_machine(hostname: str) -> bool:
    # The pattern requires the FQDN; only the supplied hostname is tested
    # (faithful to the old anchored aliaspattern).
    return bool(_PATTERN.search(hostname))
