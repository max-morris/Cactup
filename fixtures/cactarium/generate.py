#!/usr/bin/env python3
# Rewrites fixtures/cactarium/pool/ from the definitions below: python3 fixtures/cactarium/generate.py
"""The package pool the resolver's table tests read (src/deps/tests.rs).

Each pool file is one `GET /packages/{o}/{n}/versions` answer (`PackageVersions`, every field as
the Cactarium serves it), or under pool/snapshots/ one `GET .../versions/{commit|head|master}`
answer wrapped as {"slug", "at", "untrusted", "served"}. A case's case.json names the pool files
it uses ("packages", "snapshots"); case.json files are edited by hand, never generated.

Commits are the sha1 of "owner/name@version", so a case can name one (`commit()` below). A
version may carry "review": "pending" (S6), which the crate reads as `VersionInfo.review`.
Standard library only.
"""
import hashlib
import json
import os
import shutil

HERE = os.path.dirname(os.path.abspath(__file__))
POOL_DIR = os.path.join(HERE, "pool")

FACT_KEYS = ["implements", "inherits", "friends", "requires-functions", "uses-functions",
             "provides-functions", "uses-includes", "requires", "requires-thorns", "optional", "provides"]


def commit(slug, version):
    return hashlib.sha1(f"{slug}@{version}".encode()).hexdigest()


def T(name, arrangement=None, **facts):
    """A thorn: facts as keyword arguments in snake case (implements defaults to the name)."""
    return {"name": name, "arrangement": arrangement, "facts": facts}


def thorn_info(slug, t):
    if isinstance(t, str):
        t = T(t)
    facts = {}
    for k in FACT_KEYS:
        py = k.replace("-", "_")
        facts[k] = t["facts"].get(py, t["name"]) if k == "implements" else t["facts"].get(py, [])
    arrangement = t["arrangement"] or slug.split("/")[1].capitalize()
    return {"arrangement": arrangement, "thorn": t["name"], "repo-path": "$1/$2", "facts": facts}


def dep(target, range=None, needs=(), needed_by=(), sha=None, into=None, any_of=None):
    c = {"kind": "dependency", "target": target, "range": range}
    if sha:
        c["sha"] = sha
    if needs:
        c["needs"] = list(needs)
    if into:
        c["into"] = into
    if needed_by:
        c["needed-by"] = list(needed_by)
    if any_of:
        alts = [{"target": a[0], "range": a[1], "needs": list(a[2])} for a in any_of]
        c["alternatives"] = alts
        c["target"], c["range"] = alts[0]["target"], alts[0]["range"]
        c["needs"] = alts[0]["needs"]
    c["note"] = None
    return c


def fork_of(target, range=None, sha=None):
    c = {"kind": "fork-of", "target": target, "range": range}
    if sha:
        c["sha"] = sha
    c["note"] = None
    return c


def incompatible(target, range=None):
    return {"kind": "incompatible-with", "target": target, "range": range, "note": None}


def V(version, thorns=(), claims=(), yanked=False, pending=False, flesh=False, library=False, state="ok"):
    return dict(version=version, thorns=thorns, claims=claims, yanked=yanked, pending=pending,
                flesh=flesh, library=library, state=state)


def served(slug, v):
    version = v["version"]
    track = version.split(":")[0] if ":" in version else None
    plain = version.split(":")[-1]
    out = {
        "version": version,
        "track": track,
        "tag": f"{track}/v{plain}" if track else f"v{plain}",
        "commit": commit(slug, version),
        "tree": None,
        "yanked": v["yanked"],
        "yank-reason": "broken build" if v["yanked"] else None,
        "state": v["state"],
        "curated": False,
        "published": 1793000000,
    }
    if v["pending"]:
        out["review"] = "pending"
    out["url"] = f"https://github.com/{slug}.git"
    if v["flesh"]:
        out["flesh"] = True
    if v["library"]:
        out["library"] = True
    out["thorns"] = [thorn_info(slug, t) for t in v["thorns"]]
    out["claims"] = list(v["claims"])
    return out


def snapshot(slug, at, found, thorns=(), claims=(), untrusted=True, library=False):
    """The package read at `at` (a commit, or head/master), found at commit `found`: the server
    answers a commit read with the full commit as the version, a tip read with the tip's name."""
    pv = served(slug, V(at if at in ("head", "master") else found, thorns, claims, library=library))
    pv["tag"] = None
    pv["commit"] = found
    pv["track"] = None
    pv["tree"] = "0" * 40
    return {"slug": slug, "at": at, "untrusted": untrusted, "served": pv}


Z4C = "max/z4c"
Z4C_THORNS = ["Z4c", "Z4cTest"]
ANYOF = dep("et/carpet", any_of=[("et/carpet", None, ["CarpetSlab"]), ("et/cactuspugh", None, ["PUGHSlab"])])
CX = ["CarpetX", "Loop", "Arith"]
C551 = commit("et/carpetx", "2026.5.1")
CX_SNAP = "4444444444444444444444444444444444444444"

