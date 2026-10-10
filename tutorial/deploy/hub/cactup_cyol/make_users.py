"""Create accounts on the machine: one by name (an admin's, say), or many
with random passwords, for handing out on paper.

    docker compose exec -it hub python3 -m cactup_cyol.make_users --name ada
    docker compose exec hub python3 -m cactup_cyol.make_users 30 [--prefix attendee]

`--name` asks for the password (or makes a random one with --random). The
bulk form prints `username,password` lines and skips names already taken.
Both write into the account database the login page uses. Names follow the
login page's rule: lowercase letters, digits and dashes, starting with a
letter.
"""

from __future__ import annotations

import argparse
import getpass
import os
import re
import secrets
import sys

from .accounts import Accounts
from .session import load_words

VALID = re.compile(r"^[a-z][a-z0-9-]{1,31}$")


def password() -> str:
    """Four words and a number: easy to type from a sheet of paper."""
    rng = secrets.SystemRandom()
    words = load_words("nouns") + load_words("adjectives")
    return "-".join(rng.choice(words) for _ in range(4)) + f"-{rng.randint(10, 99)}"


def ask_password() -> str:
    while True:
        first = getpass.getpass("Password (at least 8 characters): ")
        if len(first) < 8:
            print("too short", file=sys.stderr)
            continue
        if getpass.getpass("Again: ") == first:
            return first
        print("they differ", file=sys.stderr)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    parser.add_argument("count", type=int, nargs="?", help="how many accounts to make")
    parser.add_argument("--prefix", default="attendee", help="bulk names: PREFIX01, PREFIX02, ...")
    parser.add_argument("--name", help="make one account with this name")
    parser.add_argument("--random", action="store_true", help="with --name: a random password, printed")
    parser.add_argument("--db", default=os.environ.get("CYOL_ACCOUNTS", "/srv/jupyterhub/cyol-accounts.sqlite"))
    args = parser.parse_args()
    accounts = Accounts(args.db)
    if args.name:
        name = args.name.lower()
        if not VALID.fullmatch(name):
            sys.exit(f"{args.name!r}: use lowercase letters, digits and dashes, starting with a letter")
        if accounts.exists(name):
            sys.exit(f"{name} exists already (set_password changes its password)")
        pw = password() if args.random else ask_password()
        accounts.create(name, pw)
        print(f"{name},{pw}" if args.random else f"made {name}")
        return 0
    if args.count is None:
        parser.error("give a count, or --name")
    if not VALID.fullmatch(f"{args.prefix}01"):
        sys.exit(f"--prefix {args.prefix!r}: use lowercase letters, digits and dashes, starting with a letter")
    made = 0
    n = 1
    while made < args.count:
        name = f"{args.prefix}{n:02d}"
        n += 1
        if accounts.exists(name):
            continue
        pw = password()
        if accounts.create(name, pw):
            print(f"{name},{pw}")
            made += 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
