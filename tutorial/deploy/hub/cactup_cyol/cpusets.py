"""Which cores each attendee's container gets (its cpuset).

The machine's cores are cut into slices of CPUS_PER_ATTENDEE; a starting
container gets the least used slice. "Used" counts both what Docker says the
running containers have and what this hub handed out itself: a room logging
in at once starts many containers before any of them exists in Docker, and
they must not all get the first slice. A hub that restarts forgets what it
handed out, but the containers still running tell it.
"""

from __future__ import annotations

import threading


def parse_cpu_list(text: str) -> list[int]:
    """"0-3,8,10-11" -> [0, 1, 2, 3, 8, 10, 11]."""
    cpus = []
    for part in text.strip().split(","):
        if not part:
            continue
        lo, _, hi = part.partition("-")
        cpus.extend(range(int(lo), int(hi or lo) + 1))
    return cpus


def slices(cpus: list[int], per_attendee: int) -> list[str]:
    """Whole slices only; cores left over (fewer than per_attendee) go unused.
    With fewer cores than one slice, the one slice is all of them."""
    n = per_attendee
    whole = [cpus[i:i + n] for i in range(0, len(cpus) - n + 1, n)] or [cpus]
    return [",".join(map(str, s)) for s in whole]


class Assigner:
    def __init__(self, slices: list[str]):
        self.slices = slices
        self.given: dict[str, str] = {}
        self.lock = threading.Lock()

    def assign(self, name: str, running: dict[str, str]) -> str:
        """The slice for container `name`. `running` maps the attendees'
        containers Docker has now to their cpusets."""
        with self.lock:
            use = {s: 0 for s in self.slices}
            holders = {**running, **self.given}
            holders.pop(name, None)
            for cpuset in holders.values():
                if cpuset in use:
                    use[cpuset] += 1
            chosen = min(self.slices, key=lambda s: (use[s], self.slices.index(s)))
            self.given[name] = chosen
            return chosen

    def release(self, name: str) -> None:
        with self.lock:
            self.given.pop(name, None)
