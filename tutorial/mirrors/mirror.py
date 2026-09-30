#!/usr/bin/env python3
"""Local git mirrors for the Cactup notebook tutorial.

Reads every thornlist the tutorial uses and turns the repositories they name
into bare mirrors under one root, so that installs, refetches and the MDB
sync inside the tutorial image never reach GitHub or Bitbucket:

- the ET release thornlist(s) and the development thornlist (`master`),
  read from the Einstein Toolkit manifest repository exactly where cactup
  reads them: `einsteintoolkit.th` in the tree of the release tag, or of the
  tip of the manifest's `master` branch (src/manifest.rs). The manifest
  repository is mirrored first, and is itself one of the mirrors;
- the tutorial's own thornlists (`tutorial/thornlists/*.th`);
- Cactup's `mdb` branch, which cactup's MDB sync fetches from the `mdb-url`
  knob (`https://github.com/max-morris/Cactup.git` by default) with the
  refspec `+refs/heads/mdb:refs/mdb/head`, reading `GENERATION` at the tree's
  root (src/mdb/sync.rs). It is built from a local directory (this
  checkout's `mdb/`, uncommitted edits included), not fetched, so the image
  works before a machine entry is published.

Subcommands:

  pin     the default build step: bring the mirrors to the commits the lock
          file records (tutorial/mirrors/mirrors.lock, committed with the
          tutorial), fetching from upstream only the objects a mirror lacks,
          so a fresh machine builds the same mirrors and a root already at
          the lock costs a few seconds and no network. The MDB branch is
          always built from --mdb-dir, which is the source of truth for it;
          a directory that differs from the lock gets a one-line note. Then
          it writes the gitconfig fragment and checks the result.
  sync    the deliberate refresh: mirror every repository into
          <root>/<host>/<path>.git (a new mirror is cloned, an existing one
          updated with `git remote update --prune`), repack each (`git repack
          -ad`), rewrite the lock file from what was fetched, write the
          gitconfig fragment of `insteadOf` rules, then check the result.
  check   assert that every URL in every thornlist resolves, by git's
          longest-prefix `insteadOf` matching, to an existing mirror that has
          the branch the thornlist asks for.
  list    resolve and list the repositories, their URL spellings, the
          branches the thornlists ask for and the non-git entries, without
          cloning anything. Reads the manifest from --manifest-repo or from a
          shallow temporary fetch, and asks each repository for its refs with
          `git ls-remote` unless --no-resolve.

The lock records every branch and tag of every mirror, not only the ones the
thornlists name. That is what keeps the mirrors faithful copies: `cactup
releases` lists every release tag of the manifest, `refetch --release` may
move an installation to any of them, and an attendee may list or fetch any
branch of a repository, all of which a lock of the named refs alone would
quietly take away. The price is a lock of about 0.4 MB, one sorted line per
ref, so a refresh diffs readably.

Mirrors carry branches and tags only (`+refs/heads/*` and `+refs/tags/*`):
cactup fetches nothing else, and `git clone --mirror`'s `+refs/*:refs/*`
would also copy GitHub's `refs/pull/*`, which multiply a mirror's size.

The gitconfig fragment has one `[url "file://<rule-root>/<host>/<path>.git"]`
section per repository, with an `insteadOf` line for every spelling of its
URL the thornlists use, each both with and without `.git`. git and gix
match `insteadOf` by plain string prefix and pick the longest match, so a
rule that is a proper prefix of some other URL the thornlists use would
capture it too; the tool refuses to write such a rule unless a longer rule
of that URL's own repository covers it.

Upstream fetches run with the mirror rules switched off in effect: the tool
refuses to fetch a repository whose URL git would rewrite into the mirror
root, since a rerun inside an image that already has the rules would
otherwise fetch the mirrors from themselves and call them current.
"""

from __future__ import annotations

import argparse
import concurrent.futures
import dataclasses
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

MANIFEST_URL = "https://bitbucket.org/einsteintoolkit/manifest.git"
MANIFEST_FILE = "einsteintoolkit.th"
MANIFEST_MASTER = "refs/heads/master"
MDB_URL = "https://github.com/max-morris/Cactup.git"
MDB_BRANCH = "mdb"
DEFAULT_RELEASES = ["ET_2026_05_v0"]
DEFAULT_ROOT = "/opt/cactup-mirrors"
LOCK_NAME = "mirrors.lock"
GITCONFIG_NAME = "mirrors.gitconfig"
DROPPED_NAME = "mirrors.dropped"
LOCK_VERSION = 1

HERE = Path(__file__).resolve().parent
DEFAULT_THORNLISTS = HERE.parent / "thornlists"
DEFAULT_MDB_DIR = HERE.parent.parent / "mdb"
DEFAULT_LOCK = HERE / LOCK_NAME

# Hosts whose repository paths are case-insensitive: spellings that differ
# only in case name one repository there, and share one mirror.
CASE_INSENSITIVE_HOSTS = {"github.com", "bitbucket.org", "gitlab.com"}

# The identity and date of the synthesized mdb commit, fixed so the same
# directory contents always give the same commit.
MDB_COMMIT_ENV = {
    "GIT_AUTHOR_NAME": "cactup-tutorial",
    "GIT_AUTHOR_EMAIL": "cactup-tutorial@localhost",
    "GIT_AUTHOR_DATE": "2026-01-01T00:00:00Z",
    "GIT_COMMITTER_NAME": "cactup-tutorial",
    "GIT_COMMITTER_EMAIL": "cactup-tutorial@localhost",
    "GIT_COMMITTER_DATE": "2026-01-01T00:00:00Z",
}


class MirrorError(Exception):
    """A failure to report and exit on, without a traceback."""


# ---------------------------------------------------------------------------
# Thornlist parsing: a port of cactup's CRL 1.0 parser (src/thornlist.rs),
# which ports GetComponents. Only what decides which repositories a list
# fetches is kept, but every step that can change a URL, a branch or a type
# is reproduced exactly, quirks included.


@dataclasses.dataclass(frozen=True)
class Component:
    ty: str
    target: str
    checkout: str
    name: str | None
    url: str | None
    auth_url: str | None
    branch: str | None
    repo: str


COMPONENT_TYPES = {"cvs", "svn", "git", "darcs", "http", "https", "ftp", "hg", "ignore"}


def _is_url(s: str) -> bool:
    return s.startswith(("http://", "https://", "ftp://"))


def _splice_includes(text: str, base: Path | None) -> str:
    include_re = re.compile(r"^[^#]*!INCLUDE *= *(.*)$")
    out: list[str] = []
    for line in text.split("\n"):
        m = include_re.match(line)
        if not m:
            out.append(line)
            continue
        target = m[1].strip()
        if _is_url(target):
            raise MirrorError(f"!INCLUDE of URL {target!r} is not supported (cactup rejects it too)")
        if base is None:
            raise MirrorError(f"!INCLUDE {target!r} has no base directory to resolve against")
        path = base / target
        try:
            included = path.read_text()
        except OSError as e:
            raise MirrorError(f"failed to read !INCLUDE file {path}: {e}") from e
        included = included.replace("\r\n", "\n").replace("\r", "\n")
        out.extend(_splice_includes(included, path.parent).split("\n"))
    return "\n".join(out)