FUKA_SNAP = "9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a"
POOL = {
    # Packages many cases share.
    "et--carpetx": ("et/carpetx", [V("2026.5.1", CX), V("2026.5.0", CX), V("2026.4.0", CX)]),
    "et--carpet": ("et/carpet", [V("2026.5.0", ["Carpet", "CarpetSlab"])]),
    "et--carpet--two": ("et/carpet", [V("2026.6.0", ["Carpet", "CarpetSlab"]), V("2026.5.0", ["Carpet", "CarpetSlab"])]),
    "et--carpet--hyperslab": ("et/carpet", [V("2026.5.0", ["Carpet", T("CarpetSlab", implements="Hyperslab")])]),
    "et--cactuspugh": ("et/cactuspugh", [V("2026.5.0", ["PUGH", "PUGHSlab", "PUGHReduce"])]),
    "et--admbase": ("et/admbase", [V("2026.5.1", ["ADMBase", "StaticConformal"]),
                                   V("2026.5.0", ["ADMBase", "StaticConformal"]),
                                   V("2026.4.0", ["ADMBase", "StaticConformal"])]),
    "et--admbase--dropped-sc": ("et/admbase", [V("2026.5.1", ["ADMBase"]), V("2026.5.0", ["ADMBase", "StaticConformal"]),
                                               V("2026.4.0", ["ADMBase", "StaticConformal"])]),
    "et--hydrobase": ("et/hydrobase", [V("1.1.0", ["HydroBase"]), V("mixed-precision:1.5.0", ["HydroBase"]),
                                       V("1.0.0", ["HydroBase"])]),
    "et--tmunubase": ("et/tmunubase", [V("2026.5.0", ["TmunuBase"])]),
    "et--arith": ("et/arith", [V("2.0.0", ["Arith"]), V("1.3.0", ["Arith"]), V("1.2.0", ["Arith"])]),
    "et--arith--yanked": ("et/arith", [V("1.3.0", ["Arith"], yanked=True), V("1.2.0", ["Arith"])]),
    "et--arith--invalid": ("et/arith", [V("1.3.0", ["Arith"], state="invalid"), V("1.2.0", ["Arith"], state="tag-moved")]),
    "et--testlib": ("et/testlib", [V("1.0.0", ["TestLib"])]),
    "et--fuka": ("et/fuka", [V("2.0.0", library=True), V("1.2.0", library=True), V("1.1.5", library=True)]),
    "et--cactus": ("et/cactus", [V("2026.5.0", flesh=True), V("2026.4.0", flesh=True)]),
    "et--cactus--old": ("et/cactus", [V("2026.4.0", flesh=True)]),
    "max--carpetx--tracks": ("max/carpetx", [V("1.9.0", ["CarpetX"]), V("mixed-precision:1.4.0", ["CarpetX"]),
                                             V("mixed-precision:1.3.0", ["CarpetX"])]),
    "et--carpetx--pending": ("et/carpetx", [V("2026.6.0", ["CarpetX", "Loop"], pending=True),
                                            V("2026.5.1", ["CarpetX", "Loop"])]),

    # max/z4c, with the claims each case gives it.
    "max--z4c": (Z4C, [V("1.2.0", Z4C_THORNS)]),
    "max--z4c--carpetx-arith": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", "^2026.5", ["CarpetX", "Loop"], ["Z4c"])])]),
    "et--carpetx--needs-arith": ("et/carpetx", [V("2026.5.1", CX, [dep("et/arith", "^1", ["Arith"], ["CarpetX"])]),
                                                V("2026.4.0", CX)]),
    "max--z4c--needed-by": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", "^2026.5", ["CarpetX"], ["Z4c"]),
                                                         dep("et/testlib", None, ["TestLib"], ["Z4cTest"])])]),
    "max--z4c--anyof": (Z4C, [V("1.2.0", Z4C_THORNS, [ANYOF])]),
    "max--z4c--anyof-and-pugh": (Z4C, [V("1.2.0", Z4C_THORNS, [ANYOF, dep("et/cactuspugh", None, ["PUGHReduce"])])]),
    "max--z4c--anyof-carpet-2026-6": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpet", any_of=[
        ("et/carpet", "^2026.6", ["CarpetSlab"]), ("et/cactuspugh", None, ["PUGHSlab"])])])]),
    "max--z4c--anyof-carpet-2026-5": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpet", any_of=[
        ("et/carpet", "~2026.5", ["CarpetSlab"]), ("et/cactuspugh", None, ["PUGHSlab"])])])]),
    "max--z4c--carpetx": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", "^2026.5", ["CarpetX"])])]),
    "max--z4c--carpetx-loop": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", "^2026.5", ["CarpetX", "Loop"])])]),
    "max--z4c--carpetx-2026-6": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", "^2026.6", ["CarpetX"])])]),
    "max--z4c--carpetx-sha": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", sha=C551[:12], needs=["CarpetX"])])]),
    "max--z4c--carpetx-snapshot": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", sha=CX_SNAP[:10], needs=["CarpetX"])])]),
    "max--z4c--carpetx-grhydro": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", "^2026.5", ["CarpetX"]),
                                                               dep("max/grhydro", "^1", ["GRHydro"])])]),
    "max--grhydro": ("max/grhydro", [V("1.0.0", ["GRHydro"], [dep("et/carpetx", "~2026.4", ["CarpetX"])])]),
    "max--z4c--admbase-2026-5": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/admbase", "^2026.5", ["ADMBase"])])]),
    "max--z4c--admbase-2026-4": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/admbase", "^2026.4", ["ADMBase"])])]),
    "max--z4c--tracked-carpetx": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("max/carpetx", "mixed-precision:^1.2", ["CarpetX"])])]),
    "max--z4c--default-track-carpetx": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("max/carpetx", "^1.4", ["CarpetX"])])]),
    "max--z4c--arith-1": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/arith", "^1", ["Arith"])])]),
    "max--z4c--arith-exact": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/arith", "=1.3.0", ["Arith"])])]),
    "max--z4c--hyperslab": (Z4C, [V("1.2.0", [T("Z4c", inherits=["hyperslab"]), "Z4cTest"])]),
    "max--z4c--grid": (Z4C, [V("1.2.0", [T("Z4c", inherits=["GRID"], requires_functions=["GetBoundary"]), "Z4cTest"])]),
    "max--z4c--carpet-hyperslab": (Z4C, [V("1.2.0", [T("Z4c", inherits=["Hyperslab"]), "Z4cTest"],
                                          [dep("et/carpet", None, ["CarpetSlab"])])]),
    "max--z4c--carpetx-fuka": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", "^2026.5", ["CarpetX"]),
                                                            dep("et/fuka", "^1", into="src/fuka")])]),
    "max--z4c--cactus": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/cactus", "^2026.5")])]),
    "max--z4c--nothere": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/nothere", None, ["X"])])]),
    # Facts as the scanner reads Z4c's real interface.ccl and configuration.ccl (trimmed).
    "max--z4c--real-ccl": (Z4C, [V("1.2.0", [
        T("Z4c", inherits=["ADMBase", "TmunuBase"], uses_includes=["loop_device.hxx"],
          requires_thorns=["Arith", "Loop"], uses_functions=["Boundary_SelectGroupForBC"]),
        T("Z4cTest", inherits=["Z4c"], requires_thorns=["Loop"])],
        [dep("et/carpetx", "^2026.5", ["Arith", "Loop"], ["Z4c", "Z4cTest"]),
         dep("et/admbase", "^2026.5", ["ADMBase"], ["Z4c"]),
         dep("et/tmunubase", None, ["TmunuBase"], ["Z4c"])])]),
    "et--carpetx--real-ccl": ("et/carpetx", [V("2026.5.1", [
        T("CarpetX", inherits=["IO"], requires=["AMReX", "MPI"], requires_thorns=["Arith", "Loop"]),
        T("Loop", provides_functions=["GetCallFunctionCount"]),
        T("Arith")])]),

    # Forks.
    "max--carpetx-mp--covers-2026-5-1": ("max/carpetx-mp", [V("1.0.0", ["CarpetX", "Loop"], [fork_of("et/carpetx", "=2026.5.1")])]),
    "max--carpetx-mp--sha": ("max/carpetx-mp", [V("1.0.0", ["CarpetX", "Loop"], [fork_of("et/carpetx", sha=C551)])]),
    "max--carpetx-mp--covers-2026-4": ("max/carpetx-mp", [V("1.0.0", ["CarpetX", "Loop"], [fork_of("et/carpetx", "=2026.4.0")])]),
    "max--carpetx-mp--whole": ("max/carpetx-mp", [V("1.0.0", CX, [fork_of("et/carpetx", "=2026.5.1")])]),
    "max--carpetx-mp--carpetx-only": ("max/carpetx-mp", [V("1.0.0", ["CarpetX"], [fork_of("et/carpetx", "=2026.5.1")])]),
    "max--carpetx-mp--unrelated-arith": ("max/carpetx-mp", [V("1.0.0", [T("CarpetX", inherits=["arith"])])]),
    "max--cx-lower": ("max/cx-lower", [V("1.0.0", ["carpetx"])]),

    # Incompatibilities.
    "max--a--incompatible-b": ("max/a", [V("1.0.0", ["A"], [incompatible("et/b", "^1")])]),
    "max--a--incompatible-b2": ("max/a", [V("1.0.0", ["A"], [incompatible("et/b", "^2")])]),
    "max--a": ("max/a", [V("1.0.0", ["A"])]),
    "et--b": ("et/b", [V("1.5.0", ["B"])]),
    "et--b--incompatible-a": ("et/b", [V("1.5.0", ["B"], [incompatible("max/a")])]),

    # Any-of order.
    "max--a--anyof-p-q": ("max/a", [V("1.0.0", ["A"], [dep("et/p", any_of=[("et/p", None, ["P"]), ("et/q", None, ["Q"])])])]),
    "max--b--anyof-q-p": ("max/b", [V("1.0.0", ["B"], [dep("et/q", any_of=[("et/q", None, ["Q"]), ("et/p", None, ["P"])])])]),
    "et--p": ("et/p", [V("1.0.0", ["P"])]),
    "et--q": ("et/q", [V("1.0.0", ["Q"])]),

    # Restarts.
    "max--a--needs-b-c": ("max/a", [V("1.0.0", ["A"], [dep("et/b", None, ["B"]), dep("et/c", None, ["C"])])]),
    "et--b--needs-d": ("et/b", [V("2.0.0", ["B"], [dep("et/d", "^2", ["D"])]), V("1.9.0", ["B"], [dep("et/d", "^1", ["D"])])]),
    "et--c--needs-b1": ("et/c", [V("1.0.0", ["C"], [dep("et/b", "^1", ["B"])])]),
    "et--d": ("et/d", [V("2.0.0", ["D"]), V("1.5.0", ["D"]), V("1.0.0", ["D"])]),
    "max--x--needs-p-q": ("max/x", [V("1.0.0", ["X"], [dep("et/p", None, ["P"]), dep("et/q", None, ["Q"])])]),
    "et--p--swings": ("et/p", [V("2.0.0", ["P"], [dep("et/q", "^1", ["Q"])]), V("1.0.0", ["P"], [dep("et/q", "^2", ["Q"])])]),
    "et--q--swings": ("et/q", [V("2.0.0", ["Q"], [dep("et/p", "^2", ["P"])]), V("1.0.0", ["Q"], [dep("et/p", "^1", ["P"])])]),
    "et--p--drops-d": ("et/p", [V("2.0.0", ["P"]), V("1.0.0", ["P"], [dep("et/d", "^1", ["D"])])]),
    "et--p--needs-d": ("et/p", [V("2.0.0", ["P"], [dep("et/d", "^2", ["D"])]), V("1.0.0", ["P"], [dep("et/d", "^1", ["D"])])]),
    # Picks invalidated: et/x 2.0.0 holds et/c down, until et/y moves et/x to 1.0.0.
    "et--c": ("et/c", [V("2.0.0", ["C"]), V("1.9.0", ["C"])]),
    "et--c--pending": ("et/c", [V("3.0.0", ["C"], pending=True), V("2.0.0", ["C"])]),
    "et--x--needs-c1": ("et/x", [V("2.0.0", ["X"], [dep("et/c", "^1", ["C"])]), V("1.0.0", ["X"])]),
    "et--x--needs-c3": ("et/x", [V("2.0.0", ["X"], [dep("et/c", "^3", ["C"])]), V("1.0.0", ["X"])]),
    "et--y--needs-x1": ("et/y", [V("1.0.0", ["Y"], [dep("et/x", "^1", ["X"])])]),
    # A fork reached by a longer path than its upstream.
    "max--app--b-then-c": ("max/app", [V("1.0.0", ["App"], [dep("et/lib-b", None, ["B"]), dep("et/lib-c", None, ["C"])])]),
    "max--app--c-then-b": ("max/app", [V("1.0.0", ["App"], [dep("et/lib-c", None, ["C"]), dep("et/lib-b", None, ["B"])])]),
    "et--lib-b": ("et/lib-b", [V("1.0.0", ["B"], [dep("et/upstream", None, ["Up"])])]),
    "et--lib-c": ("et/lib-c", [V("1.0.0", ["C"], [dep("et/lib-d", None, ["D"])])]),
    "et--lib-d": ("et/lib-d", [V("1.0.0", ["D"], [dep("max/fork", None, ["Up"])])]),
    "et--upstream": ("et/upstream", [V("1.0.0", ["Up"])]),
    "max--fork": ("max/fork", [V("1.0.0", ["Up"], [fork_of("et/upstream", "=1.0.0")])]),

    # Two states that each need the other's other release (N1).
    "et--p--n1": ("et/p", [V("2.0.0", ["P"]), V("1.0.0", ["P"], [dep("et/q", "^1", ["Q"])])]),
    "et--q--n1": ("et/q", [V("2.0.0", ["Q"], [dep("et/p", "^1", ["P"])]), V("1.0.0", ["Q"], [dep("et/p", "^2", ["P"])])]),
    "et--c--with-1": ("et/c", [V("2.0.0", ["C"]), V("1.9.0", ["C"]), V("1.0.0", ["C"])]),
    "et--c--pending-with-1": ("et/c", [V("3.0.0", ["C"], pending=True), V("2.0.0", ["C"]), V("1.0.0", ["C"])]),
    "max--carpetx-mp--whole-2026-4": ("max/carpetx-mp", [V("1.0.0", CX, [fork_of("et/carpetx", "=2026.4.0")])]),
    "max--carpetx-mp--two-claims": ("max/carpetx-mp", [V("1.0.0", ["CarpetX", "Loop"], [
        fork_of("et/carpetx", "=2026.4.0"), fork_of("et/carpetx", "=2026.5.1")])]),
    "max--carpetx-xp--covers-2026-5-1": ("max/carpetx-xp", [V("1.0.0", ["CarpetX", "Loop"], [fork_of("et/carpetx", "=2026.5.1")])]),
    "max--carpetx-mp--bad-sha": ("max/carpetx-mp", [V("1.0.0", ["CarpetX", "Loop"], [fork_of("et/carpetx", sha="not-a-commit")])]),
    "max--kadath2": ("max/kadath2", [V("1.0.0", ["Kadath2"], [dep("et/fuka", "^1", into="src/fuka")])]),
    "et--aslab": ("et/aslab", [V("1.0.0", ["CarpetSlab"])]),
    "max--z4c--carpet-plain": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpet", None, ["CarpetSlab"])])]),
    "et--a-flesh": ("et/a-flesh", [V("1.0.0", flesh=True)]),
    # The fork reached later, with the package bringing it sorting after the upstream (H2).
    "max--app--b-then-c-zz": ("max/app", [V("1.0.0", ["App"], [dep("et/lib-b", None, ["B"]), dep("et/lib-c-zz", None, ["C"])])]),
    "et--lib-c-zz": ("et/lib-c-zz", [V("1.0.0", ["C"], [dep("et/zz-d", None, ["D"])])]),
    "et--zz-d": ("et/zz-d", [V("1.0.0", ["D"], [dep("max/fork", None, ["Up"])])]),
    # A present package that must move for what its new thorn needs (H3).
    "max--z4c--two-arith": (Z4C, [V("2.0.0", Z4C_THORNS, [dep("et/arith", "^2", ["Arith"], ["Z4cTest"])]),
                                  V("1.0.0", Z4C_THORNS, [dep("et/arith", "^1", ["Arith"], ["Z4cTest"])])]),
    "max--z4c--arith-exact-for-test": (Z4C, [V("1.0.0", Z4C_THORNS, [dep("et/arith", "=1.3.0", ["Arith"], ["Z4cTest"])])]),
    "et--arith--two-only": ("et/arith", [V("2.0.0", ["Arith"])]),
    # A later decision takes a conflicting package out of play (C1, C2).
    "max--app--c1": ("max/app", [V("1.0.0", ["App"], [incompatible("et/upstream"), dep("et/b", None, ["B"]),
                                                     dep("et/z", None, ["Z"])])]),
    "et--b--c1": ("et/b", [V("1.0.0", ["B"], [dep("et/upstream", None, ["Up"])])]),
    "et--z--c1": ("et/z", [V("2.0.0", ["Z"]), V("1.0.0", ["Z"], [dep("max/fork", None, ["Up"])])]),
    "et--z--sat": ("et/z", [V("2.0.0", ["Z"]), V("1.0.0", ["Z"], [dep("et/sat", None, ["Up"])])]),
    "et--sat": ("et/sat", [V("1.0.0", ["Up"])]),
    "max--n--incompatible-bb": ("max/n", [V("1.0.0", ["N"], [incompatible("et/bb"), dep("et/zr", None, ["Z"])])]),
    "et--zr": ("et/zr", [V("2.0.0", ["Z"]), V("1.0.0", ["Z"], [dep("max/bbfork", None, ["BB"])])]),
    "max--bbfork": ("max/bbfork", [V("1.0.0", ["BB"], [fork_of("et/bb", "=1.0.0")])]),
    "et--bb": ("et/bb", [V("1.0.0", ["BB"])]),
    "max--n2--ft": ("max/n2", [V("1.0.0", ["FT"], [dep("et/zf", None, ["Z"])])]),
    "et--zf": ("et/zf", [V("2.0.0", ["Z"]), V("1.0.0", ["Z"], [dep("et/newflesh", None)])]),
    "et--newflesh": ("et/newflesh", [V("1.0.0", flesh=True)]),
    "et--oldflesh": ("et/oldflesh", [V("1.0.0", ["FT"], flesh=True)]),
    # A present package that is no flesh, whose newer release is one: a flesh coming in can
    # still replace it once it moves.
    "et--pflesh": ("et/pflesh", [V("2.0.0", ["FT"], flesh=True), V("1.0.0", ["FT"])]),
    # An untouched present package that may still move (C3), and one whose other release
    # provides what a requirement needs (C4).
    "max--n--c3": ("max/n", [V("1.0.0", ["N"], [dep("et/t", "^2", ["T"]), dep("et/x", None, ["X"])])]),
    "et--t--c3": ("et/t", [V("2.0.0", ["T"]), V("1.0.0", ["T"])]),
    "et--x--c3": ("et/x", [V("2.0.0", ["X"]), V("1.0.0", ["X"], [dep("et/p3", "^2", ["P"])])]),
    "et--p3": ("et/p3", [V("2.0.0", ["P"], [dep("et/t", "^2", ["T"])]), V("1.0.0", ["P"], [dep("et/t", "^1", ["T"])])]),
    "max--n--c4": ("max/n", [V("1.0.0", [T("N", requires=["C"])], [dep("et/x4", None, ["X"])])]),
    "et--x4": ("et/x4", [V("2.0.0", ["X"]), V("1.0.0", ["X"], [dep("et/p4", "^2", ["PC"])])]),
    "et--p4": ("et/p4", [V("2.0.0", ["P", T("PC", provides=["C"])]), V("1.0.0", ["P"])]),
    "max--z4c--carpetx-missing-commit": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", sha="5555555555", needs=["CarpetX"])])]),
    "max--carpetx-mp--arithx": ("max/carpetx-mp", [V("1.0.0", ["CarpetX", T("Arith", implements="arithx")])]),
    "max--new--needs-z4ctest": ("max/new", [V("1.0.0", ["New"], [dep("max/z4c", None, ["Z4cTest"])])]),
    "et--carpet--newest-incompatible": ("et/carpet", [V("2026.6.0", ["Carpet", "CarpetSlab"], [incompatible("et/b")]),
                                                       V("2026.5.0", ["Carpet", "CarpetSlab"])]),

    # Passive constraints.
    "max--needs-sc": ("max/needs-sc", [V("1.0.0", ["NeedsSC"], [dep("et/admbase", None, ["StaticConformal"])])]),
    "max--other--carpet-2026-6": ("max/other", [V("1.0.0", ["Other"], [dep("et/carpet", "^2026.6", ["CarpetSlab"])])]),

    # Flesh.
    "max--cactus-mp--covers-2026-4": ("max/cactus-mp", [V("1.0.0", flesh=True, claims=[fork_of("et/cactus", "=2026.4.0")])]),
    "max--cactus-other": ("max/cactus-other", [V("1.0.0", flesh=True)]),

    # Libraries.
    "max--kadath": ("max/kadath", [V("1.0.0", ["Kadath"], [dep("et/fuka", "^1", into="src/fuka")])]),
    "max--fuka-gw--fuka-2": ("max/fuka-gw", [V("1.0.0", ["FukaGW"], [dep("et/fuka", "^2", into="lib/fuka")])]),
    "max--fuka-gw--fuka-1": ("max/fuka-gw", [V("1.0.0", ["FukaGW"], [dep("et/fuka", "^1", into="lib/fuka")])]),
    "max--fuka-gw--fuka-1-1": ("max/fuka-gw", [V("1.0.0", ["FukaGW"], [dep("et/fuka", "~1.1", into="lib/fuka")])]),
    "max--carpet-mp--two": ("max/carpet-mp", [V("2.0.0", ["CarpetSlab"], [fork_of("et/carpet", "=2026.6.0")]),
                                              V("1.0.0", ["CarpetSlab"], [fork_of("et/carpet", "=2026.5.0")])]),
    "et--carpetx--arith-needs-lib": ("et/carpetx", [V("2026.5.1", ["CarpetX", "Loop", "Arith"],
                                                      [dep("max/lib", "^1.0", ["Lib"], needed_by=["Arith"])])]),
    "et--carpetx--both-need-lib": ("et/carpetx", [V("2026.5.1", ["CarpetX", "Loop", "Arith"],
                                                     [dep("max/lib", "^1.0", ["Lib"], needed_by=["CarpetX", "Arith"])])]),
    "et--carpetx--needs-lib": ("et/carpetx", [V("2026.5.1", ["CarpetX", "Loop", "Arith"], [dep("max/lib", "^1.0", ["Lib"])])]),
    "max--lib": ("max/lib", [V("1.0.0", ["Lib"])]),
    "et--p5": ("et/p5", [V("2.0.0", ["PA", "PC"]), V("1.0.0", ["PA"])]),
    "max--d--carpetslab": ("max/d", [V("1.0.0", ["D"], [dep("et/carpet", "^2026.5", ["CarpetSlab"])])]),
    "et--kadath--extra-needs-fuka": ("et/kadath", [V("1.0.0", ["Kadath", "KadathExtra"],
                                                     [dep("et/fuka", "^1", into="src/fuka", needed_by=["KadathExtra"])])]),
    "max--needs-kx": ("max/needs-kx", [V("1.0.0", ["NeedsKX"], [dep("et/kadath", None, ["KadathExtra"])])]),
    # Exact reads of base packages at unfrozen commits.
    "max--x--old-needs-adm": ("max/x", [V("2.0.0", ["X"]), V("1.0.0", ["X"], [dep("et/admbase", None, ["StaticConformal"])])]),
    "max--y--anyof-carpet-adm": ("max/y", [V("1.0.0", ["Y"], [dep("et/carpet", any_of=[
        ("et/carpet", None, ["CarpetSlab"]), ("et/admbase", None, ["StaticConformal"])])])]),
    "max--newcarpet": ("max/newcarpet", [V("1.0.0", ["Carpet"])]),
    "max--n10": ("max/n10", [V("1.0.0", ["N10"], [dep("et/p10", None, ["P10b"])])]),
    "et--p10": ("et/p10", [V("1.0.0", ["P10a", "P10b", "P10c"])]),
    "et--p11": ("et/p11", [V("1.0.0", ["Q0", "Q"])]),
    "et--p12": ("et/p12", [V("1.0.0", ["R0", "R"])]),
    "max--needs-adm-all": ("max/needs-adm-all", [V("1.0.0", ["NA"], [dep("et/admbase", None, [])])]),
    "max--kadath--into-carpetx": ("max/kadath", [V("1.0.0", ["Kadath"], [dep("et/carpetx", "^2026.5", into="src/carpetx")])]),
    # What an unread base package means: only its claims are unknown (the zz cases of the
    # fifth review).
    "max--zza": ("max/zza", [V("2.0.0", ["ZA"], [dep("et/admbase", None, ["StaticConformal"])]), V("1.0.0", ["ZA"])]),
    "max--zzf--forks-adm": ("max/zzf", [V("2.0.0", ["ADMBase", "FX"], [fork_of("et/admbase")]), V("1.0.0", ["FX"])]),
    "max--zzx": ("max/zzx", [V("1.0.0", ["ZX"], [dep("et/admbase", None, ["StaticConformal"]), dep("max/zzy", None, ["ZY"])])]),
    "max--zzy": ("max/zzy", [V("1.0.0", ["ZY"], [dep("et/admbase", "=2026.5.0", ["ADMBase"])])]),
    "et--zzf": ("et/zzf", [V("1.0.0", ["F1", "F2"], [dep("et/zzs", None, ["S2"], needed_by=["F2"])])]),
    "et--zzf--needs-every-zzs": ("et/zzf", [V("1.0.0", ["F1", "F2"], [dep("et/zzs", None, (), needed_by=["F2"])])]),
    "max--zzn": ("max/zzn", [V("1.0.0", ["N"], [dep("et/zzf", None, ["F2"])])]),
    "et--zzs": ("et/zzs", [V("1.0.0", ["S1", "S2"])]),
    "max--zzn2": ("max/zzn2", [V("1.0.0", ["N2"], [dep("et/zzq", None, ["A2"]), dep("max/zzy2", None, ["Y2"])])]),
    "max--zzy2": ("max/zzy2", [V("1.0.0", ["Y2"], [dep("et/zzq", None, ["A3"])])]),
    "max--zzg": ("max/zzg", [V("2.0.0", ["G"], [dep("et/admbase", None, ["StaticConformal"])]), V("1.0.0", ["G"])]),
    "max--zzh": ("max/zzh", [V("2.0.0", ["H"], [dep("max/zzg", "^1.0", ["G"])]), V("1.0.0", ["H"])]),
    "max--zzp--admbase-thorn": ("max/zzp", [V("1.0.0", ["ADMBase"])]),
    "max--needs-zzp": ("max/needs-zzp", [V("1.0.0", ["NP"], [dep("max/zzp", None, ["ADMBase"])])]),
    "max--zzc": ("max/zzc", [V("1.0.0", ["C"])]),
    "et--zzw--needs-zzq": ("et/zzw", [V("1.0.0", ["W"], [dep("et/zzq", None, ["A2"])])]),
    # A requirement a declared dependency would meet if it moved is that dependency's conflict.
    "max--fieldlib": ("max/fieldlib", [V("3.0.0", ["FieldA", "FieldD"]), V("mp:1.0.0", ["FieldA", "FieldMP"]),
                                       V("1.0.0", ["FieldA"])]),
    "max--duser": ("max/duser", [V("1.0.0", [T("DUser", requires_thorns=["FieldD"])],
                                   [dep("max/fieldlib", "^3", ["FieldD"], needed_by=["DUser"])])]),
    "max--mpuser": ("max/mpuser", [V("1.0.0", [T("MPUser", requires_thorns=["FieldMP"])],
                                     [dep("max/fieldlib", "mp:^1", ["FieldMP"], needed_by=["MPUser"])])]),
    "max--symuser": ("max/symuser", [V("1.0.0", [T("SymUser", requires_thorns=["SymBase"])],
                                       [dep("et/cactusbase", None, ["SymBase"], needed_by=["SymUser"])])]),
    "et--cactusbase": ("et/cactusbase", [V("2026.5.0", ["Boundary", "SymBase"])]),
    # Notes on releases left out for want of a read: from what each asks itself.
    "et--zpx": ("et/zpx", [V("2.0.0", ["ZX"], [dep("et/admbase", None, ["StaticConformal"])]), V("1.0.0", ["ZX"])]),
    "max--zpapp": ("max/zpapp", [V("2.0.0", ["ZP"], [dep("et/zpx", "^2", ["ZX"])]),
                                 V("1.0.0", ["ZP"], [dep("et/zpx", "^1", ["ZX"])])]),
    "max--zpapq": ("max/zpapq", [V("2.0.0", ["ZQ"], [dep("et/admbase", None, ["StaticConformal"]),
                                                    dep("et/cactusbase", None, ["SymBase"])]),
                                 V("1.0.0", ["ZQ"])]),
    "max--kadath--fuka-at-commit": ("max/kadath", [V("1.0.0", ["Kadath"], [dep("et/fuka", sha=FUKA_SNAP, into="src/fuka")])]),

    # Remove.
    "max--z4c--carpetx-plain": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/carpetx", "^2026.5", ["CarpetX"])])]),
    "et--carpetx--arith-1": ("et/carpetx", [V("2026.5.1", ["CarpetX", "Loop"], [dep("et/arith", "^1", ["Arith"])])]),
    "et--arith--one": ("et/arith", [V("1.3.0", ["Arith"])]),
    "et--dd--needs-o": ("et/dd", [V("1.0.0", ["DD"], [dep("et/o", None, ["O"])])]),
    "et--o": ("et/o", [V("1.0.0", ["O"])]),
    "max--z4c--cyc": (Z4C, [V("1.2.0", Z4C_THORNS, [dep("et/cyc-a", None, ["CycA"])])]),
    "et--cyc-a": ("et/cyc-a", [V("1.0.0", ["CycA"], [dep("et/cyc-b", None, ["CycB"])])]),
    "et--cyc-b": ("et/cyc-b", [V("1.0.0", ["CycB"], [dep("et/cyc-a", None, ["CycA"])])]),
}

