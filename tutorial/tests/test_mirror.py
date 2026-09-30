"""mirrors/mirror.py: git mirrors of every tutorial repository, a lock file,
and insteadOf rules. Local bare repositories stand in for GitHub and
Bitbucket: a global gitconfig rewrites `https://github.com/` and
`https://bitbucket.org/` to them, so the tool fetches upstream-style URLs
exactly as it would in the image build."""

import importlib.util
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

import pytest

MIRROR_PY = Path(__file__).resolve().parents[1] / "mirrors" / "mirror.py"
_spec = importlib.util.spec_from_file_location("tutorial_mirror", MIRROR_PY)
mirror = importlib.util.module_from_spec(_spec)
sys.modules["tutorial_mirror"] = mirror
_spec.loader.exec_module(mirror)

RELEASE_TH = """\
# a miniature einsteintoolkit.th
!CRL_VERSION = 1.0
!DEFINE ROOT = Cactus
!DEFINE ARR  = $ROOT/arrangements
!DEFINE ET_RELEASE = ET_2026_05

!TARGET   = $ROOT
!TYPE     = git
!URL      = https://bitbucket.org/einsteintoolkit/manifest.git
!REPO_BRANCH = $ET_RELEASE
!REPO_PATH= $1
!NAME     = manifest
!CHECKOUT = ./manifest

!TARGET   = $ROOT
!TYPE     = git
!URL      = https://bitbucket.org/cactuscode/cactus.git
!REPO_BRANCH = $ET_RELEASE
!NAME     = flesh
!CHECKOUT = Makefile lib src

!TARGET   = $ARR
!TYPE     = git
!URL      = https://github.com/EinsteinToolkit/CarpetX
!REPO_BRANCH = $ET_RELEASE
!REPO_PATH= $2
!CHECKOUT = CarpetX/Algo
CarpetX/CarpetX
#DISABLED CarpetX/Algo

!TARGET   = $ARR
!TYPE     = git
!URL      = https://github.com/EinsteinToolkit/$1-$2
!REPO_BRANCH = $ET_RELEASE
!REPO_PATH = ../$1-$2
!CHECKOUT = ExternalLibraries/zlib
ExternalLibraries/AMReX

# Private thorns
!TARGET   = $ARR
!TYPE     = ignore
!CHECKOUT =
"""

MASTER_TH = RELEASE_TH.replace("ET_2026_05", "master") + """
!TARGET   = $ARR
!TYPE     = git
!URL      = https://github.com/Org/Extra.git
!REPO_PATH= $2
!CHECKOUT =
Extra/Thorn
"""

# A tutorial-style list spelling CarpetX differently (case and `.git`).
LOCAL_TH = """\
!CRL_VERSION = 1.0
!DEFINE ROOT = Cactus
!DEFINE ARR  = $ROOT/arrangements

!TARGET   = $ROOT
!TYPE     = git
!URL      = https://bitbucket.org/cactuscode/cactus.git
!REPO_BRANCH = ET_2026_05
!NAME     = flesh
!CHECKOUT = Makefile lib src

!TARGET   = $ARR
!TYPE     = git
!URL      = https://github.com/einsteintoolkit/CarpetX.git
!REPO_BRANCH = ET_2026_05
!REPO_PATH= $2
!CHECKOUT =
CarpetX/CarpetX

!TARGET   = $ROOT
!TYPE     = svn
!URL      = https://svn.example.org/repos/utils
!CHECKOUT = utils
"""


def run(*args, cwd=None, env=None):
    p = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True)
    assert p.returncode == 0, f"{args}: {p.stderr}"
    return p.stdout.strip()