def _extract_header(text: str) -> tuple[str, bool, str]:
    lines = text.split("\n")
    for i, line in enumerate(lines):
        if line.startswith("#") or not line.strip():
            continue
        if line.startswith("!CRL_VERSION"):
            version = line[len("!CRL_VERSION"):].strip().lstrip("=").strip()
            experimental = line.startswith("!CRL_VERSION ") and "_experimental" in line
            return version, experimental, "\n".join(lines[i + 1:])
        if any(c.isalnum() or c == "_" for c in line):
            raise MirrorError(f"thornlist does not start with !CRL_VERSION (found {line!r})")
    raise MirrorError("thornlist is missing the required !CRL_VERSION header")


def _collect_defines(body: str, experimental: bool) -> dict[str, str]:
    define_re = re.compile(r"^!DEFINE\s*(\S+)\s*=\s*(.+)$")
    var_re = re.compile(r"\$(\w+)")
    defines: dict[str, str] = {}
    for idx, line in enumerate(body.split("\n")):
        m = define_re.match(line)
        if not m:
            continue
        key, raw = m[1], m[2]
        # Only the first $VAR in a value resolves (GetComponents' single,
        # non-global substitution).
        v = var_re.search(raw)
        value = raw[:v.start()] + defines.get(v[1], "") + raw[v.end():] if v else raw
        if key in defines and defines[key] != value:
            if experimental:
                continue
            raise MirrorError(f"Repeated definition of {key} on line {idx + 1}")
        defines[key] = value
    return defines


def _strip_comments(body: str) -> str:
    step_a = re.sub(r"(?m)^\s*#.*$", "", body)
    step_b = step_a.replace("\n\n", "\n")
    return re.sub(r"(?m)#.*$", "", step_b)


def _apply_aliases(text: str) -> str:
    for old, new in (
        ("!ANONYMOUS_USER", "!ANON_USER"),
        ("!ANONYMOUS_PASS", "!ANON_PASS"),
        ("!ANONYMOUS_PASSWORD", "!ANON_PASS"),
        ("!LOCAL_PATH", "!LOC_PATH"),
        ("!REPOSITORY_PATH", "!REPO_PATH"),
        ("!REPOSITORY_BRANCH", "!REPO_BRANCH"),
        ("!AUTHORIZATION_URL", "!AUTH_URL"),
    ):
        text = text.replace(old, new)
    return text


def _substitute_vars(text: str, defines: dict[str, str]) -> str:
    def from_defines(m: re.Match[str]) -> str:
        if m[1] is not None:
            return m[0]
        return defines.get(m[2], m[0])

    phase1 = re.sub(r"\\\$(\w+)|\$(\w+)", from_defines, text)
    env_re = re.compile(r"\\\$([A-Za-z]\w*)|\$([A-Za-z]\w*)")
    for m in env_re.finditer(phase1):
        if m[1] is None and m[2] not in os.environ:
            raise MirrorError(f"No definition for {m[2]} found in input file or environment")

    def from_env(m: re.Match[str]) -> str:
        return m[0] if m[1] is not None else os.environ.get(m[2], "")

    return env_re.sub(from_env, phase1)


def _kv_map(section: str) -> dict[str, str]:
    matches = list(re.finditer(r"(?m)^\s*!([^\s=]+)\s*=\s*", section))
    out: dict[str, str] = {}
    for i, m in enumerate(matches):
        end = matches[i + 1].start() if i + 1 < len(matches) else len(section)
        out[m[1]] = section[m.end():end].rstrip()
    return out


def _split_checkout(token: str) -> tuple[str, str]:
    idx = token.rfind("/")
    return (token, "") if idx < 0 else (token[:idx], token[idx + 1:])


def _subst_12(s: str, d1: str, d2: str) -> str:
    return s.replace("$1", d1).replace("$2", d2)


def _basename_strip(url: str) -> str:
    if url.endswith(".git"):
        s = url[:-4]
    elif url.endswith("_darcs"):
        s = url[:-6]
    elif url.endswith(".hg"):
        s = url[:-3]
        s = s[:-1] if s.endswith("/") else s
    else:
        s = url
    idx = max(s.rfind("/"), s.rfind(":"))
    return s[idx + 1:] if idx >= 0 else s


def _build_section(kv: dict[str, str], out: list[Component]) -> None:
    target = kv.get("TARGET", "")
    ty = kv.get("TYPE")
    if ty == "ignore":
        for token in kv.get("CHECKOUT", "").split():
            d1, d2 = _split_checkout(token)
            name = _subst_12(kv["NAME"], d1, d2) if "NAME" in kv else None
            out.append(Component("ignore", target, token, name, None, None,
                                 kv.get("REPO_BRANCH"), name or token))
        return
    if "CHECKOUT" not in kv:
        return  # cactup warns and skips the section
    if ty is None:
        raise MirrorError(f"section for target {target!r} is missing the required !TYPE directive")
    if ty not in COMPONENT_TYPES:
        raise MirrorError(f"section for target {target!r} has unrecognized !TYPE {ty!r}")
    if "URL" not in kv:
        raise MirrorError(f"section for target {target!r} (type {ty}) is missing the required !URL directive")
    for token in kv["CHECKOUT"].split():
        d1, d2 = _split_checkout(token)
        url = _subst_12(kv["URL"], d1, d2)
        auth = _subst_12(kv["AUTH_URL"], d1, d2) if "AUTH_URL" in kv else None
        name = _subst_12(kv["NAME"], d1, d2) if "NAME" in kv else None
        out.append(Component(ty, target, token, name, url, auth, kv.get("REPO_BRANCH"),
                             name if name is not None else _basename_strip(url)))


def _lexical_canonicalize(path: str) -> str:
    absolute = path.startswith("/")
    stack: list[str] = []
    for seg in path.split("/"):
        if seg in ("", "."):
            continue
        if seg == "..":
            if stack and stack[-1] != "..":
                stack.pop()
            elif not absolute:
                stack.append("..")
            continue
        stack.append(seg)
    joined = "/".join(stack)
    return "/" + joined if absolute else joined


def _detect_duplicates(components: list[Component], root: str) -> None:
    seen: set[str] = set()
    dupes: list[str] = []
    for c in components:
        if c.ty == "ignore":
            continue
        canon = _lexical_canonicalize(f"{c.target}/{c.checkout}")
        canon = canon[len(root) + 1:] if canon.startswith(root + "/") else canon
        if canon in seen:
            dupes.append(canon)
        seen.add(canon)
    if dupes:
        raise MirrorError("Duplicate checkouts: " + " ".join(dupes))


