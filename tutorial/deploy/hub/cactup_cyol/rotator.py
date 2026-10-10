"""Rotate the session token: run by the hub as a managed service.

    python3 -m cactup_cyol.rotator

It keeps the token state (read by the authenticator) and a copy of the
current token for people: the file `session-token` in the shared directory,
which the host bind-mounts (anyone in its group reads it with `cat`, or with
`tutorial/deploy/token`, which prints it big for a projector). A new token
replaces the old one every CYOL_TOKEN_ROTATE minutes, or as soon as the file
`rotate` appears in the shared directory (`tutorial/deploy/token --rotate`
creates it); the old one keeps working for CYOL_TOKEN_GRACE minutes.

Settings, from the environment:
    CYOL_STATE           the state file (default /srv/jupyterhub/cyol-token.json)
    CYOL_SHARED          the shared directory (default /srv/cactup-tutorial)
    CYOL_SHARED_GID      the group that may read the token there (default: the directory's)
    CYOL_TOKEN_ROTATE    minutes between tokens (default 60; 0 never rotates by time)
    CYOL_TOKEN_GRACE     minutes the previous token stays valid (default 5)
"""

from __future__ import annotations

import os
import signal
import sys
import time
from pathlib import Path

from .session import State, generate, read_state, write_atomic, write_state

POLL = 2.0


class Rotator:
    def __init__(self, state_path: Path, shared: Path, rotate_minutes: float, grace_minutes: float,
                 gid: int | None = None, clock=time.time):
        self.state_path = state_path
        self.shared = shared
        self.interval = rotate_minutes * 60
        self.grace = grace_minutes * 60
        self.gid = gid
        self.clock = clock

    @property
    def public(self) -> Path:
        return self.shared / "session-token"

    @property
    def request(self) -> Path:
        return self.shared / "rotate"

    def publish(self, state: State) -> None:
        write_atomic(self.public, state.current + "\n", mode=0o640, gid=self.gid)

    def tick(self) -> State:
        """One round: rotate if it is time or someone asked; keep the public
        copy in step either way. Returns the state now in force."""
        now = self.clock()
        state = read_state(self.state_path)
        asked = self.request.exists()
        due = state is None or (self.interval > 0 and now - state.since >= self.interval)
        if due or asked:
            token = generate()
            state = State(current=token, since=now) if state is None else state.rotated(token, now, self.grace)
            write_state(self.state_path, state)
            self.publish(state)
            if asked:
                try:
                    self.request.unlink()
                except FileNotFoundError:
                    pass
        else:
            try:
                in_sync = self.public.read_text().strip() == state.current
            except OSError:
                in_sync = False
            if not in_sync:
                self.publish(state)
        return state


def main() -> int:
    shared = Path(os.environ.get("CYOL_SHARED", "/srv/cactup-tutorial"))
    shared.mkdir(parents=True, exist_ok=True)
    gid_text = os.environ.get("CYOL_SHARED_GID", "")
    gid = int(gid_text) if gid_text else shared.stat().st_gid
    rotator = Rotator(
        state_path=Path(os.environ.get("CYOL_STATE", "/srv/jupyterhub/cyol-token.json")),
        shared=shared,
        rotate_minutes=float(os.environ.get("CYOL_TOKEN_ROTATE", "60")),
        grace_minutes=float(os.environ.get("CYOL_TOKEN_GRACE", "5")),
        gid=gid,
    )
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(0))
    while True:
        try:
            rotator.tick()
        except OSError as e:
            print(f"cyol rotator: {e}", file=sys.stderr, flush=True)
        time.sleep(POLL)


if __name__ == "__main__":
    sys.exit(main())
