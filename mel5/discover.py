"""Machine discovery for mel5 (Melete 05).

Replaces simfactory's `aliaspattern = melete05.cct.lsu.edu`. cactup determines
the local hostname and passes it in; `is_machine` returns True iff this is mel5.
The implementation may use the supplied hostname or ignore it and probe itself.
"""


def is_machine(hostname: str) -> bool:
    # cactup supplies the resolved hostname; match the FQDN or its short form.
    return hostname == "melete05.cct.lsu.edu" or hostname.split(".")[0] == "melete05"