def parse_thornlist(text: str, base: Path | None = None) -> list[Component]:
    """Every component of a CRL 1.0 thornlist, `ignore` ones included (cactup
    drops those after validating them; they are kept here to be reported)."""
    text = text.replace("\r\n", "\n").replace("\r", "\n")
    text = _splice_includes(text, base)
    _, experimental, body = _extract_header(text)
    defines = _collect_defines(body, experimental)
    root = defines.get("ROOT", ".")
    text = _substitute_vars(_apply_aliases(_strip_comments(body)), defines)
    components: list[Component] = []
    for piece in re.split(r"(?m)^!TARGET\s*=\s*", text)[1:]:
        if piece:
            _build_section(_kv_map("!TARGET = " + piece), components)
    _detect_duplicates(components, root)
    return components


def check_repo_dirs(components: list[Component], label: str) -> None:
    """cactup's own consistency rule (fetch::plan): the sections naming one
    repository directory must agree on URL and branch."""
    seen: dict[str, Component] = {}
    for c in components:
        if c.ty != "git":
            continue
        first = seen.setdefault(c.repo, c)
        if (first.url, first.branch) != (c.url, c.branch):
            raise MirrorError(
                f"{label}: repository directory {c.repo} is named twice with conflicting sources: "
                f"{first.url} @ {first.branch} (for {first.checkout}) vs {c.url} @ {c.branch} (for {c.checkout})"
            )


# ---------------------------------------------------------------------------
# URLs and repositories


URL_RE = re.compile(r"^(?P<scheme>[A-Za-z][A-Za-z0-9+.-]*)://(?:[^@/]*@)?(?P<host>[^/:]*)(?::\d+)?/(?P<path>.*)$")
SCP_RE = re.compile(r"^(?:[^@/]+@)?(?P<host>[^/:]+):(?P<path>[^/].*)$")


def split_url(url: str) -> tuple[str, str]:
    """(host, path) of a git URL, the path without a trailing `/` or `.git`."""
    m = URL_RE.match(url) or SCP_RE.match(url)
    if not m or not m["host"]:
        raise MirrorError(f"cannot mirror {url!r}: not a URL with a host")
    path = m["path"].rstrip("/")
    if path.endswith(".git"):
        path = path[:-4]
    if not path or ".." in path.split("/"):
        raise MirrorError(f"cannot mirror {url!r}: no usable repository path")
    return m["host"].lower(), path


def url_key(url: str) -> str:
    host, path = split_url(url)
    if host in CASE_INSENSITIVE_HOSTS:
        path = path.lower()
    return f"{host}/{path}"


def spelling_variants(url: str) -> list[str]:
    """The insteadOf values one spelling needs: the URL without a trailing
    `/` or `.git`, and with `.git`. Both are always written, so a URL spelled
    without `.git` never swallows the `.git` spelling (which would rewrite it
    to `….git.git`)."""
    base = url.rstrip("/")
    if base.endswith(".git"):
        base = base[:-4]
    return [base, base + ".git"]


@dataclasses.dataclass(eq=False)
class Repo:
    key: str
    url: str  # the spelling fetched from: the first one seen
    host: str
    path: str
    spellings: dict[str, set[str]] = dataclasses.field(default_factory=dict)
    branches: dict[str | None, set[str]] = dataclasses.field(default_factory=dict)
    local_dir: Path | None = None  # set for a repository built from a directory

    def mirror_dir(self, root: Path) -> Path:
        return root / self.host / (self.path + ".git")

    def rule_base(self, rule_root: str) -> str:
        return "file://" + rule_root.rstrip("/") + f"/{self.host}/{self.path}.git"


@dataclasses.dataclass
class Source:
    label: str
    components: list[Component]


@dataclasses.dataclass
class Plan:
    sources: list[Source]
    repos: dict[str, Repo]
    non_git: list[tuple[str, Component]]
    extra_urls: dict[str, set[str]]  # URLs to keep safe that are not fetched by git
    cactup_urls: list[tuple[str, str | None, str]]  # (URL, branch, label) cactup fetches on its own


def build_plan(sources: list[Source], manifest_url: str, mdb_url: str | None, mdb_dir: Path | None) -> Plan:
    repos: dict[str, Repo] = {}
    non_git: list[tuple[str, Component]] = []
    extra: dict[str, set[str]] = {}

    def add(url: str, label: str, branch: str | None) -> Repo:
        key = url_key(url)
        repo = repos.get(key)
        if repo is None:
            host, path = split_url(url)
            repo = repos[key] = Repo(key, url, host, path)
        repo.spellings.setdefault(url, set()).add(label)
        repo.branches.setdefault(branch, set()).add(label)
        return repo

    for src in sources:
        for c in src.components:
            if c.ty == "git":
                assert c.url is not None
                add(c.url, src.label, c.branch)
                if c.auth_url and c.auth_url != c.url:
                    extra.setdefault(c.auth_url, set()).add(src.label)
            else:
                non_git.append((src.label, c))
                for u in (c.url, c.auth_url):
                    if u:
                        extra.setdefault(u, set()).add(src.label)
    cactup_urls: list[tuple[str, str | None, str]] = [(manifest_url, None, "cactup manifest-url")]
    add(manifest_url, "cactup manifest-url", None)
    if mdb_url is not None:
        repo = add(mdb_url, "cactup mdb-url", MDB_BRANCH)
        repo.local_dir = mdb_dir
        cactup_urls.append((mdb_url, MDB_BRANCH, "cactup mdb-url"))
    return Plan(sources, repos, non_git, extra, cactup_urls)


def all_rules(plan: Plan, rule_root: str) -> list[tuple[str, Repo]]:
    """(insteadOf value, repository) for every rule, sorted."""
    rules: dict[str, Repo] = {}
    for repo in plan.repos.values():
        for spelling in repo.spellings:
            for value in spelling_variants(spelling):
                other = rules.setdefault(value, repo)
                if other is not repo:
                    raise MirrorError(f"{value} would be a rule for both {other.key} and {repo.key}")
    return sorted(rules.items())


def longest_rule(url: str, rules: list[tuple[str, Repo]]) -> tuple[str, Repo] | None:
    best: tuple[str, Repo] | None = None
    for value, repo in rules:
        if url.startswith(value) and (best is None or len(value) > len(best[0])):
            best = (value, repo)
    return best


def check_prefix_safety(plan: Plan, rules: list[tuple[str, Repo]]) -> None:
    """Refuse a rule that captures a URL of the thornlists meant for another
    repository, or for no mirror at all, unless a longer rule wins it."""
    owner: dict[str, Repo | None] = {u: None for u in plan.extra_urls}
    for repo in plan.repos.values():
        for spelling in repo.spellings:
            owner[spelling] = repo
    problems = []
    for url, meant in sorted(owner.items(), key=lambda kv: kv[0]):
        for value, repo in rules:
            if url == value or not url.startswith(value) or repo is meant:
                continue
            best = longest_rule(url, rules)
            assert best is not None
            if best[1] is meant:
                continue  # a longer rule of the URL's own repository covers it
            what = f"the mirror of {meant.key}" if meant else "no mirror (it is not fetched with git)"
            problems.append(f"  insteadOf = {value} (mirror of {repo.key}) would capture {url}, meant for {what}")
    if problems:
        raise MirrorError("refusing to write insteadOf rules that capture other URLs:\n" + "\n".join(problems))


