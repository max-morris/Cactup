"""Attendee accounts: usernames and scrypt password hashes in SQLite."""

from __future__ import annotations

import hashlib
import hmac
import secrets
import sqlite3
import time
from pathlib import Path

# scrypt's cost: about 50 ms and 16 MB a check, which makes guessing slow and
# keeps a room logging in at once cheap.
N, R, P = 2**14, 8, 1


def hash_password(password: str) -> str:
    salt = secrets.token_bytes(16)
    digest = hashlib.scrypt(password.encode(), salt=salt, n=N, r=R, p=P, dklen=32)
    return f"scrypt${N}${R}${P}${salt.hex()}${digest.hex()}"


_DUMMY: str | None = None


def dummy_hash() -> str:
    """A hash to check against for a name with no account, so that takes as
    long as a real check (made once)."""
    global _DUMMY
    if _DUMMY is None:
        _DUMMY = hash_password("no such account")
    return _DUMMY


def check_password(password: str, stored: str) -> bool:
    try:
        kind, n, r, p, salt, digest = stored.split("$")
        if kind != "scrypt":
            return False
        got = hashlib.scrypt(password.encode(), salt=bytes.fromhex(salt), n=int(n), r=int(r), p=int(p),
                             dklen=len(digest) // 2)
    except (ValueError, TypeError):
        return False
    return hmac.compare_digest(got.hex(), digest)


class Accounts:
    def __init__(self, path: str | Path):
        self.path = Path(path)
        self.path.parent.mkdir(parents=True, exist_ok=True)
        with self._connect() as db:
            db.execute("CREATE TABLE IF NOT EXISTS accounts ("
                       "username TEXT PRIMARY KEY, hash TEXT NOT NULL, created REAL NOT NULL)")

    def _connect(self) -> sqlite3.Connection:
        db = sqlite3.connect(self.path, timeout=10)
        db.execute("PRAGMA journal_mode=WAL")
        return db

    def exists(self, username: str) -> bool:
        with self._connect() as db:
            return db.execute("SELECT 1 FROM accounts WHERE username = ?", (username,)).fetchone() is not None

    def create(self, username: str, password: str) -> bool:
        """Make the account; False if the name is taken (two people choosing
        the same name at once: the second one loses)."""
        try:
            with self._connect() as db:
                db.execute("INSERT INTO accounts VALUES (?, ?, ?)", (username, hash_password(password), time.time()))
            return True
        except sqlite3.IntegrityError:
            return False

    def verify(self, username: str, password: str) -> bool:
        with self._connect() as db:
            row = db.execute("SELECT hash FROM accounts WHERE username = ?", (username,)).fetchone()
        if row is None:
            # As slow as a real check, so timing doesn't tell which names exist.
            check_password(password, dummy_hash())
            return False
        return check_password(password, row[0])

    def set_password(self, username: str, password: str) -> bool:
        with self._connect() as db:
            cur = db.execute("UPDATE accounts SET hash = ? WHERE username = ?", (hash_password(password), username))
            return cur.rowcount == 1

    def usernames(self) -> list[str]:
        with self._connect() as db:
            return [row[0] for row in db.execute("SELECT username FROM accounts ORDER BY created")]