class Upstream:
    """Bare repositories under <tmp>/upstream/<host>/<path>.git, each fed
    from a work tree under <tmp>/work."""

    def __init__(self, tmp):
        self.root = tmp / "upstream"
        self.work = tmp / "work"

    def bare(self, name):
        return self.root / (name + ".git")

    def commit(self, name, branch, files, tag=None):
        w = self.work / name
        if not w.exists():
            w.mkdir(parents=True)
            run("git", "init", "-q", str(w))
        branches = run("git", "-C", str(w), "branch", "--list", branch)
        has_commits = subprocess.run(["git", "-C", str(w), "rev-parse", "-q", "--verify", "HEAD"],
                                     capture_output=True).returncode == 0
        if branches:
            run("git", "-C", str(w), "checkout", "-q", branch)
        elif has_commits:
            run("git", "-C", str(w), "checkout", "-q", "-b", branch)
        else:
            run("git", "-C", str(w), "checkout", "-q", "--orphan", branch)
        for rel, text in files.items():
            (w / rel).parent.mkdir(parents=True, exist_ok=True)
            (w / rel).write_text(text)
        run("git", "-C", str(w), "add", "-A")
        run("git", "-C", str(w), "commit", "-q", "--allow-empty", "-m", f"{branch} {sorted(files)}")
        if tag:
            run("git", "-C", str(w), "tag", "-a", "-f", "-m", "release", tag)
        self.push(name)
        return run("git", "-C", str(w), "rev-parse", "HEAD")

    def push(self, name):
        bare = self.bare(name)
        if not bare.exists():
            run("git", "init", "-q", "--bare", str(bare))
        run("git", "-C", str(self.work / name), "push", "-q", "--mirror", str(bare))
        # Like a hosting site: HEAD names the first branch there was.
        head = run("git", "--git-dir", str(bare), "symbolic-ref", "HEAD")
        if head not in self.refs(name):
            first = run("git", "-C", str(self.work / name), "symbolic-ref", "HEAD")
            run("git", "--git-dir", str(bare), "symbolic-ref", "HEAD", first)

    def delete_branch(self, name, branch):
        run("git", "-C", str(self.work / name), "branch", "-q", "-D", branch)
        self.push(name)

    def refs(self, name):
        out = run("git", "--git-dir", str(self.bare(name)), "for-each-ref", "--format=%(refname) %(objectname)")
        return dict(line.split(" ", 1) for line in out.splitlines())


@pytest.fixture
def world(tmp_path, monkeypatch):
    up = Upstream(tmp_path)
    home = tmp_path / "home"
    home.mkdir()
    cfg = tmp_path / "upstream.gitconfig"
    cfg.write_text(
        f'[url "file://{up.root}/github.com/"]\n\tinsteadOf = https://github.com/\n'
        f'[url "file://{up.root}/bitbucket.org/"]\n\tinsteadOf = https://bitbucket.org/\n'
        "[user]\n\tname = test\n\temail = test@example.invalid\n"
        "[init]\n\tdefaultBranch = main\n"
        "[advice]\n\tdetachedHead = false\n"
    )
    for var in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        monkeypatch.delenv(var, raising=False)
    monkeypatch.setenv("HOME", str(home))
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(cfg))
    monkeypatch.setenv("GIT_CONFIG_NOSYSTEM", "1")

    up.commit("bitbucket.org/einsteintoolkit/manifest", "ET_2026_05", {"einsteintoolkit.th": RELEASE_TH},
              tag="ET_2026_05_v0")
    up.commit("bitbucket.org/einsteintoolkit/manifest", "master", {"einsteintoolkit.th": MASTER_TH})
    for name in ("bitbucket.org/cactuscode/cactus", "github.com/EinsteinToolkit/CarpetX",
                 "github.com/EinsteinToolkit/ExternalLibraries-AMReX",
                 "github.com/EinsteinToolkit/ExternalLibraries-zlib"):
        up.commit(name, "master", {"README": name})
        up.commit(name, "ET_2026_05", {"README": name + " release"})
    up.commit("github.com/Org/Extra", "main", {"Thorn/interface.ccl": "IMPLEMENTS: Thorn\n"})

    mdb = tmp_path / "mdb"
    (mdb / "generic").mkdir(parents=True)
    (mdb / "GENERATION").write_text("1\n")
    (mdb / "generic" / "meta.toml").write_text('name = "generic"\n')
    local = tmp_path / "lists" / "local.th"
    local.parent.mkdir()
    local.write_text(LOCAL_TH)

    class World:
        pass

    w = World()
    w.tmp, w.up, w.mdb, w.local, w.root, w.cfg = tmp_path, up, mdb, local, tmp_path / "mirrors", cfg
    w.lock = tmp_path / "mirrors.lock"
    # Always an explicit lock: the default is the tutorial's committed one.
    w.args = ["--root", str(w.root), "--lock", str(w.lock), "--no-default-thornlists",
              "--thornlist", str(local), "--mdb-dir", str(mdb), "-j", "4"]
    return w