def render_gitconfig(rules: list[tuple[str, Repo]], rule_root: str) -> str:
    by_repo: dict[str, list[str]] = {}
    bases: dict[str, str] = {}
    for value, repo in rules:
        by_repo.setdefault(repo.key, []).append(value)
        bases[repo.key] = repo.rule_base(rule_root)
    out = [
        "# Generated by tutorial/mirrors/mirror.py; regenerate rather than edit.",
        "# One section per mirrored repository, one insteadOf per URL spelling.",
    ]
    for key in sorted(bases, key=lambda k: bases[k]):
        base = bases[key].replace("\\", "\\\\").replace('"', '\\"')
        out.append(f'[url "{base}"]')
        out.extend(f"\tinsteadOf = {v}" for v in sorted(by_repo[key]))
    return "\n".join(out) + "\n"


# ---------------------------------------------------------------------------
# git


def git(*args: str, cwd: Path | None = None, env: dict[str, str] | None = None,
        check: bool = True, input: str | None = None) -> subprocess.CompletedProcess[str]:
    full_env = dict(os.environ)
    # Never prompt: a missing repository on GitHub asks for credentials.
    full_env["GIT_TERMINAL_PROMPT"] = "0"
    if env:
        full_env.update(env)
    p = subprocess.run(["git", *args], cwd=cwd, env=full_env, input=input,
                       capture_output=True, text=True)
    if check and p.returncode != 0:
        raise MirrorError(f"git {' '.join(args)} failed ({p.returncode}):\n{p.stderr.strip()}")
    return p


def refs_of(gitdir: Path) -> dict[str, str]:
    out = git("--git-dir", str(gitdir), "for-each-ref", "--format=%(refname) %(objectname)").stdout
    return dict(line.split(" ", 1) for line in out.splitlines() if line)


def head_of(gitdir: Path) -> str | None:
    p = git("--git-dir", str(gitdir), "symbolic-ref", "-q", "HEAD", check=False)
    return p.stdout.strip() or None


def guard_not_self(url: str, root: Path, rule_root: str) -> None:
    effective = git("ls-remote", "--get-url", url).stdout.strip()
    for r in {str(root), rule_root.rstrip("/")}:
        if effective.startswith(("file://" + r + "/", r + "/")):
            raise MirrorError(
                f"git rewrites {url} to {effective}, inside the mirrors: the mirror rules are active in "
                "this git configuration, so a fetch would read the mirrors themselves. Run without them "
                "(for example GIT_CONFIG_NOSYSTEM=1)."
            )


def remote_head(gitdir: Path) -> str | None:
    p = git("--git-dir", str(gitdir), "ls-remote", "--symref", "origin", "HEAD", check=False)
    for line in p.stdout.splitlines():
        if line.startswith("ref: ") and line.endswith("\tHEAD"):
            return line[5:-5]
    return None


def fetch_mirror(repo: Repo, root: Path, rule_root: str) -> str:
    """Clone or update one mirror from upstream. Returns what happened."""
    guard_not_self(repo.url, root, rule_root)
    d = repo.mirror_dir(root)
    if not d.exists():
        d.parent.mkdir(parents=True, exist_ok=True)
        for stale in d.parent.glob(d.name + ".tmp-*"):  # an interrupted earlier run's
            shutil.rmtree(stale, ignore_errors=True)
        tmp = d.with_name(d.name + f".tmp-{os.getpid()}")
        git("init", "--bare", "-q", str(tmp))
        g = ("--git-dir", str(tmp))
        git(*g, "config", "remote.origin.url", repo.url)
        git(*g, "config", "remote.origin.fetch", "+refs/heads/*:refs/heads/*")
        git(*g, "config", "--add", "remote.origin.fetch", "+refs/tags/*:refs/tags/*")
        git(*g, "config", "remote.origin.mirror", "true")
        git(*g, "remote", "update", "--prune")
        if head := remote_head(tmp):
            git(*g, "symbolic-ref", "HEAD", head)
        tmp.rename(d)
        return "cloned"
    g = ("--git-dir", str(d))
    if git(*g, "config", "remote.origin.url").stdout.strip() != repo.url:
        git(*g, "config", "remote.origin.url", repo.url)
    before = refs_of(d)
    git(*g, "remote", "update", "--prune")
    if (head := remote_head(d)) and head != head_of(d):
        git(*g, "symbolic-ref", "HEAD", head)
    return "updated" if refs_of(d) != before else "up to date"


def mdb_files(mdb_dir: Path) -> list[str]:
    """The files a commit of `mdb_dir` would hold: git's view (tracked plus
    untracked-but-not-ignored) when it is inside a work tree, else all."""
    p = git("-C", str(mdb_dir), "ls-files", "-z", "--cached", "--others", "--exclude-standard", ".", check=False)
    if p.returncode == 0:
        names = sorted({n for n in p.stdout.split("\0") if n})
        return [n for n in names if (mdb_dir / n).is_file() or (mdb_dir / n).is_symlink()]
    return sorted(str(f.relative_to(mdb_dir)) for f in mdb_dir.rglob("*")
                  if (f.is_file() or f.is_symlink()) and ".git" not in f.relative_to(mdb_dir).parts)


def build_mdb(repo: Repo, root: Path) -> tuple[str, str]:
    """Write `repo.local_dir`'s files as the single commit of the mirror's
    `mdb` branch. The commit depends on the contents only, so a rebuild from
    the same files gives the same commit."""
    src = repo.local_dir
    assert src is not None
    if not (src / "GENERATION").is_file():
        raise MirrorError(f"{src} has no GENERATION file: not an MDB directory")
    d = repo.mirror_dir(root)
    if not d.exists():
        d.parent.mkdir(parents=True, exist_ok=True)
        git("init", "--bare", "-q", str(d))
    g = ("--git-dir", str(d))
    files = mdb_files(src)
    with tempfile.TemporaryDirectory() as tmp:
        env = {"GIT_INDEX_FILE": str(Path(tmp) / "index"), **MDB_COMMIT_ENV}
        git(*g, "--work-tree", str(src), "-c", "core.autocrlf=false", "-c", "core.fileMode=true",
            "update-index", "--add", "-z", "--stdin", env=env, input="\0".join(files) + "\0")
        tree = git(*g, "write-tree", env=env).stdout.strip()
        commit = git(*g, "commit-tree", tree, "-m", "Cactup MDB, built from a local directory",
                     env=env).stdout.strip()
    before = refs_of(d).get(f"refs/heads/{MDB_BRANCH}")
    git(*g, "update-ref", f"refs/heads/{MDB_BRANCH}", commit)
    git(*g, "symbolic-ref", "HEAD", f"refs/heads/{MDB_BRANCH}")
    # Only the one branch: drop anything an earlier run left behind.
    for ref in refs_of(d):
        if ref != f"refs/heads/{MDB_BRANCH}":
            git(*g, "update-ref", "-d", ref)
    return commit, ("built" if before is None else "up to date" if before == commit else "rebuilt")


def repack(d: Path) -> None:
    git("--git-dir", str(d), "repack", "-adq")


# ---------------------------------------------------------------------------
# Sources


