"""Set a forgotten password (the hub's own admin page can't).

    docker compose exec -it hub python3 -m cactup_cyol.set_password ada

Asks for the new password, so it never appears on a command line or in a
shell's history.
"""

from __future__ import annotations

import argparse
import os
import sys

from .accounts import Accounts
from .make_users import ask_password


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("username")
    parser.add_argument("--db", default=os.environ.get("CYOL_ACCOUNTS", "/srv/jupyterhub/cyol-accounts.sqlite"))
    args = parser.parse_args()
    accounts = Accounts(args.db)
    name = args.username.lower()
    if not accounts.exists(name):
        sys.exit(f"no account named {name} (the hub's admin page lists them)")
    accounts.set_password(name, ask_password())
    print(f"set the password of {name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