def lock_of(w):
    return json.loads(w.lock.read_text())["repositories"]


def rules_of(path):
    return mirror.parse_gitconfig_rules(path)


def test_parser_expands_checkouts_like_cactup():
    comps = mirror.parse_thornlist(RELEASE_TH)
    git = [c for c in comps if c.ty == "git"]
    by_checkout = {c.checkout: c for c in git}
    # $1/$2 expand per checkout token, the !CHECKOUT line's included.
    assert by_checkout["ExternalLibraries/zlib"].url == "https://github.com/EinsteinToolkit/ExternalLibraries-zlib"
    assert by_checkout["ExternalLibraries/AMReX"].repo == "ExternalLibraries-AMReX"
    # !NAME names the directory; the defines resolve in the branch.
    assert by_checkout["Makefile"].repo == "flesh"
    assert by_checkout["CarpetX/CarpetX"].branch == "ET_2026_05"
    # #DISABLED lines are comments; the ignore section is kept to report.
    assert [c.checkout for c in git if c.url.endswith("CarpetX")] == ["CarpetX/Algo", "CarpetX/CarpetX"]
    assert [c.ty for c in comps if c.ty != "git"] == []  # an empty ignore section has no tokens
    with pytest.raises(mirror.MirrorError, match="Duplicate checkouts"):
        mirror.parse_thornlist(RELEASE_TH + "\n!TARGET = $ARR\n!TYPE = git\n!URL = https://x.org/y\n"
                               "!REPO_PATH = $2\n!CHECKOUT = CarpetX/CarpetX\n")


def test_sync_mirrors_every_repository_once(world, capsys):
    w = world
    # A pull-request ref upstream must not be mirrored.
    carpetx = w.up.bare("github.com/EinsteinToolkit/CarpetX")
    run("git", "--git-dir", str(carpetx), "update-ref", "refs/pull/1/head", w.up.refs("github.com/EinsteinToolkit/CarpetX")["refs/heads/master"])

    assert mirror.main(["sync", *w.args]) == 0
    out = capsys.readouterr().out

    expected = {
        "bitbucket.org/einsteintoolkit/manifest.git",
        "bitbucket.org/cactuscode/cactus.git",
        # First spelling seen (the release's) names the one mirror both
        # spellings share.
        "github.com/EinsteinToolkit/CarpetX.git",
        "github.com/EinsteinToolkit/ExternalLibraries-AMReX.git",
        "github.com/EinsteinToolkit/ExternalLibraries-zlib.git",
        "github.com/Org/Extra.git",
        "github.com/max-morris/Cactup.git",
    }
    lock = lock_of(w)
    assert set(lock) == expected
    for key in expected:
        assert (w.root / key / "HEAD").is_file(), key
    assert not (w.root / "github.com" / "einsteintoolkit").exists()

    # The lock records every mirrored ref at the upstream commit.
    for key in expected - {"github.com/max-morris/Cactup.git"}:
        upstream = {r: s for r, s in w.up.refs(key[:-4]).items() if not r.startswith("refs/pull/")}
        assert lock[key]["refs"] == upstream, key
    assert "refs/pull/1/head" not in lock["github.com/EinsteinToolkit/CarpetX.git"]["refs"]
    assert lock["github.com/EinsteinToolkit/CarpetX.git"]["head"] == "refs/heads/master"
    assert "refs/tags/ET_2026_05_v0" in lock["bitbucket.org/einsteintoolkit/manifest.git"]["refs"]

    # Repacked: one pack, no loose objects.
    d = w.root / "github.com/EinsteinToolkit/CarpetX.git"
    assert len(list((d / "objects" / "pack").glob("*.pack"))) == 1
    assert run("git", "--git-dir", str(d), "count-objects").startswith("0 objects")

    # The mdb branch is this directory's files, in the layout the sync reads.
    mdb = w.root / "github.com/max-morris/Cactup.git"
    assert set(lock["github.com/max-morris/Cactup.git"]["refs"]) == {"refs/heads/mdb"}
    assert run("git", "--git-dir", str(mdb), "show", "mdb:GENERATION") == "1"
    assert run("git", "--git-dir", str(mdb), "ls-tree", "-r", "--name-only", "mdb").split() == [
        "GENERATION", "generic/meta.toml"]

    assert "ignore entries" not in out  # the release's ignore section is empty
    assert "NOT MIRRORED (svn" in out and "https://svn.example.org/repos/utils" in out
    assert mirror.main(["check", *w.args]) == 0