def manifest_thornlist(gitdir: Path, rev: str) -> str:
    return git("--git-dir", str(gitdir), "show", f"{rev}:{MANIFEST_FILE}").stdout


def load_sources(args: argparse.Namespace, manifest_dir: Path | None) -> list[Source]:
    sources: list[Source] = []
    if manifest_dir is not None:
        for release in args.release:
            rev = f"refs/tags/{release}"
            if git("--git-dir", str(manifest_dir), "rev-parse", "-q", "--verify", rev, check=False).returncode:
                raise MirrorError(f"the manifest has no release tag {release}")
            sources.append(Source(f"release {release}", parse_thornlist(manifest_thornlist(manifest_dir, rev))))
        if args.master:
            sha = git("--git-dir", str(manifest_dir), "rev-parse", MANIFEST_MASTER).stdout.strip()
            sources.append(Source(f"master ({sha[:7]})",
                                  parse_thornlist(manifest_thornlist(manifest_dir, MANIFEST_MASTER))))
    for path in thornlist_files(args):
        sources.append(Source(path.name, parse_thornlist(path.read_text(), path.parent)))
    for s in sources:
        check_repo_dirs(s.components, s.label)
    return sources


def thornlist_files(args: argparse.Namespace) -> list[Path]:
    files = [Path(p) for p in args.thornlist]
    if not args.no_default_thornlists and DEFAULT_THORNLISTS.is_dir():
        files += sorted(DEFAULT_THORNLISTS.glob("*.th"))
    return files


def manifest_repo(args: argparse.Namespace) -> Repo:
    host, path = split_url(args.manifest_url)
    return Repo(url_key(args.manifest_url), args.manifest_url, host, path)


def mdb_url(args: argparse.Namespace) -> str | None:
    return None if args.no_mdb else args.mdb_url


def mdb_dir(args: argparse.Namespace) -> Path | None:
    return None if args.no_mdb else Path(args.mdb_dir).resolve()


# ---------------------------------------------------------------------------
# Lock file


def lock_entry(repo: Repo, root: Path) -> dict[str, object]:
    d = repo.mirror_dir(root)
    entry: dict[str, object] = {"url": repo.url, "head": head_of(d), "refs": refs_of(d)}
    if repo.local_dir is not None:
        entry["source"] = "local directory"
    return entry


def write_lock(path: Path, plan: Plan, root: Path, releases: list[str], master: bool) -> None:
    data = {
        "version": LOCK_VERSION,
        "releases": releases,
        "master": master,
        "repositories": {
            f"{r.host}/{r.path}.git": lock_entry(r, root)
            for r in sorted(plan.repos.values(), key=lambda r: (r.host, r.path))
        },
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, indent=1, sort_keys=True) + "\n")


def read_lock(path: Path) -> dict[str, object]:
    try:
        data = json.loads(path.read_text())
    except (OSError, ValueError) as e:
        raise MirrorError(f"cannot read lock file {path}: {e}") from e
    if data.get("version") != LOCK_VERSION:
        raise MirrorError(f"{path}: unsupported lock version {data.get('version')!r}")
    return data


def missing_objects(d: Path, shas: set[str]) -> list[str]:
    """Which of `shas` the repository at `d` lacks (all, if there is none)."""
    if not (d / "HEAD").is_file():
        return sorted(shas)
    out = git("--git-dir", str(d), "cat-file", "--batch-check", input="".join(f"{s}\n" for s in sorted(shas))).stdout
    return sorted(line.split()[0] for line in out.splitlines() if line.endswith(" missing"))


UPDATE_HINT = ("`build.sh --update-mirrors` (mirror.py sync) re-locks from upstream, but it moves every "
               "mirror to upstream's current state, so every bake is rebuilt")


def vanished_needed(url: str, refs: list[str]) -> MirrorError:
    return MirrorError(f"{url}: upstream no longer has the locked commit of {', '.join(refs)}, which the "
                       f"tutorial needs. {UPDATE_HINT}.")


def pin_mirror(repo: Repo, entry: dict[str, object], root: Path, rule_root: str,
               needed: set[str], known_dropped: dict[str, str]) -> tuple[str, bool, dict[str, str]]:
    """Bring one mirror to the lock's refs and HEAD. Returns what happened,
    whether anything changed, and the refs left out because upstream no
    longer has their locked commits (ref -> locked commit). Such a ref fails
    the pin if it is in `needed` or is HEAD's target; any other is left out
    with a warning. `known_dropped` are refs an earlier pin left out: a
    mirror that differs from the lock by exactly those is at the lock.
    Touches the network only for objects the mirror lacks; a mirror already
    at the lock costs two local git calls."""
    d = repo.mirror_dir(root)
    refs: dict[str, str] = dict(entry["refs"])  # type: ignore[arg-type]
    head = entry.get("head")
    g = ("--git-dir", str(d))
    skip = {r: sha for r, sha in known_dropped.items() if refs.get(r) == sha and r not in needed and r != head}
    if (d / "HEAD").is_file() and head_of(d) == head and \
            refs_of(d) == {r: sha for r, sha in refs.items() if r not in skip}:
        return "at the lock", False, skip
    action = "pinned"
    dropped: dict[str, str] = {}
    if missing_objects(d, set(refs.values())):
        fetch_mirror(repo, root, rule_root)
        action = "fetched and pinned"
        for sha in missing_objects(d, set(refs.values())):
            git(*g, "fetch", "-q", "origin", sha, check=False)
        if gone := set(missing_objects(d, set(refs.values()))):
            dropped = {r: sha for r, sha in refs.items() if sha in gone}
            must = sorted(r for r in dropped if r in needed or r == head)
            if must:
                raise vanished_needed(repo.url, must)
            for r, sha in sorted(dropped.items()):
                say(f"warning: {repo.url}: upstream no longer has {r} at its locked commit {sha[:12]}; "
                    f"left out of the mirror (nothing the tutorial uses names it) and recorded in "
                    f"{DROPPED_NAME}, so later builds count this mirror as at the lock without the "
                    "network. `build.sh --update-mirrors` refreshes the lock.")
                del refs[r]
    current = refs_of(d)
    lines = [f"delete {r}" for r in current if r not in refs]
    lines += [f"update {r} {sha}" for r, sha in refs.items() if current.get(r) != sha]
    if lines:
        git(*g, "update-ref", "--stdin", input="\n".join(lines) + "\n")
    if head and head_of(d) != head:
        git(*g, "symbolic-ref", "HEAD", str(head))
    return action, True, dropped


def needed_refs(plan: Plan, releases: list[str], master: bool, manifest_key: str) -> dict[str, set[str]]:
    """Per locked repository key, the refs the tutorial actually uses: every
    branch a thornlist, the manifest or the MDB names, and for the manifest
    the release tags and, when the lock includes it, master. (Each mirror's HEAD target is added by
    pin_mirror itself.)"""
    out: dict[str, set[str]] = {}
    for repo in plan.repos.values():
        out[f"{repo.host}/{repo.path}.git"] = {f"refs/heads/{b}" for b in repo.branches if b}
    out.setdefault(manifest_key, set()).update(
        {f"refs/tags/{r}" for r in releases} | ({MANIFEST_MASTER} if master else set()))
    return out


