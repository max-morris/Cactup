"""The JupyterHub authenticator: choose your own login.

- A known username logs in with its password; the token field is ignored.
- An unknown username with the current session token (or the previous one,
  within its grace period) creates the account with that password, and logs
  in. Admin usernames are never created this way: an admin's account is
  made on the machine (make_users --name), so nobody holding the token can
  take an admin name first.
- Anything else is refused with JupyterHub's usual "Invalid username or
  password", whatever went wrong, and counts as a failure. The reason goes
  to the hub's log ("login refused for ...").

Failures are limited, each within `failure_window` seconds:

- per address and username (`max_failures`): one attendee's typos lock only
  that name, from that address;
- per address, wrong session tokens only (`max_address_failures`): bounds
  guessing the token from one address. Only signing up checks it: a room
  often reaches the hub from a single address (a NAT, the site's proxy,
  rootless Docker's), and a returning attendee must not be locked out by
  the rest of the room's typos;
- per username, from any address (`max_user_failures`): bounds guessing one
  account's password even by someone who can claim any address (from inside
  an attendee's container, the hub can be reached directly, where the address
  header is whatever the client sends). The price: someone can lock a name
  out for the window.

Tries made while blocked are refused without counting, so a lock ends one
window after the failure that set it; a hub restart ends them all. Each
session token is one of about 7 * 10**10 (case, spacing and punctuation
don't count), and is replaced every hour.
"""

from __future__ import annotations

import asyncio
import re
import time
from collections import deque

from jupyterhub.auth import Authenticator
from traitlets import Float, Integer, Unicode, default

from .accounts import Accounts
from .session import Reader

# Longer input is refused before anything is looked up or remembered.
MAX_USERNAME = 64
MAX_PASSWORD = 1024
# Above this many tracked keys, expired ones are swept, at most once a
# minute: someone making a new key with every try mustn't buy a full sweep
# with each.
SWEEP_AT = 10_000
SWEEP_EVERY = 60


class CYOLAuthenticator(Authenticator):
    accounts_db = Unicode("/srv/jupyterhub/cyol-accounts.sqlite", config=True,
                          help="The SQLite file of accounts.")
    token_state = Unicode("/srv/jupyterhub/cyol-token.json", config=True,
                          help="The session token state the rotator keeps.")
    valid_username = Unicode(r"^[a-z][a-z0-9-]{1,31}$", config=True,
                             help="New usernames must match this (after lowercasing).")
    min_password_length = Integer(8, config=True)
    max_failures = Integer(5, config=True,
                           help="Failed logins one address may make for one username within failure_window.")
    max_address_failures = Integer(100, config=True,
                                   help="Wrong session tokens one address may send, for any usernames, within "
                                        "failure_window; past it, that address can't sign up.")
    max_user_failures = Integer(20, config=True,
                                help="Failed logins for one username, from any address, within failure_window.")
    failure_window = Float(600.0, config=True, help="Seconds.")

    @default("request_otp")
    def _default_request_otp(self):
        return True

    @default("otp_prompt")
    def _default_otp_prompt(self):
        return "Session token (new accounts only):"

    def __init__(self, **kwargs):
        super().__init__(**kwargs)
        self._accounts: Accounts | None = None
        self._token = Reader(self.token_state)
        # Failure times, keyed ("pair", address, username), ("address", address)
        # or ("user", username).
        self._failures: dict[tuple, deque[float]] = {}
        self._swept = 0.0

    @property
    def accounts(self) -> Accounts:
        if self._accounts is None:
            self._accounts = Accounts(self.accounts_db)
        return self._accounts

    def _keys(self, address: str, username: str, token_guess: bool = False) -> list[tuple[tuple, int]]:
        keys = [(("pair", address, username), self.max_failures),
                (("user", username), self.max_user_failures)]
        if token_guess:
            keys.append((("address", address), self.max_address_failures))
        return keys

    def _count(self, key: tuple, now: float) -> int:
        failures = self._failures.get(key)
        if failures is None:
            return 0
        while failures and now - failures[0] > self.failure_window:
            failures.popleft()
        if not failures:
            del self._failures[key]
            return 0
        return len(failures)

    def _blocked(self, address: str, username: str, now: float, token_guess: bool = False) -> bool:
        return any(self._count(key, now) >= limit for key, limit in self._keys(address, username, token_guess))

    def _failed(self, address: str, now: float, why: str, username: str, token_guess: bool = False) -> None:
        if len(self._failures) > SWEEP_AT and now - self._swept >= SWEEP_EVERY:
            self._swept = now
            for key in list(self._failures):
                self._count(key, now)
        for key, _ in self._keys(address, username, token_guess):
            self._failures.setdefault(key, deque()).append(now)
        self.log.warning("login refused for %r from %s: %s", username[:32], address, why)

    def _is_admin(self, username: str) -> bool:
        return username in {a.lower() for a in self.admin_users}

    async def authenticate(self, handler, data):
        address = handler.request.remote_ip if handler is not None else "local"
        now = time.time()
        username = (data.get("username") or "").strip().lower()
        password = data.get("password") or ""
        token = data.get("otp") or ""
        if len(username) > MAX_USERNAME or len(password) > MAX_PASSWORD:
            self.log.warning("login refused from %s: oversized input", address)
            return None
        if self._blocked(address, username, now):
            self.log.warning("login refused for %r from %s: too many failed logins", username[:32], address)
            return None
        if not username or not password:
            self._failed(address, now, "empty username or password", username)
            return None
        # scrypt takes a moment and SQLite may wait on a lock: off the event
        # loop. verify() takes as long for a name that doesn't exist, so the
        # time taken doesn't tell which names do.
        loop = asyncio.get_running_loop()
        if await loop.run_in_executor(None, self.accounts.verify, username, password):
            return username
        if await loop.run_in_executor(None, self.accounts.exists, username):
            self._failed(address, now, "wrong password", username)
            return None
        # Signing up. A blank token is a returning attendee's misspelled
        # name; a wrong one may be a guess, and counts against the address.
        if not token.strip():
            self._failed(address, now, "unknown username, and no session token", username)
            return None
        if self._blocked(address, username, now, token_guess=True):
            self.log.warning("sign-up refused for %r from %s: too many wrong session tokens", username[:32], address)
            return None
        if not self._token.accepts(token, now):
            self._failed(address, now, "unknown username, and a wrong session token", username, token_guess=True)
            return None
        if self._is_admin(username):
            self._failed(address, now, "an admin's account is made on the machine, not by signing up", username)
            return None
        if not re.fullmatch(self.valid_username, username):
            self._failed(address, now, "username not allowed", username)
            return None
        if len(password) < self.min_password_length:
            self._failed(address, now, "password too short", username)
            return None
        if not await loop.run_in_executor(None, self.accounts.create, username, password):
            # Taken a moment ago by someone else (their password differs, or
            # verify() above would have let this one in).
            self._failed(address, now, "username taken", username)
            return None
        self.log.info("created account %r from %s", username, address)
        return username