def test_rules_cover_every_spelling_with_and_without_dot_git(world):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    text = (w.root / "mirrors.gitconfig").read_text()
    carpetx_base = f"file://{w.root}/github.com/EinsteinToolkit/CarpetX.git"
    assert text.count(f'[url "{carpetx_base}"]') == 1
    values = sorted(v for v, b in rules_of(w.root / "mirrors.gitconfig") if b == carpetx_base)
    assert values == [
        "https://github.com/EinsteinToolkit/CarpetX",
        "https://github.com/EinsteinToolkit/CarpetX.git",
        "https://github.com/einsteintoolkit/CarpetX",
        "https://github.com/einsteintoolkit/CarpetX.git",
    ]
    # One section per repository.
    assert text.count("[url ") == len(lock_of(w))
    # Every spelling rewrites to the mirror itself, never to `.git.git`.
    for url in values:
        got = run("git", "ls-remote", "--get-url", url,
                  env={**os.environ, "GIT_CONFIG_GLOBAL": str(w.root / "mirrors.gitconfig")})
        assert got == carpetx_base


def test_rerun_updates_and_prunes(world):
    w = world
    w.up.commit("github.com/EinsteinToolkit/CarpetX", "old", {"O": "o"})
    assert mirror.main(["sync", *w.args]) == 0
    before = lock_of(w)["github.com/EinsteinToolkit/CarpetX.git"]["refs"]
    assert "refs/heads/old" in before
    new = w.up.commit("github.com/EinsteinToolkit/CarpetX", "ET_2026_05", {"NEWS": "moved"})
    w.up.commit("github.com/EinsteinToolkit/CarpetX", "feature", {"F": "f"})
    w.up.delete_branch("github.com/EinsteinToolkit/CarpetX", "old")
    run("git", "--git-dir", str(w.up.bare("github.com/EinsteinToolkit/CarpetX")), "symbolic-ref", "HEAD",
        "refs/heads/ET_2026_05")

    assert mirror.main(["sync", *w.args]) == 0
    entry = lock_of(w)["github.com/EinsteinToolkit/CarpetX.git"]
    assert entry["refs"]["refs/heads/ET_2026_05"] == new != before["refs/heads/ET_2026_05"]
    assert "refs/heads/feature" in entry["refs"]
    assert "refs/heads/old" not in entry["refs"]
    assert entry["head"] == "refs/heads/ET_2026_05"
    mirror_refs = mirror.refs_of(w.root / "github.com/EinsteinToolkit/CarpetX.git")
    assert mirror_refs == entry["refs"]