def repo_from_lock(key: str, entry: dict[str, object]) -> Repo:
    host, _, rest = key.partition("/")
    return Repo(url_key(str(entry["url"])), str(entry["url"]), host, rest.removesuffix(".git"))


# ---------------------------------------------------------------------------
# Check


def parse_gitconfig_rules(path: Path) -> list[tuple[str, str]]:
    """(insteadOf value, base) pairs, read by git itself."""
    p = git("config", "-f", str(path), "-z", "--get-regexp", r"^url\..*\.insteadof$", check=False)
    if p.returncode not in (0, 1):
        raise MirrorError(f"cannot read {path}: {p.stderr.strip()}")
    rules = []
    for item in p.stdout.split("\0"):
        if not item:
            continue
        key, _, value = item.partition("\n")
        rules.append((value, key[len("url."):-len(".insteadof")]))
    return rules


def check(plan: Plan, gitconfig: Path, root: Path, rule_root: str) -> list[str]:
    """Problems with the mirrors as `gitconfig` routes the plan's URLs."""
    rules = parse_gitconfig_rules(gitconfig)
    problems: list[str] = []
    prefix = "file://" + rule_root.rstrip("/") + "/"
    env = {"GIT_CONFIG_GLOBAL": str(gitconfig), "GIT_CONFIG_NOSYSTEM": "1"}
    wanted: dict[tuple[str, str | None], set[str]] = {}
    for src in plan.sources:
        for c in src.components:
            if c.ty == "git" and c.url:
                wanted.setdefault((c.url, c.branch), set()).add(src.label)
    for url, branch, label in plan.cactup_urls:
        wanted.setdefault((url, branch), set()).add(label)
    for (url, branch), labels in sorted(wanted.items(), key=lambda kv: (kv[0][0], kv[0][1] or "")):
        where = ", ".join(sorted(labels))
        matches = [(v, b) for v, b in rules if url.startswith(v)]
        if not matches:
            problems.append(f"{url} ({where}): no insteadOf rule")
            continue
        value, base = max(matches, key=lambda vb: len(vb[0]))
        rewritten = base + url[len(value):]
        by_git = git("ls-remote", "--get-url", url, env=env).stdout.strip()
        if by_git != rewritten:
            problems.append(f"{url} ({where}): git rewrites it to {by_git}, expected {rewritten}")
            continue
        if not rewritten.startswith(prefix):
            problems.append(f"{url} ({where}): rewritten to {rewritten}, outside the mirror root")
            continue
        d = root / rewritten[len(prefix):]
        if not (d / "HEAD").is_file():
            problems.append(f"{url} ({where}): no mirror at {d}")
            continue
        ref = f"refs/heads/{branch}" if branch else "HEAD"
        if git("--git-dir", str(d), "rev-parse", "-q", "--verify", ref + "^{commit}", check=False).returncode:
            problems.append(f"{url} ({where}): mirror {d} has no {ref}")
    # The rules as written must be as safe as the ones sync would write.
    by_base = {r.rule_base(rule_root): r for r in plan.repos.values()}
    file_rules = [(v, by_base.get(b) or Repo(b, b, "", "")) for v, b in rules]
    try:
        check_prefix_safety(plan, file_rules)
    except MirrorError as e:
        problems.append(str(e))
    return problems


# ---------------------------------------------------------------------------
# Commands


def parallel(fn, items, jobs: int):
    with concurrent.futures.ThreadPoolExecutor(max_workers=max(1, jobs)) as pool:
        futures = {pool.submit(fn, item): item for item in items}
        results, errors = {}, []
        for fut in concurrent.futures.as_completed(futures):
            item = futures[fut]
            try:
                results[item] = fut.result()
            except MirrorError as e:
                errors.append(str(e))
        return results, errors


def say(msg: str) -> None:
    print(msg, flush=True)


def report_non_git(plan: Plan) -> None:
    ignored = [(label, c) for label, c in plan.non_git if c.ty == "ignore"]
    other = [(label, c) for label, c in plan.non_git if c.ty != "ignore"]
    if ignored:
        by_source: dict[str, int] = {}
        for label, _ in ignored:
            by_source[label] = by_source.get(label, 0) + 1
        say("ignore entries (nothing to fetch): "
            + ", ".join(f"{n} in {label}" for label, n in by_source.items()))
    for label, c in other:
        say(f"NOT MIRRORED ({c.ty}, needs the network in the image): {c.target}/{c.checkout} <- {c.url} [{label}]")


def paths(args: argparse.Namespace) -> tuple[Path, str, Path, Path]:
    root = Path(args.root).resolve()
    rule_root = args.rule_root or str(root)
    lock = Path(args.lock) if args.lock else DEFAULT_LOCK
    gitconfig = Path(args.gitconfig) if args.gitconfig else root / GITCONFIG_NAME
    return root, rule_root, lock, gitconfig


def finish(plan: Plan, root: Path, rule_root: str, gitconfig: Path) -> int:
    rules = all_rules(plan, rule_root)
    check_prefix_safety(plan, rules)
    gitconfig.parent.mkdir(parents=True, exist_ok=True)
    gitconfig.write_text(render_gitconfig(rules, rule_root))
    say(f"wrote {gitconfig}: {len(plan.repos)} repositories, {len(rules)} insteadOf rules")
    problems = check(plan, gitconfig, root, rule_root)
    for p in problems:
        say("CHECK FAILED: " + p)
    return 1 if problems else 0


def cmd_sync(args: argparse.Namespace) -> int:
    root, rule_root, lock, gitconfig = paths(args)
    root.mkdir(parents=True, exist_ok=True)
    manifest = manifest_repo(args)
    say(f"{manifest.url}: {fetch_mirror(manifest, root, rule_root)}")
    plan = build_plan(load_sources(args, manifest.mirror_dir(root)), args.manifest_url, mdb_url(args), mdb_dir(args))
    check_prefix_safety(plan, all_rules(plan, rule_root))  # before any more network
    report_non_git(plan)
    fetched = [r for r in plan.repos.values() if r.local_dir is None and r.key != manifest.key]
    results, errors = parallel(lambda r: fetch_mirror(r, root, rule_root), fetched, args.jobs)
    for repo, what in sorted(results.items(), key=lambda kv: kv[0].key):
        say(f"{repo.url}: {what}")
    for repo in plan.repos.values():
        if repo.local_dir is not None:
            commit, what = build_mdb(repo, root)
            say(f"{repo.url} (branch {MDB_BRANCH} from {repo.local_dir}): {what}, {commit[:12]}")
    if errors:
        raise MirrorError("\n".join(errors))
    _, errors = parallel(lambda r: repack(r.mirror_dir(root)), list(plan.repos.values()), args.jobs)
    if errors:
        raise MirrorError("\n".join(errors))
    write_lock(lock, plan, root, args.release, args.master)
    # A fresh lock: nothing is left out of any mirror any more.
    (root / DROPPED_NAME).unlink(missing_ok=True)
    say(f"wrote {lock}")
    return finish(plan, root, rule_root, gitconfig)