ADM_SNAP = "abababababababababababababababababababab"
ADM2_SNAP = "efefefefefefefefefefefefefefefefefefefef"
P10_SNAP = "1010101010101010101010101010101010101010"
P11_SNAP = "1111111111111111111111111111111111111111"
P12_SNAP = "1212121212121212121212121212121212121212"
MP_SNAP = "cdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcdcd"
ZZQ_SNAP = "3434343434343434343434343434343434343434"
ZZC_SNAP = "7878787878787878787878787878787878787878"
ZZS_SNAP = "5656565656565656565656565656565656565656"
SNAPSHOTS = {
    "et--carpetx--at-4444": snapshot("et/carpetx", CX_SNAP, CX_SNAP, ["CarpetX", "Loop"]),
    # Base packages at commits no version froze, read when a plan depends on them.
    "et--admbase--at-abab": snapshot("et/admbase", ADM_SNAP, ADM_SNAP, ["ADMBase", "ADMCoupling", "StaticConformal"],
                                     untrusted=False),
    "et--admbase--at-efef": snapshot("et/admbase", ADM2_SNAP, ADM2_SNAP, ["ADMBase", "StaticConformal"], untrusted=False),
    "et--p10--at-1010": snapshot("et/p10", P10_SNAP, P10_SNAP, ["P10a", "P10b", "P10c"],
                                 [dep("et/p11", None, ["Q"], needed_by=["P10b"]), dep("et/p12", None, ["R"], needed_by=["P10c"])],
                                 untrusted=False),
    "et--p11--at-1111": snapshot("et/p11", P11_SNAP, P11_SNAP, ["Q0", "Q"], untrusted=False),
    "et--p12--at-1212": snapshot("et/p12", P12_SNAP, P12_SNAP, ["R0", "R"], untrusted=False),
    "max--carpet-mp--at-cdcd": snapshot("max/carpet-mp", MP_SNAP, MP_SNAP, ["Carpet", "CarpetSlab"],
                                        [fork_of("et/carpet", "=2026.5.0")], untrusted=False),
    "et--carpetx--at-head": snapshot("et/carpetx", "head", CX_SNAP, ["CarpetX", "Loop"]),
    "et--zzq--at-3434": snapshot("et/zzq", ZZQ_SNAP, ZZQ_SNAP, ["A1", "A2", "A3"],
                                 [dep("et/zzs", None, ["S2"], needed_by=["A3"])], untrusted=False),
    "et--zzs--at-5656": snapshot("et/zzs", ZZS_SNAP, ZZS_SNAP, ["S1", "S2"], untrusted=False),
    "et--zzq--at-3434--a3-needs-zzc-commit": snapshot("et/zzq", ZZQ_SNAP, ZZQ_SNAP, ["A1", "A2", "A3"],
                                                      [dep("max/zzc", sha=ZZC_SNAP, needs=["C"], needed_by=["A3"])],
                                                      untrusted=False),
    "max--zzc--at-7878": snapshot("max/zzc", ZZC_SNAP, ZZC_SNAP, ["C"], untrusted=False),
    "et--fuka--at-9a9a": snapshot("et/fuka", FUKA_SNAP, FUKA_SNAP, untrusted=False, library=True),
}


def write(path, doc):
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(doc, f, indent=2)
        f.write("\n")


def main():
    if os.path.isdir(POOL_DIR):
        shutil.rmtree(POOL_DIR)
    for name, (slug, versions) in POOL.items():
        write(os.path.join(POOL_DIR, name + ".json"), {"slug": slug, "versions": [served(slug, v) for v in versions]})
    for name, doc in SNAPSHOTS.items():
        write(os.path.join(POOL_DIR, "snapshots", name + ".json"), doc)
    with open(os.path.join(POOL_DIR, "README"), "w") as f:
        f.write("Generated by ../generate.py; do not edit by hand. Change the definitions there and rerun it:\n"
                "    python3 fixtures/cactarium/generate.py\n")
    print(f"{len(POOL)} packages, {len(SNAPSHOTS)} snapshots")


if __name__ == "__main__":
    main()