def test_pin_recreates_the_locked_commits(world, tmp_path):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    saved = tmp_path / "pinned.lock"
    shutil.copy(w.lock, saved)
    locked = json.loads(saved.read_text())["repositories"]

    # Upstream moves on, and a later sync follows it.
    w.up.commit("github.com/EinsteinToolkit/CarpetX", "ET_2026_05", {"NEWS": "later"})
    w.up.commit("bitbucket.org/cactuscode/cactus", "extra", {"X": "x"})
    assert mirror.main(["sync", *w.args]) == 0
    assert lock_of(w) != locked

    # Pinning an existing mirror rewinds it, with no refs beyond the lock.
    assert mirror.main(["pin", *w.args, "--lock", str(saved)]) == 0
    for key, entry in locked.items():
        assert mirror.refs_of(w.root / key) == entry["refs"], key
        assert mirror.head_of(w.root / key) == entry["head"], key

    # And from nothing, it re-creates the same mirrors.
    shutil.rmtree(w.root)
    assert mirror.main(["pin", *w.args, "--lock", str(saved)]) == 0
    for key, entry in locked.items():
        assert mirror.refs_of(w.root / key) == entry["refs"], key
    assert mirror.main(["check", *w.args]) == 0


def test_pin_serves_a_changed_mdb_directory_with_a_note(world, capsys):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    locked = lock_of(w)["github.com/max-morris/Cactup.git"]["refs"]["refs/heads/mdb"]
    capsys.readouterr()

    # The checkout's MDB directory is the source of truth, so a pin serves it
    # as it is now, says so in one line, and does not fail.
    (w.mdb / "generic" / "meta.toml").write_text('name = "changed"\n')
    assert mirror.main(["pin", *w.args]) == 0
    out = capsys.readouterr().out
    notes = [line for line in out.splitlines() if line.startswith("note: ")]
    assert len(notes) == 1 and "differs from the locked MDB" in notes[0] and locked[:12] in notes[0]
    mdb = w.root / "github.com/max-morris/Cactup.git"
    assert run("git", "--git-dir", str(mdb), "show", "mdb:generic/meta.toml") == 'name = "changed"'
    assert lock_of(w)["github.com/max-morris/Cactup.git"]["refs"]["refs/heads/mdb"] == locked  # untouched

    # Put back, the same files give the locked commit again.
    (w.mdb / "generic" / "meta.toml").write_text('name = "generic"\n')
    assert mirror.main(["pin", *w.args]) == 0
    assert "note: " not in capsys.readouterr().out
    assert mirror.refs_of(mdb) == {"refs/heads/mdb": locked}


def test_pin_at_the_lock_is_local_and_changes_nothing(world, capsys, tmp_path):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    capsys.readouterr()
    packs = sorted(p.name for p in w.root.rglob("*.pack"))
    # No upstream at all: a root already at the lock must not need it.
    shutil.move(w.up.root, tmp_path / "gone")
    assert mirror.main(["pin", *w.args]) == 0
    out = capsys.readouterr().out
    # Six fetched mirrors; the MDB, built from its directory, is reported apart.
    assert "6 of 6 mirrors already at the lock" in out
    assert not [line for line in out.splitlines() if line.endswith(("pinned", "fetched and pinned"))]
    # Nothing repacked: the very same packs.
    assert sorted(p.name for p in w.root.rglob("*.pack")) == packs
    assert (w.root / "mirrors.gitconfig").is_file()


def forget_upstream_commit(w, name, branch, move_to=None):
    """Make a locked commit vanish upstream: delete (or force-move) the branch
    that alone reaches it, and prune it from the bare repository."""
    work = w.up.work / name
    run("git", "-C", str(work), "checkout", "-q", "--detach")
    if move_to is None:
        w.up.delete_branch(name, branch)
    else:
        run("git", "-C", str(work), "branch", "-q", "-f", branch, move_to)
        w.up.push(name)
    bare = str(w.up.bare(name))
    run("git", "--git-dir", bare, "reflog", "expire", "--expire=now", "--all")
    run("git", "--git-dir", bare, "gc", "-q", "--prune=now")