def cmd_pin(args: argparse.Namespace) -> int:
    root, rule_root, lock, gitconfig = paths(args)
    data = read_lock(lock)
    locked: dict[str, dict[str, object]] = data["repositories"]  # type: ignore[assignment]
    # Reproduce the lock: its releases and master decide which refs are
    # needed. A lock written before `master` was recorded included it.
    lock_releases = list(data.get("releases") or DEFAULT_RELEASES)  # type: ignore[call-overload]
    lock_master = bool(data.get("master", True))
    if args.release is not None and sorted(args.release) != sorted(lock_releases):
        raise MirrorError(f"{lock} was written for release(s) {' '.join(lock_releases)}, not "
                          f"{' '.join(args.release)}: pin reproduces the lock, so drop --release, or run "
                          "sync with it to write a new lock")
    if args.master is not None and args.master != lock_master:
        raise MirrorError(f"{lock} was written {'with' if lock_master else 'without'} the master "
                          "thornlist: pin reproduces the lock, so drop --no-master, or run sync with it "
                          "to write a new lock")
    args.release, args.master = lock_releases, lock_master
    root.mkdir(parents=True, exist_ok=True)
    dropped_file = root / DROPPED_NAME
    try:
        known: dict[str, dict[str, str]] = json.loads(dropped_file.read_text())
    except (OSError, ValueError):
        known = {}
    left_out: dict[str, dict[str, str]] = {}
    manifest = manifest_repo(args)
    mkey = f"{manifest.host}/{manifest.path}.git"
    if mkey not in locked:
        raise MirrorError(f"{lock} does not lock the manifest repository {mkey}")
    # The thornlists live in the manifest, so it is pinned before they can
    # say which of its refs they need; its release tags and master are known.
    early = {f"refs/tags/{r}" for r in args.release} | ({MANIFEST_MASTER} if args.master else set())
    what, changed, manifest_dropped = pin_mirror(manifest, locked[mkey], root, rule_root, early,
                                                 known.get(mkey, {}))
    left_out[mkey] = manifest_dropped
    say(f"{manifest.url}: {what}")
    plan = build_plan(load_sources(args, manifest.mirror_dir(root)), args.manifest_url, mdb_url(args), mdb_dir(args))
    check_prefix_safety(plan, all_rules(plan, rule_root))
    report_non_git(plan)
    stale = sorted(k for k in (f"{r.host}/{r.path}.git" for r in plan.repos.values()) if k not in locked)
    if stale:
        raise MirrorError(f"{lock} does not lock {', '.join(stale)}: the thornlists changed since; run sync")
    needed = needed_refs(plan, args.release, args.master, mkey)
    if late := sorted(set(manifest_dropped) & needed[mkey]):
        raise vanished_needed(manifest.url, late)
    # Every repository the lock names, needed now or not, so the mirror root
    # always matches the lock as a whole.
    todo = [repo_from_lock(k, e) for k, e in sorted(locked.items())
            if k != mkey and e.get("source") != "local directory"]
    results, errors = parallel(
        lambda r: pin_mirror(r, locked[f"{r.host}/{r.path}.git"], root, rule_root,
                             needed.get(f"{r.host}/{r.path}.git", set()), known.get(f"{r.host}/{r.path}.git", {})),
        todo, args.jobs)
    touched = [manifest] if changed else []
    for repo, (what, repo_changed, repo_dropped) in sorted(results.items(), key=lambda kv: kv[0].key):
        left_out[f"{repo.host}/{repo.path}.git"] = repo_dropped
        if repo_changed:
            touched.append(repo)
            say(f"{repo.url}: {what}")
    if errors:
        raise MirrorError("\n".join(errors))
    left_out = {k: v for k, v in sorted(left_out.items()) if v}
    if left_out:
        dropped_file.write_text(json.dumps(left_out, indent=1, sort_keys=True) + "\n")
        n = sum(len(v) for v in left_out.values())
        say(f"{n} locked ref(s) left out because upstream lost their commits (listed in {dropped_file})")
    else:
        dropped_file.unlink(missing_ok=True)
    say(f"{len(todo) + 1 - len(touched)} of {len(todo) + 1} mirrors already at the lock")
    for repo in plan.repos.values():
        if repo.local_dir is None:
            continue
        # The checkout's MDB directory is the source of truth: serve it as it
        # is, and only say so when it is not what the lock recorded.
        commit, what = build_mdb(repo, root)
        want = locked.get(f"{repo.host}/{repo.path}.git", {}).get("refs", {}).get(f"refs/heads/{MDB_BRANCH}")
        if commit == want:
            say(f"{repo.url} (branch {MDB_BRANCH} from {repo.local_dir}): at the lock, {commit[:12]}")
        else:
            say(f"note: {repo.local_dir} differs from the locked MDB; branch {MDB_BRANCH} is "
                f"{commit[:12]} (the lock has {str(want)[:12]})")
        if what != "up to date":
            touched.append(repo)
    _, errors = parallel(lambda r: repack(r.mirror_dir(root)), touched, args.jobs)
    if errors:
        raise MirrorError("\n".join(errors))
    return finish(plan, root, rule_root, gitconfig)


def cmd_check(args: argparse.Namespace) -> int:
    root, rule_root, _, gitconfig = paths(args)
    manifest = manifest_repo(args)
    mdir = manifest.mirror_dir(root)
    if not (mdir / "HEAD").is_file():
        raise MirrorError(f"no manifest mirror at {mdir}")
    plan = build_plan(load_sources(args, mdir), args.manifest_url, mdb_url(args), mdb_dir(args))
    problems = check(plan, gitconfig, root, rule_root)
    for p in problems:
        say("CHECK FAILED: " + p)
    if not problems:
        n = sum(len(s.components) for s in plan.sources)
        say(f"ok: {len(plan.sources)} thornlists, {n} components, {len(plan.repos)} repositories, "
            "every URL resolves to its mirror")
    return 1 if problems else 0


def ls_remote(repo: Repo) -> tuple[str | None, dict[str, str]]:
    p = git("ls-remote", "--symref", repo.url, check=False)
    if p.returncode:
        raise MirrorError(f"{repo.url}: {p.stderr.strip().splitlines()[-1] if p.stderr.strip() else 'ls-remote failed'}")
    head, refs = None, {}
    for line in p.stdout.splitlines():
        if line.startswith("ref: ") and line.endswith("\tHEAD"):
            head = line[5:-5]
        elif "\t" in line:
            sha, ref = line.split("\t", 1)
            refs[ref] = sha
    return head, refs


