"""The session token: a silly, memorable sentence that makes new accounts.

    noun verb number adjective noun punctuation    ("lantern juggled 42 calm harps!")

after the template of Steve Brandt's password generator
(https://www.cct.lsu.edu/~sbrandt/passwds.txt), with word lists of our own.

The rotator (rotator.py) writes the token state; the authenticator only
reads it. The state holds the current token and, for a grace period after a
rotation, the previous one, so someone typing the old token as it changes
isn't turned away.
"""

from __future__ import annotations

import json
import os
import re
import secrets
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path

WORDS = Path(__file__).parent / "words"
PUNCTUATION = "!?."


def load_words(name: str) -> list[str]:
    words = []
    for line in (WORDS / f"{name}.txt").read_text().splitlines():
        line = line.strip()
        if line and not line.startswith("#"):
            words.append(line)
    return words


def plural(noun: str) -> str:
    if noun.endswith(("s", "x", "z", "ch", "sh")):
        return noun + "es"
    if noun.endswith("y") and noun[-2:-1] not in "aeiou":
        return noun[:-1] + "ies"
    return noun + "s"


def generate(rng: secrets.SystemRandom | None = None) -> str:
    rng = rng or secrets.SystemRandom()
    nouns, verbs, adjectives = load_words("nouns"), load_words("verbs"), load_words("adjectives")
    return (f"{rng.choice(nouns)} {rng.choice(verbs)} {rng.randint(2, 99)} "
            f"{rng.choice(adjectives)} {plural(rng.choice(nouns))}{rng.choice(PUNCTUATION)}")


def normalize(text: str) -> str:
    """What has to match: the words and the number, whatever the case,
    spacing or punctuation someone typed."""
    return " ".join(re.findall(r"[a-z0-9]+", text.lower()))


@dataclass
class State:
    current: str
    since: float
    previous: str | None = None
    previous_until: float = 0.0

    def accepts(self, text: str, now: float) -> bool:
        typed = normalize(text)
        if not typed:
            return False
        if secrets.compare_digest(typed, normalize(self.current)):
            return True
        return (self.previous is not None and now < self.previous_until
                and secrets.compare_digest(typed, normalize(self.previous)))

    def rotated(self, token: str, now: float, grace: float) -> "State":
        return State(current=token, since=now, previous=self.current, previous_until=now + grace)


def read_state(path: Path) -> State | None:
    try:
        data = json.loads(path.read_text())
        return State(**data)
    except (OSError, ValueError, TypeError):
        return None


def write_atomic(path: Path, text: str, mode: int = 0o600, gid: int | None = None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp = tempfile.mkstemp(dir=path.parent, prefix=f".{path.name}.")
    try:
        with os.fdopen(fd, "w") as f:
            f.write(text)
        os.chmod(tmp, mode)
        if gid is not None:
            os.chown(tmp, -1, gid)
        os.replace(tmp, path)
    except BaseException:
        try:
            os.unlink(tmp)
        except FileNotFoundError:
            pass
        raise


def write_state(path: Path, state: State) -> None:
    write_atomic(path, json.dumps(state.__dict__))


class Reader:
    """The authenticator's view of the state, reread when the file changes."""

    def __init__(self, path: str | Path):
        self.path = Path(path)
        self._stamp: tuple[int, int] | None = None
        self._state: State | None = None

    def state(self) -> State | None:
        try:
            st = self.path.stat()
        except FileNotFoundError:
            return None
        stamp = (st.st_mtime_ns, st.st_size)
        if stamp != self._stamp:
            self._state = read_state(self.path)
            self._stamp = stamp
        return self._state

    def accepts(self, text: str, now: float | None = None) -> bool:
        state = self.state()
        return state is not None and state.accepts(text, time.time() if now is None else now)