def test_pin_leaves_out_a_vanished_branch_nothing_needs(world, capsys):
    w = world
    side = w.up.commit("github.com/EinsteinToolkit/CarpetX", "side", {"S": "only on side"})
    assert mirror.main(["sync", *w.args]) == 0
    assert lock_of(w)["github.com/EinsteinToolkit/CarpetX.git"]["refs"]["refs/heads/side"] == side
    forget_upstream_commit(w, "github.com/EinsteinToolkit/CarpetX", "side")
    shutil.rmtree(w.root)  # a fresh machine
    capsys.readouterr()

    assert mirror.main(["pin", *w.args]) == 0
    out = capsys.readouterr().out
    warnings = [line for line in out.splitlines() if line.startswith("warning: ")]
    assert len(warnings) == 1
    assert "refs/heads/side" in warnings[0] and side[:12] in warnings[0]
    refs = mirror.refs_of(w.root / "github.com/EinsteinToolkit/CarpetX.git")
    locked = lock_of(w)["github.com/EinsteinToolkit/CarpetX.git"]["refs"]
    assert refs == {r: sha for r, sha in locked.items() if r != "refs/heads/side"}
    assert json.loads((w.root / "mirrors.dropped").read_text()) == {
        "github.com/EinsteinToolkit/CarpetX.git": {"refs/heads/side": side}}

    # Later builds count the mirror as at the lock: no network, no warning.
    gone = w.tmp / "gone"
    shutil.move(w.up.root, gone)
    assert mirror.main(["pin", *w.args]) == 0
    out = capsys.readouterr().out
    assert "warning: " not in out
    assert "6 of 6 mirrors already at the lock" in out
    assert "1 locked ref(s) left out" in out

    # A deliberate refresh re-locks without the lost branch and forgets it.
    shutil.move(gone, w.up.root)
    assert mirror.main(["sync", *w.args]) == 0
    assert "refs/heads/side" not in lock_of(w)["github.com/EinsteinToolkit/CarpetX.git"]["refs"]
    assert not (w.root / "mirrors.dropped").exists()


def test_pin_fails_clearly_when_a_needed_commit_vanished(world, capsys):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    # Force-push the release branch the thornlists name onto another commit.
    forget_upstream_commit(w, "github.com/EinsteinToolkit/CarpetX", "ET_2026_05", move_to="master")
    shutil.rmtree(w.root)
    capsys.readouterr()

    assert mirror.main(["pin", *w.args]) == 2
    err = capsys.readouterr().err
    assert "no longer has the locked commit of refs/heads/ET_2026_05" in err
    assert "--update-mirrors" in err and "every bake is rebuilt" in err


def test_pin_takes_releases_and_master_from_the_lock(world, capsys):
    w = world
    # Locked without master: its thornlist (and so Org/Extra) is not locked.
    assert mirror.main(["sync", *w.args, "--no-master"]) == 0
    data = json.loads(w.lock.read_text())
    assert data["releases"] == ["ET_2026_05_v0"] and data["master"] is False
    assert "github.com/Org/Extra.git" not in data["repositories"]
    # With master taken from the lock, pin never asks for Org/Extra.
    assert mirror.main(["pin", *w.args]) == 0
    capsys.readouterr()

    # Flags that disagree with the lock are refused, not silently applied.
    assert mirror.main(["pin", *w.args, "--release", "ET_2025_05_v0"]) == 2
    assert "was written for release(s) ET_2026_05_v0, not ET_2025_05_v0" in capsys.readouterr().err
    assert mirror.main(["sync", *w.args]) == 0  # now with master
    capsys.readouterr()
    assert mirror.main(["pin", *w.args, "--no-master"]) == 2
    assert "was written with the master thornlist" in capsys.readouterr().err
    # Agreeing flags are fine.
    assert mirror.main(["pin", *w.args, "--release", "ET_2026_05_v0"]) == 0


def test_pin_reads_a_lock_that_predates_the_master_field(world):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    data = json.loads(w.lock.read_text())
    del data["master"]
    w.lock.write_text(json.dumps(data))
    # Such a lock was written with master included, so Org/Extra is expected.
    assert mirror.main(["pin", *w.args]) == 0
    assert mirror.main(["pin", *w.args, "--no-master"]) == 2


def test_mdb_commit_depends_only_on_contents(world, tmp_path):
    w = world
    repo = mirror.Repo("k", mirror.MDB_URL, "github.com", "max-morris/Cactup", local_dir=w.mdb)
    first, what = mirror.build_mdb(repo, tmp_path / "a")
    assert what == "built"
    again, what = mirror.build_mdb(repo, tmp_path / "b")
    assert (again, what) == (first, "built")
    assert mirror.build_mdb(repo, tmp_path / "a") == (first, "up to date")
    (w.mdb / "GENERATION").write_text("2\n")
    changed, what = mirror.build_mdb(repo, tmp_path / "a")
    assert changed != first and what == "rebuilt"