def cmd_list(args: argparse.Namespace) -> int:
    with tempfile.TemporaryDirectory() as tmp:
        if args.manifest_repo:
            mdir = Path(args.manifest_repo)
            if (mdir / ".git").exists():
                mdir = mdir / ".git"
            if not git("--git-dir", str(mdir), "rev-parse", "-q", "--verify", "refs/remotes/origin/master",
                       check=False).returncode:
                # A working clone, as cactup keeps one: its local master is
                # frozen at clone time, and master means origin's.
                mdir = _manifest_view(Path(tmp), mdir)
        else:
            mdir = Path(tmp) / "manifest.git"
            git("init", "--bare", "-q", str(mdir))
            specs = [f"+refs/tags/{r}:refs/tags/{r}" for r in args.release]
            if args.master:
                specs.append(f"+{MANIFEST_MASTER}:{MANIFEST_MASTER}")
            git("--git-dir", str(mdir), "fetch", "-q", "--depth", "1", "--no-tags", args.manifest_url, *specs)
        sources = load_sources(args, mdir)
    plan = build_plan(sources, args.manifest_url, mdb_url(args), mdb_dir(args))
    rule_root = args.rule_root or str(Path(args.root).resolve())
    rules = all_rules(plan, rule_root)
    safety = None
    try:
        check_prefix_safety(plan, rules)
    except MirrorError as e:
        safety = str(e)

    remote: dict[Repo, tuple[str | None, dict[str, str]]] = {}
    errors: list[str] = []
    if args.resolve:
        fetchable = [r for r in plan.repos.values() if r.local_dir is None]
        remote, errors = parallel(ls_remote, fetchable, args.jobs)

    for src in plan.sources:
        git_n = sum(c.ty == "git" for c in src.components)
        other = len(src.components) - git_n
        repos = len({url_key(c.url) for c in src.components if c.ty == "git" and c.url})
        types: dict[str, int] = {}
        for c in src.components:
            if c.ty != "git":
                types[c.ty] = types.get(c.ty, 0) + 1
        extra = ", ".join(f"{n} {t}" for t, n in sorted(types.items()))
        say(f"source {src.label}: {git_n} git checkouts from {repos} repositories"
            + (f", plus {other} other ({extra})" if other else ""))
    say("")
    oddities: list[str] = []
    for repo in sorted(plan.repos.values(), key=lambda r: (r.host, r.path)):
        spell = sorted(repo.spellings)
        branches = sorted(repo.branches, key=lambda b: b or "")
        say(f"{repo.host}/{repo.path}.git")
        for s in spell:
            say(f"    url     {s}  [{', '.join(sorted(repo.spellings[s]))}]")
        for b in branches:
            say(f"    branch  {b or '<default>'}  [{', '.join(sorted(repo.branches[b]))}]")
        if repo.local_dir is not None:
            say(f"    built from {repo.local_dir}")
        if len({s.rstrip('/').removesuffix('.git') for s in spell}) > 1:
            oddities.append(f"{repo.key}: case variants {', '.join(spell)}")
        if any(s.endswith("/") for s in spell):
            oddities.append(f"{repo.key}: a spelling with a trailing /")
        if repo in remote:
            head, refs = remote[repo]
            say(f"    remote  HEAD -> {head}, {sum(r.startswith('refs/heads/') for r in refs)} branches, "
                f"{sum(r.startswith('refs/tags/') and not r.endswith('^{}') for r in refs)} tags, "
                f"{sum(r.startswith('refs/pull/') for r in refs)} pull refs (not mirrored)")
            for b in branches:
                if b and f"refs/heads/{b}" not in refs:
                    oddities.append(f"{repo.url}: no branch {b} upstream (wanted by "
                                    f"{', '.join(sorted(repo.branches[b]))})")
    say("")
    report_non_git(plan)
    say("")
    n_spell = sum(len(r.spellings) for r in plan.repos.values())
    say(f"{len(plan.repos)} repositories, {n_spell} URL spellings, {len(rules)} insteadOf rules")
    for o in oddities:
        say("ODDITY: " + o)
    for e in errors:
        say("UNREACHABLE: " + e)
    if safety:
        say(safety)
    return 1 if errors or safety else 0


def _manifest_view(tmp: Path, clone_gitdir: Path) -> Path:
    """A bare view of a working manifest clone with origin's master as
    refs/heads/master, which is the ref cactup's `master` reads."""
    view = tmp / "manifest-view.git"
    git("clone", "-q", "--bare", "--no-tags", str(clone_gitdir), str(view))
    master = git("--git-dir", str(clone_gitdir), "rev-parse", "refs/remotes/origin/master").stdout.strip()
    git("--git-dir", str(view), "fetch", "-q", str(clone_gitdir), "+refs/tags/*:refs/tags/*")
    git("--git-dir", str(view), "update-ref", MANIFEST_MASTER, master)
    return view


def parse_args(argv: list[str] | None) -> argparse.Namespace:
    common = argparse.ArgumentParser(add_help=False)
    common.add_argument("--root", default=DEFAULT_ROOT, help="mirror root (default %(default)s)")
    common.add_argument("--rule-root", help="the mirror root as the image sees it, for the rules "
                        "(default: --root)")
    common.add_argument("--lock", help=f"lock file: sync writes it, pin reads it (default {DEFAULT_LOCK})")
    common.add_argument("--gitconfig", help=f"gitconfig fragment (default <root>/{GITCONFIG_NAME})")
    common.add_argument("--release", action="append", help="release tag whose thornlist to mirror "
                        f"(repeatable; default {' '.join(DEFAULT_RELEASES)})")
    common.add_argument("--no-master", dest="master", action="store_false", default=None,
                        help="leave out the manifest's master thornlist")
    common.add_argument("--thornlist", action="append", default=[], help="another thornlist file (repeatable)")
    common.add_argument("--no-default-thornlists", action="store_true",
                        help=f"leave out {DEFAULT_THORNLISTS}/*.th")
    common.add_argument("--manifest-url", default=MANIFEST_URL)
    common.add_argument("--mdb-url", default=MDB_URL, help="the URL cactup fetches the MDB from")
    common.add_argument("--mdb-dir", default=str(DEFAULT_MDB_DIR),
                        help="directory to build the mdb branch from (default %(default)s)")
    common.add_argument("--no-mdb", action="store_true", help="leave out the MDB repository")
    common.add_argument("-j", "--jobs", type=int, default=8, help="parallel git operations (default 8)")
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    sub = p.add_subparsers(dest="command", required=True)
    sub.add_parser("sync", parents=[common], help="clone or update every mirror, lock, write rules")
    sub.add_parser("pin", parents=[common], help="re-create the mirrors at a lock file's commits")
    sub.add_parser("check", parents=[common], help="check that every thornlist URL reaches a mirror")
    lp = sub.add_parser("list", parents=[common], help="list repositories and URL spellings, clone nothing")
    lp.add_argument("--manifest-repo", help="read the manifest from this local repository instead of a "
                    "shallow temporary fetch")
    lp.add_argument("--no-resolve", dest="resolve", action="store_false",
                    help="do not ask each repository for its refs")
    args = p.parse_args(argv)
    if args.command != "pin":  # pin takes both from the lock
        args.release = args.release or list(DEFAULT_RELEASES)
        args.master = args.master is not False
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    try:
        return {"sync": cmd_sync, "pin": cmd_pin, "check": cmd_check, "list": cmd_list}[args.command](args)
    except MirrorError as e:
        print(f"mirror.py: error: {e}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    sys.exit(main())