def test_mdb_inside_a_work_tree_takes_what_a_commit_would(world, tmp_path):
    w = world
    run("git", "init", "-q", str(w.mdb))
    (w.mdb / ".gitignore").write_text("*.bak\n")
    run("git", "-C", str(w.mdb), "add", "-A")
    run("git", "-C", str(w.mdb), "commit", "-q", "-m", "mdb")
    (w.mdb / "generic" / "old.bak").write_text("ignored\n")
    (w.mdb / "generic" / "new.sh").write_text("untracked, not ignored\n")
    assert mirror.mdb_files(w.mdb) == [".gitignore", "GENERATION", "generic/meta.toml", "generic/new.sh"]


def test_a_rule_capturing_another_url_is_refused(world, capsys):
    w = world
    w.up.commit("github.com/Org/Foo", "main", {"a": "a"})
    w.local.write_text(LOCAL_TH + """
!TARGET   = $ARR
!TYPE     = git
!URL      = https://github.com/Org/Foo
!REPO_PATH= $2
!CHECKOUT =
Foo/Thorn

!TARGET   = $ROOT
!TYPE     = https
!URL      = https://github.com/Org/Foo-data/archive/v1.tar.gz
!CHECKOUT = data
""")
    assert mirror.main(["sync", *w.args]) == 2
    err = capsys.readouterr().err
    assert "insteadOf = https://github.com/Org/Foo (mirror of github.com/org/foo)" in err
    assert "would capture https://github.com/Org/Foo-data/archive/v1.tar.gz" in err
    # Refused before anything but the manifest was fetched.
    assert not (w.root / "github.com" / "Org").exists()


def test_a_longer_rule_of_the_urls_own_repository_makes_a_prefix_safe():
    plan = mirror.build_plan(
        [mirror.Source("x", mirror.parse_thornlist(
            "!CRL_VERSION = 1.0\n"
            "!TARGET = A\n!TYPE = git\n!URL = https://github.com/GRHayL/GRHayL\n!REPO_PATH = $2\n!CHECKOUT = G/One\n"
            "!TARGET = A\n!TYPE = git\n!URL = https://github.com/GRHayL/GRHayLET.git\n!REPO_PATH = $2\n!CHECKOUT = G/Two\n"
        ))],
        mirror.MANIFEST_URL, None, None)
    rules = mirror.all_rules(plan, "/m")
    mirror.check_prefix_safety(plan, rules)  # GRHayL is a prefix of GRHayLET, which has its own rule
    # Without GRHayLET's own rules, GRHayL's would capture it.
    target = plan.repos["github.com/grhayl/grhaylet"]
    with pytest.raises(mirror.MirrorError, match="would capture https://github.com/GRHayL/GRHayLET.git"):
        mirror.check_prefix_safety(plan, [(v, r) for v, r in rules if r is not target])


def test_check_reports_what_does_not_resolve(world, capsys):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    capsys.readouterr()

    shutil.rmtree(w.root / "github.com/EinsteinToolkit/ExternalLibraries-zlib.git")
    extra = w.tmp / "lists" / "later.th"
    extra.write_text(
        "!CRL_VERSION = 1.0\n"
        "!TARGET = A\n!TYPE = git\n!URL = https://github.com/Org/Unmirrored.git\n!REPO_PATH = $2\n!CHECKOUT = U/T\n"
        "!TARGET = A\n!TYPE = git\n!URL = https://github.com/EinsteinToolkit/CarpetX\n!REPO_BRANCH = nope\n"
        "!REPO_PATH = $2\n!CHECKOUT = CarpetX/Other\n"
    )
    assert mirror.main(["check", *w.args, "--thornlist", str(extra)]) == 1
    out = capsys.readouterr().out
    assert "ExternalLibraries-zlib (release ET_2026_05_v0" in out and "no mirror at" in out
    assert "https://github.com/Org/Unmirrored.git (later.th): no insteadOf rule" in out
    assert "has no refs/heads/nope" in out


def test_clone_through_the_generated_gitconfig_is_served_from_the_mirror(world, tmp_path):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    lock = lock_of(w)
    # Take upstream away entirely: only the mirrors can serve the clone.
    shutil.move(w.up.root, tmp_path / "gone")
    frag = w.root / "mirrors.gitconfig"
    env = {**os.environ, "GIT_CONFIG_GLOBAL": str(frag), "GIT_CONFIG_NOSYSTEM": "1"}

    dest = tmp_path / "clone"
    run("git", "clone", "-q", "--depth", "1", "--branch", "ET_2026_05",
        "https://github.com/einsteintoolkit/CarpetX.git", str(dest), env=env)
    assert run("git", "-C", str(dest), "rev-parse", "HEAD") == \
        lock["github.com/EinsteinToolkit/CarpetX.git"]["refs"]["refs/heads/ET_2026_05"]
    # origin still names the upstream URL; only the transport was rewritten.
    assert run("git", "-C", str(dest), "config", "remote.origin.url") == \
        "https://github.com/einsteintoolkit/CarpetX.git"

    # The same through `-c include.path=` instead of a global config.
    dest2 = tmp_path / "clone2"
    run("git", "-c", f"include.path={frag}", "clone", "-q", "--depth", "1",
        "https://github.com/EinsteinToolkit/ExternalLibraries-AMReX", str(dest2),
        env={**os.environ, "GIT_CONFIG_GLOBAL": "/dev/null", "GIT_CONFIG_NOSYSTEM": "1"})
    assert (dest2 / "README").read_text() == "github.com/EinsteinToolkit/ExternalLibraries-AMReX"

    # The MDB sync's fetch: the mdb branch into refs/mdb/head.
    bare = tmp_path / "mdb-repo"
    run("git", "init", "-q", "--bare", str(bare), env=env)
    run("git", "--git-dir", str(bare), "fetch", "-q", mirror.MDB_URL, "+refs/heads/mdb:refs/mdb/head", env=env)
    assert run("git", "--git-dir", str(bare), "show", "refs/mdb/head:GENERATION", env=env) == "1"


def test_sync_refuses_to_fetch_the_mirrors_from_themselves(world, capsys, monkeypatch):
    w = world
    assert mirror.main(["sync", *w.args]) == 0
    capsys.readouterr()
    monkeypatch.setenv("GIT_CONFIG_GLOBAL", str(w.root / "mirrors.gitconfig"))
    assert mirror.main(["sync", *w.args]) == 2
    assert "inside the mirrors" in capsys.readouterr().err


def test_ignore_entries_are_reported_not_mirrored(world, capsys):
    w = world
    w.local.write_text(LOCAL_TH + "\n!TARGET = $ROOT/arrangements\n!TYPE = ignore\n!CHECKOUT = Private/One Private/Two\n")
    assert mirror.main(["sync", *w.args]) == 0
    out = capsys.readouterr().out
    assert "ignore entries (nothing to fetch): 2 in local.th" in out


def test_list_resolves_without_cloning(world, capsys):
    w = world
    w.local.write_text(LOCAL_TH.replace("!REPO_BRANCH = ET_2026_05\n!REPO_PATH", "!REPO_BRANCH = gone\n!REPO_PATH"))
    assert mirror.main(["list", *w.args]) == 0
    out = capsys.readouterr().out
    assert not w.root.exists()
    assert "source release ET_2026_05_v0: 8 git checkouts from 5 repositories" in out
    assert "github.com/EinsteinToolkit/CarpetX.git\n" in out
    assert "    url     https://github.com/einsteintoolkit/CarpetX.git  [local.th]" in out
    assert "7 repositories" in out
    assert "ODDITY: https://github.com/EinsteinToolkit/CarpetX: no branch gone upstream (wanted by local.th)" in out
    assert "ODDITY: github.com/einsteintoolkit/carpetx: case variants" in out
