//! Library packages (`[package] library = true`): a library has no thorns.
//! The package that builds against it says where its checkout goes, with
//! `into` on the dependency, relative to the package's own repo root
//! (KadathThorn's build looks for FUKA at `src/fuka`).
//!
//! In a thornlist that is a section placing the whole repo, `!CHECKOUT = .`
//! under `!NAME`, with a `!TARGET` inside the dependent's clone:
//!
//! ```text
//! !TARGET      = Cactus/repos/KadathThorn/src
//! !TYPE        = git
//! !URL         = https://bitbucket.org/fukaws/fuka
//! !REPO_COMMIT = <the library version's commit>
//! !NAME        = fuka
//! !CHECKOUT    = .
//! ```
//!
//! So the library is cloned like any repo, at `repos/fuka` (fetched, pinned,
//! recorded, refetched and pruned as every other one is), and
//! `repos/KadathThorn/src/fuka` is one relative symlink to it: the same
//! arrangement the Einstein Toolkit's own list makes for FUKA, whose
//! `!TARGET` reaches into `repos/KadathThorn/src/fuka` and links FUKA's
//! top-level items there. Nothing is ever cloned inside another repo's
//! working tree, so no git operation on the dependent (a checkout of a new
//! commit, a forced refetch, a prune) can delete or rewrite the library, and
//! the library's own fetch never touches the dependent.
//!
//! The one write inside the dependent, the link (with a line appended to its
//! `.git/info/exclude`), is made by [`place`], after every clone of the fetch
//! has finished (see `fetch::execute`), in the order [`order`] gives, and only
//! when the checks there pass: a plain relative `into`; a dependent that is
//! really checked out, at its source; no symlink or non-directory on the way;
//! nothing tracked at the path or around it; and nothing at all at the path
//! except the link cactup made. The way is held by file descriptor, so the
//! write lands where the checks looked. It never overwrites, moves or deletes
//! anything but its own link: when a list stops placing a library,
//! [`unplace_dropped`] takes that link away, and only if it is exactly the
//! link [`place`] makes.

use super::link::LinkOutcome;
use crate::thornlist::{Component, Thornlist};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use rustix::fs::{AtFlags, FileType, Mode, OFlags};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

/// One library placed inside another clone: `repos/<dependent>/<into>` is a
/// link to `repos/<repo>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Placement {
    /// The library's clone (`repos/<repo>`), which is also the link's name:
    /// the last component of `into`.
    pub repo: String,
    /// The library's repository, as the thornlist gives it.
    pub url: String,
    /// The clone it goes inside: a package's, or another library's.
    pub dependent: String,
    /// The dependent's source, as the thornlist gives it, to check the clone
    /// on disk against: its URL, and its pin when it has one.
    pub dependent_url: String,
    pub dependent_commit: Option<String>,
    /// Where, relative to the dependent's root, ending in `repo`.
    pub into: String,
}

impl Placement {
    /// The placement as people read it: `KadathThorn/src/fuka`.
    pub fn label(&self) -> String {
        format!("{}/{}", self.dependent, self.into)
    }
}

/// Whether `into` (where a library goes, relative to its dependent's repo
/// root) is a plain path inside the repo: relative, made only of ordinary
/// names (no empty, `.` or `..` part, no root, none starting with `-`, which
/// a tool would read as an option), none of them `.git`, and of
/// characters a thornlist `!TARGET` and `!NAME` carry verbatim. The registry
/// checks this too; cactup does not take its word for it, since the path is
/// written to disk.
pub fn check_into(into: &str) -> Res<()> {
    let plain = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.starts_with('-')
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if into.is_empty() {
        bail!(
            "Where a library goes (into) is empty; it must be a folder inside the dependent's repo, like \"src/fuka\""
        );
    }
    if into.starts_with('/') || Path::new(into).is_absolute() {
        bail!(
            "Where a library goes (into) is \"{}\", an absolute path; it must be relative to the dependent's repo",
            into.escape_debug()
        );
    }
    if let Some(bad) = into.split('/').find(|part| !plain(part)) {
        bail!(
            "Where a library goes (into) is \"{}\", whose part \"{}\" is not a plain folder name; it must be a \
             relative path of plain names, like \"src/fuka\"",
            into.escape_debug(),
            bad.escape_debug()
        );
    }
    if into.split('/').any(|part| part.eq_ignore_ascii_case(".git")) {
        bail!(
            "Where a library goes (into) is \"{}\", inside the repo's .git; cactup will not write there",
            into.escape_debug()
        );
    }
    Ok(())
}

/// The placement `c` describes, if it is one: a `!CHECKOUT = .` section (the
/// whole repo, under `!NAME`) whose `!TARGET` is inside `<root>/repos/<clone>`.
/// `None` for every other component, which links as it always has. Returns
/// the dependent clone and `into`.
pub fn placement_of(root: &str, c: &Component) -> Option<(String, String)> {
    use cactup_ecosystem_common::crl::lexical_canonicalize;
    if c.checkout != "." {
        return None;
    }
    let name = c.name.as_deref()?;
    let target = lexical_canonicalize(&c.target);
    let prefix = format!("{}/repos/", lexical_canonicalize(root));
    let rest = target.strip_prefix(&prefix)?;
    let (dependent, inner) = rest.split_once('/').unwrap_or((rest, ""));
    if dependent.is_empty() {
        return None;
    }
    let into = if inner.is_empty() { name.to_owned() } else { format!("{inner}/{name}") };
    Some((dependent.to_owned(), into))
}

/// The placements in the order they are made: a library that goes inside
/// another library's clone after that library, through the whole chain,
/// whatever order the thornlist lists them in (ties keep the list's order).
/// Refused: a cycle (a library inside itself, or inside one that is inside
/// it), and two placements at one path.
pub fn order(placements: Vec<Placement>) -> Res<Vec<Placement>> {
    // Names compared without case: on a case-insensitive filesystem
    // `src/FUKA` and `src/fuka` are one path, and `Fuka` and `fuka` one clone.
    let fold = |s: &str| s.to_ascii_lowercase();
    let mut at: BTreeMap<(String, String), &Placement> = BTreeMap::new();
    for p in &placements {
        if let Some(other) = at.insert((fold(&p.dependent), fold(&p.into)), p) {
            bail!(
                "Two libraries go to {}: {} and {}; one path holds one checkout",
                p.label(),
                other.url,
                p.url
            );
        }
    }
    // Kahn's algorithm, always taking the earliest-listed placement that is
    // ready: one is ready once every placement of its dependent's clone is
    // made (a clone that is no library, i.e. a package, is ready from the
    // start).
    let libraries: BTreeSet<String> = placements.iter().map(|p| fold(&p.repo)).collect();
    let mut done: Vec<bool> = vec![false; placements.len()];
    let mut placed_repos: BTreeMap<String, usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(placements.len());
    let count = |repo: &str| placements.iter().filter(|p| fold(&p.repo) == repo).count();
    while out.len() < placements.len() {
        let ready = (0..placements.len()).find(|&i| {
            let dependent = fold(&placements[i].dependent);
            !done[i]
                && (!libraries.contains(&dependent)
                    || placed_repos.get(&dependent).copied().unwrap_or(0) == count(&dependent))
        });
        let Some(i) = ready else {
            let stuck: Vec<String> = (0..placements.len())
                .filter(|&i| !done[i])
                .map(|i| format!("{} inside {}", placements[i].repo, placements[i].dependent))
                .collect();
            bail!("Libraries placed inside each other in a cycle: {}", stuck.join(", "));
        };
        done[i] = true;
        *placed_repos.entry(fold(&placements[i].repo)).or_default() += 1;
        out.push(i);
    }
    let mut slots: Vec<Option<Placement>> = placements.into_iter().map(Some).collect();
    Ok(out.into_iter().map(|i| slots[i].take().expect("each index once")).collect())
}

/// What is wrong with the clone at `dir`, if anything: it must be a real
/// directory (not a symlink), a git repository whose working tree is `dir`,
/// whose `origin` is `url`, and, when `commit` is given, whose HEAD is there.
fn check_clone(dir: &Path, what: &str, url: &str, commit: Option<&str>) -> Res<gix::Repository> {
    let meta = match std::fs::symlink_metadata(dir) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!("{what} is not checked out at '{}'", dir.display())
        }
        Err(e) => return Err(anyhow!(e).context(format!("Failed to read '{}'", dir.display()))),
    };
    if meta.file_type().is_symlink() || !meta.is_dir() {
        bail!(
            "'{}' is not {what}'s checkout but a {}",
            dir.display(),
            if meta.file_type().is_symlink() { "symlink" } else { "file" }
        );
    }
    let repo = gix::open(dir)
        .with_context(|| format!("'{}' is not a git checkout of {what}", dir.display()))?;
    let workdir = repo
        .workdir()
        .map(|w| std::fs::canonicalize(w).unwrap_or_else(|_| w.to_owned()));
    if workdir.as_deref() != Some(std::fs::canonicalize(dir)?.as_path()) {
        bail!("'{}' is not the root of a git checkout of {what}", dir.display());
    }
    let origin = repo
        .find_remote("origin")
        .ok()
        .and_then(|r| r.url(gix::remote::Direction::Fetch).map(|u| u.to_bstring().to_string()));
    match origin {
        Some(o) if super::git::normalize_url(&o) == super::git::normalize_url(url) => {}
        Some(o) => bail!("'{}' is a checkout of {o}, not of {what}'s repository {url}", dir.display()),
        None => bail!("'{}' has no origin, so it cannot be told to be {what}'s checkout", dir.display()),
    }
    if let Some(want) = commit {
        let head = repo.head_id().map(|id| id.detach().to_string()).unwrap_or_default();
        if head != want {
            let short = |s: &str| s[..12.min(s.len())].to_owned();
            bail!(
                "{what} at '{}' is at {}, not at {}, the commit the thornlist pins",
                dir.display(),
                short(&head),
                short(want)
            );
        }
    }
    Ok(repo)
}

/// What the dependent tracks that a library at `into` would cover or sit
/// inside: `into` itself or anything under it, or a proper prefix of it that
/// is no plain directory (a file, a link, a submodule's gitlink), in its
/// index or in HEAD's tree. A dependent with no index is refused: what it
/// tracks cannot be told.
fn tracked_at(repo: &gix::Repository, dependent: &str, into: &str) -> Res<Option<String>> {
    let index_path = repo.index_path();
    let index = repo.index().map_err(|e| {
        anyhow!(e).context(format!(
            "{dependent} has no readable index at '{}', so what it tracks at {into} cannot be told",
            index_path.display()
        ))
    })?;
    let parts: Vec<&str> = into.split('/').collect();
    let prefixes: Vec<String> = (1..parts.len()).map(|i| parts[..i].join("/")).collect();
    let under = format!("{into}/");
    if let Some(p) = index
        .entries()
        .iter()
        .map(|e| e.path(&index).to_string())
        .find(|p| *p == into || p.starts_with(&under) || prefixes.contains(p))
    {
        return Ok(Some(p));
    }
    // HEAD, for what the index no longer lists (a path deleted on disk and
    // from the index, but still committed). An unborn HEAD tracks nothing.
    let Ok(commit) = repo.head_commit() else { return Ok(None) };
    let tree = commit.tree().context("Failed to read the dependent's HEAD tree")?;
    if tree
        .lookup_entry_by_path(into)
        .context("Failed to read the dependent's HEAD tree")?
        .is_some()
    {
        return Ok(Some(into.to_owned()));
    }
    for prefix in &prefixes {
        let entry = tree
            .lookup_entry_by_path(prefix)
            .context("Failed to read the dependent's HEAD tree")?;
        if entry.is_some_and(|e| !e.mode().is_tree()) {
            return Ok(Some(prefix.clone()));
        }
    }
    Ok(None)
}

/// What a directory entry is, without following a symlink: `None` when
/// there is nothing.
fn kind_at(dir: &OwnedFd, name: &str, shown: &Path) -> Res<Option<FileType>> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(st) => Ok(Some(FileType::from_raw_mode(st.st_mode))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(e) => Err(anyhow!(e).context(format!("Failed to read '{}'", shown.display()))),
    }
}

/// Open the directory `name` in `dir`, never through a symlink (`O_NOFOLLOW`):
/// whatever is held after this is that very directory, whatever later
/// happens to its path.
fn open_dir_at(dir: impl AsFd, name: &str) -> rustix::io::Result<OwnedFd> {
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    rustix::fs::openat(dir, name, flags, Mode::empty())
}

/// What `kind` is, for a message.
fn kind_name(kind: FileType) -> &'static str {
    match kind {
        FileType::RegularFile => "file",
        FileType::Directory => "directory",
        FileType::Symlink => "symlink",
        _ => "special file",
    }
}

/// The directories on the way to a placement, held open: the dependent,
/// then each part of `into` before the last that exists (`existing` of
/// them); the rest are missing.
struct Way {
    dirs: Vec<OwnedFd>,
    existing: usize,
}

/// Walk `parts` (all of `into` but its last part) from the dependent's
/// directory `dependent`, one part at a time and never through a symlink:
/// each part that exists must be a plain directory, and is opened by
/// `O_NOFOLLOW` from the one before, so nothing the walk holds can be
/// swapped for a link.
fn walk(dependent: OwnedFd, shown: &Path, parts: &[&str]) -> Res<Way> {
    let mut dirs = vec![dependent];
    let mut path = shown.to_path_buf();
    for (i, part) in parts.iter().enumerate() {
        path.push(part);
        let here = dirs.last().expect("the dependent at least");
        match kind_at(here, part, &path)? {
            None => return Ok(Way { dirs, existing: i }),
            Some(FileType::Directory) => {}
            Some(FileType::Symlink) => {
                let to = rustix::fs::readlinkat(here, *part, Vec::new())
                    .map(|t| t.to_string_lossy().into_owned());
                bail!(
                    "'{}' is a symlink (to '{}'); cactup places a library only through plain directories",
                    path.display(),
                    to.unwrap_or_default()
                );
            }
            Some(kind) => {
                bail!(
                    "'{}' is a {}, not a directory, so nothing can go inside it",
                    path.display(),
                    kind_name(kind)
                )
            }
        }
        let next = open_dir_at(here, part).map_err(|e| {
            anyhow!(e).context(format!("'{}' changed while it was being read", path.display()))
        })?;
        dirs.push(next);
    }
    Ok(Way { existing: parts.len(), dirs })
}

/// The link's text: from the directory it sits in (`depth` levels below
/// `repos/`) back up to `repos/<repo>`.
fn link_text(depth: usize, repo: &str) -> PathBuf {
    let mut p = PathBuf::new();
    for _ in 0..depth {
        p.push("..");
    }
    p.push(repo);
    p
}

/// Whether the link `name` in `dir` reads exactly `text`.
fn link_reads(dir: &OwnedFd, name: &str, text: &Path) -> bool {
    rustix::fs::readlinkat(dir, name, Vec::new())
        .is_ok_and(|t| Path::new(OsStr::from_bytes(t.as_bytes())) == text)
}

/// The dependent's `.git/info/exclude`, checked before anything is written:
/// `.git` a real directory (not a symlink, nor a `gitdir:` file pointing
/// elsewhere), `info` a real directory or missing, `exclude` a plain file or
/// missing. Holds `.git` and, when it exists, `info`.
struct Exclude {
    git: OwnedFd,
    info: Option<OwnedFd>,
    shown: PathBuf,
}

pub(crate) const EXCLUDE_COMMENT: &str = "# A library cactup placed (a link to its own clone)";

/// The prefix of the temporaries [`Exclude::remove`] writes the new exclude
/// to, in `.git/info`, before renaming it over the exclude: each is
/// `<prefix><pid>-<n>`, unique to its process.
const EXCLUDE_TEMP: &str = ".exclude.cactup-";

/// Unlink the temporaries a dead process left in `info`: names with the
/// exact [`EXCLUDE_TEMP`] prefix whose pid is not running (or that carry no
/// pid, as an older cactup's fixed name did). Unlinking never follows a
/// link, and nothing is ever written through one; a live process's
/// temporary is left alone. Best effort.
fn clear_stale_temporaries(info: &OwnedFd) {
    let Ok(dir) = rustix::fs::Dir::read_from(info) else { return };
    let mut stale = Vec::new();
    for entry in dir.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(rest) = name.strip_prefix(EXCLUDE_TEMP) else { continue };
        let pid = rest
            .split('-')
            .next()
            .and_then(|p| p.parse::<i32>().ok())
            .and_then(rustix::process::Pid::from_raw);
        let alive = pid.is_some_and(|pid| {
            !matches!(rustix::process::test_kill_process(pid), Err(rustix::io::Errno::SRCH))
        });
        if !alive {
            stale.push(name);
        }
    }
    for name in stale {
        let _ = rustix::fs::unlinkat(info, name.as_str(), AtFlags::empty());
    }
}

/// A line of the exclude without its line ending (`\n`, and a `\r` before it).
fn bare(line: &[u8]) -> &[u8] {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    line.strip_suffix(b"\r").unwrap_or(line)
}

impl Exclude {
    fn check(dependent: &OwnedFd, shown: &Path) -> Res<Exclude> {
        let git_path = shown.join(".git");
        match kind_at(dependent, ".git", &git_path)? {
            Some(FileType::Directory) => {}
            Some(kind) => bail!(
                "'{}' is a {}, not the repository's own directory; cactup writes the library's exclude only into a \
                 plain .git directory",
                git_path.display(),
                kind_name(kind)
            ),
            None => bail!("'{}' is missing", git_path.display()),
        }
        let git = open_dir_at(dependent, ".git").map_err(|e| {
            anyhow!(e).context(format!("'{}' changed while it was being read", git_path.display()))
        })?;
        let info_path = git_path.join("info");
        let info = match kind_at(&git, "info", &info_path)? {
            None => None,
            Some(FileType::Directory) => Some(open_dir_at(&git, "info").map_err(|e| {
                anyhow!(e).context(format!("'{}' changed while it was being read", info_path.display()))
            })?),
            Some(kind) => bail!(
                "'{}' is a {}, not a directory; not writing an exclude there",
                info_path.display(),
                kind_name(kind)
            ),
        };
        let shown = info_path.join("exclude");
        if let Some(info) = &info {
            match kind_at(info, "exclude", &shown)? {
                None | Some(FileType::RegularFile) => {}
                Some(kind) => bail!(
                    "'{}' is a {}, not a plain file; not writing to it",
                    shown.display(),
                    kind_name(kind)
                ),
            }
        }
        Ok(Exclude { git, info, shown })
    }

    /// Append `/<into>` unless it is there: opened with `O_APPEND` and
    /// `O_NOFOLLOW` (created if missing), so the file is never truncated or
    /// rewritten, and never written through a link.
    fn add(&mut self, into: &str) -> Res<()> {
        let line = format!("/{into}");
        if self.info.is_none() {
            rustix::fs::mkdirat(&self.git, "info", Mode::from_raw_mode(0o755)).map_err(|e| {
                anyhow!(e)
                    .context(format!("Failed to create '{}'", self.shown.with_file_name("").display()))
            })?;
            self.info = Some(open_dir_at(&self.git, "info")?);
        }
        let info = self.info.as_ref().expect("made above");
        let text = self.read(info)?.map(|(text, _)| text).unwrap_or_default();
        if text.split(|b| *b == b'\n').any(|l| bare(l) == line.as_bytes()) {
            return Ok(());
        }
        let flags =
            OFlags::WRONLY | OFlags::APPEND | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(info, "exclude", flags, Mode::from_raw_mode(0o644))
            .map_err(|e| anyhow!(e).context(format!("Failed to open '{}'", self.shown.display())))?;
        let lead = if text.is_empty() || text.ends_with(b"\n") { "" } else { "\n" };
        std::fs::File::from(fd)
            .write_all(format!("{lead}{EXCLUDE_COMMENT}\n{line}\n").as_bytes())
            .with_context(|| format!("Failed to append to '{}'", self.shown.display()))
    }

    /// The exclude's bytes (it need not be UTF-8: git reads it as bytes) and
    /// its permission bits, read without following a link; `None` when there
    /// is no exclude.
    fn read(&self, info: &OwnedFd) -> Res<Option<(Vec<u8>, u32)>> {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = match rustix::fs::openat(info, "exclude", flags, Mode::empty()) {
            Ok(fd) => fd,
            Err(rustix::io::Errno::NOENT) => return Ok(None),
            Err(e) => {
                return Err(anyhow!(e).context(format!("Failed to read '{}'", self.shown.display())));
            }
        };
        let mode = rustix::fs::fstat(&fd).map(|st| st.st_mode & 0o7777).unwrap_or(0o644);
        let mut text = Vec::new();
        std::fs::File::from(fd)
            .read_to_end(&mut text)
            .with_context(|| format!("Failed to read '{}'", self.shown.display()))?;
        Ok(Some((text, mode)))
    }

    /// Take away the comment and path pair [`Exclude::add`] appended for
    /// `into`, leaving every other line byte for byte: the new bytes go to a
    /// temporary file beside it, which then replaces it in one rename. The
    /// temporary is this process's own (`<EXCLUDE_TEMP><pid>-<n>`), made
    /// exclusively and never through a link, so two cactups never meet on
    /// it; a dead process's left there is cleared first. Returns whether
    /// there was a pair.
    fn remove(&mut self, into: &str) -> Res<bool> {
        let Some(info) = &self.info else { return Ok(false) };
        let Some((text, mode)) = self.read(info)? else { return Ok(false) };
        let line = format!("/{into}");
        let pieces: Vec<&[u8]> = text.split_inclusive(|b| *b == b'\n').collect();
        let mut kept = Vec::with_capacity(text.len());
        let mut removed = false;
        let mut i = 0;
        while i < pieces.len() {
            if bare(pieces[i]) == EXCLUDE_COMMENT.as_bytes()
                && pieces.get(i + 1).is_some_and(|next| bare(next) == line.as_bytes())
            {
                removed = true;
                i += 2;
                continue;
            }
            kept.extend_from_slice(pieces[i]);
            i += 1;
        }
        if !removed {
            return Ok(false);
        }
        clear_stale_temporaries(info);
        static SEQ: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let temp = format!("{EXCLUDE_TEMP}{}-{n}", std::process::id());
        let temp = temp.as_str();
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let out = rustix::fs::openat(info, temp, flags, Mode::from_raw_mode(mode)).map_err(|e| {
            anyhow!(e).context(format!("Failed to write beside '{}'", self.shown.display()))
        })?;
        let written = std::fs::File::from(out).write_all(&kept);
        let renamed = written.map_err(anyhow::Error::from).and_then(|()| {
            rustix::fs::renameat(info, temp, info, "exclude").map_err(anyhow::Error::from)
        });
        if let Err(e) = renamed {
            let _ = rustix::fs::unlinkat(info, temp, AtFlags::empty());
            return Err(e.context(format!("Failed to rewrite '{}'", self.shown.display())));
        }
        Ok(true)
    }
}

/// Proof that a fetch's clones are over: `fetch::execute` makes one only once
/// its worker pool has been joined, and [`place`] takes one, so a library is
/// never placed from inside the pool, while the clone it goes into may still
/// be being cloned or checked out. Sequencing, not timing.
pub struct ClonesDone(());

impl ClonesDone {
    /// Only for after the pool (and tests, which run no pool).
    pub(super) fn after_the_pool() -> ClonesDone {
        ClonesDone(())
    }
}

/// `repos/` canonicalized and held open, with its path for messages.
fn open_repos(install_root: &Path, root: &str) -> Res<(PathBuf, OwnedFd)> {
    let repos = install_root.join(root).join("repos");
    let repos = std::fs::canonicalize(&repos)
        .with_context(|| format!("No repos directory at '{}'", repos.display()))?;
    let fd =
        rustix::fs::open(&repos, OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|e| anyhow!(e).context(format!("Failed to open '{}'", repos.display())))?;
    Ok((repos, fd))
}

/// Place one library: `repos/<dependent>/<into>` becomes a relative link to
/// `repos/<repo>`. Run after every clone of the fetch has finished (hence
/// [`ClonesDone`]). `failed` names what failed to fetch in this run, and
/// `unplaced` the libraries not placed: a placement whose dependent or
/// library is among them is not attempted.
///
/// Refused, with the path named, and nothing written: an `into` that is not a
/// plain relative path; a dependent that is not checked out, or not at its
/// source (`origin` and pin); a library clone likewise; any symlink or
/// non-directory on the way; a path the dependent tracks (in its index or
/// HEAD), or one inside something it tracks that is no directory (a
/// submodule); a `.git` that is no plain directory; and anything already at
/// the path other than the link this would make.
///
/// Every check comes before the first write. The way is walked by file
/// descriptor, each directory opened `O_NOFOLLOW` from the one before, and
/// the missing folders and the link are made relative to those descriptors
/// (`mkdirat`, `symlinkat`, which never replaces), so nothing swapped in on
/// the way between the check and the write is followed. If appending the
/// exclude fails, what this call made (the link, and the folders on the way)
/// is taken away again.
pub fn place(
    _clones_done: &ClonesDone,
    install_root: &Path,
    root: &str,
    p: &Placement,
    failed: &BTreeSet<String>,
    unplaced: &BTreeSet<String>,
) -> Res<LinkOutcome> {
    place_between(install_root, root, p, failed, unplaced, &|| {})
}

/// [`place`], calling `between` after every check and before the first
/// write: the window a race would use, for a test to use it.
fn place_between(
    install_root: &Path,
    root: &str,
    p: &Placement,
    failed: &BTreeSet<String>,
    unplaced: &BTreeSet<String>,
    between: &dyn Fn(),
) -> Res<LinkOutcome> {
    check_into(&p.into)?;
    let parts: Vec<&str> = p.into.split('/').collect();
    let (name, on_the_way) = parts.split_last().expect("check_into: not empty");
    if *name != p.repo {
        bail!(
            "The library {} would be linked as '{name}', not under its own name (a bug in the plan)",
            p.repo
        );
    }
    if failed.contains(&p.dependent) {
        bail!("Not placed: {}, which it goes inside, did not fetch", p.dependent);
    }
    if failed.contains(&p.repo) {
        bail!("Not placed: its clone {} did not fetch", p.repo);
    }
    if unplaced.contains(&p.dependent) {
        bail!(
            "Not placed: {}, which it goes inside, is a library that was not placed itself",
            p.dependent
        );
    }

    let (repos, repos_fd) = open_repos(install_root, root)?;
    let dependent_dir = repos.join(&p.dependent);
    let dependent = check_clone(
        &dependent_dir,
        &p.dependent,
        &p.dependent_url,
        p.dependent_commit.as_deref(),
    )
    .context(
        "Not placed: cactup puts a library only inside its dependent's own checkout, never creating it",
    )?;
    let library_dir = repos.join(&p.repo);
    check_clone(&library_dir, &p.repo, &p.url, None).context("Not placed")?;
    if let Some(path) = tracked_at(&dependent, &p.dependent, &p.into).context("Not placed")? {
        bail!(
            "Not placed: {} tracks '{path}', so '{}' is the dependent's own; cactup will not put a library over it \
             or inside it",
            p.dependent,
            dependent_dir.join(&p.into).display()
        );
    }
    let dependent_fd = open_dir_at(&repos_fd, &p.dependent).map_err(|e| {
        anyhow!(e).context(format!(
            "Not placed: '{}' changed while it was being read",
            dependent_dir.display()
        ))
    })?;
    let mut exclude = Exclude::check(&dependent_fd, &dependent_dir).context("Not placed")?;
    let way = walk(dependent_fd, &dependent_dir, on_the_way).context("Not placed")?;

    let target = dependent_dir.join(&p.into);
    let want = repos.join(&p.repo);
    let text = link_text(parts.len(), &p.repo);
    if way.existing == on_the_way.len() {
        let parent = way.dirs.last().expect("the dependent at least");
        match kind_at(parent, name, &target)? {
            None => {}
            Some(FileType::Symlink) if link_reads(parent, name, &text) => {
                exclude.add(&p.into).context("Not placed")?;
                return Ok(LinkOutcome::Unchanged);
            }
            Some(FileType::Symlink) => {
                let to = rustix::fs::readlinkat(parent, *name, Vec::new())
                    .map(|t| t.to_string_lossy().into_owned());
                bail!(
                    "Not placed: '{}' is already a link, to '{}', which cactup did not make for {}; move it aside",
                    target.display(),
                    to.unwrap_or_default(),
                    p.repo
                );
            }
            Some(FileType::Directory) => bail!(
                "Not placed: '{}' is already a directory; cactup never overwrites one (a library is a link to its \
                 own clone, '{}'); move it aside",
                target.display(),
                want.display()
            ),
            Some(kind) => bail!(
                "Not placed: '{}' is already a {}; cactup never overwrites one; move it aside",
                target.display(),
                kind_name(kind)
            ),
        }
    }

    between();

    // The writes, relative to the directories held: the missing folders
    // (mkdirat fails on anything there), then the link (symlinkat never
    // replaces), then the exclude. Any failure takes back what this call made.
    let Way { mut dirs, existing } = way;
    let mut made: Vec<usize> = Vec::new();
    let result = (|| -> Res<()> {
        for (i, part) in on_the_way.iter().enumerate().skip(existing) {
            let here = dirs.last().expect("the dependent at least");
            let shown = dependent_dir.join(on_the_way[..=i].join("/"));
            rustix::fs::mkdirat(here, *part, Mode::from_raw_mode(0o755))
                .map_err(|e| anyhow!(e).context(format!("Failed to create '{}'", shown.display())))?;
            made.push(i);
            let next = open_dir_at(here, part).map_err(|e| {
                anyhow!(e).context(format!("'{}' changed while it was being made", shown.display()))
            })?;
            dirs.push(next);
        }
        let parent = dirs.last().expect("the dependent at least");
        rustix::fs::symlinkat(&text, parent, *name).map_err(|e| {
            anyhow!(e).context(format!("Failed to link '{}' to {}", target.display(), want.display()))
        })?;
        if let Err(e) = exclude.add(&p.into) {
            if link_reads(parent, name, &text) {
                let _ = rustix::fs::unlinkat(parent, *name, AtFlags::empty());
            }
            return Err(e);
        }
        Ok(())
    })();
    if let Err(e) = result {
        // The folders this call made, deepest first, each from the one it
        // is in; rmdir takes only an empty one.
        for &i in made.iter().rev() {
            let _ = rustix::fs::unlinkat(&dirs[i], on_the_way[i], AtFlags::REMOVEDIR);
        }
        return Err(e.context("Not placed; nothing was left behind"));
    }
    Ok(LinkOutcome::Created)
}

/// What dropping one placement found and did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dropped {
    pub dependent: String,
    pub into: String,
    pub repo: String,
    /// cactup's link was there and was taken away.
    pub link_removed: bool,
    /// The exclude line pair placing appended was there and was taken away.
    pub exclude_removed: bool,
}

impl Dropped {
    /// The placement as people read it: `KadathThorn/src/fuka`.
    pub fn label(&self) -> String {
        format!("{}/{}", self.dependent, self.into)
    }
}

/// Take a placement away: its link at `repos/<dependent>/<into>`, only when
/// that is the symlink [`place`] makes there (its text exactly the relative
/// path to `repos/<repo>`), and the exclude lines placing appended to the
/// dependent's `.git/info/exclude` (only that comment and path pair). Walked
/// by descriptor and never through a link.
///
/// Whatever else is there is the dependent's own now (its new commit tracks
/// the path, or made a folder on the way a link) and is left as it is, with
/// a note: removing nothing is always safe, and the link pass refuses a
/// target that would resolve outside the installation through anything left.
pub fn unplace(
    install_root: &Path,
    root: &str,
    dependent: &str,
    into: &str,
    repo: &str,
    notes: &mut Vec<String>,
) -> Res<Dropped> {
    check_into(into)?;
    check_into(dependent)?;
    let mut done = Dropped {
        dependent: dependent.to_owned(),
        into: into.to_owned(),
        repo: repo.to_owned(),
        link_removed: false,
        exclude_removed: false,
    };
    let parts: Vec<&str> = into.split('/').collect();
    let (name, on_the_way) = parts.split_last().expect("check_into: not empty");
    let Ok((repos, repos_fd)) = open_repos(install_root, root) else { return Ok(done) };
    let dependent_dir = repos.join(dependent);
    let target = dependent_dir.join(into);
    let dependent_fd = match kind_at(&repos_fd, dependent, &dependent_dir)? {
        None => return Ok(done),
        Some(FileType::Directory) => match open_dir_at(&repos_fd, dependent) {
            Ok(fd) => fd,
            Err(e) => {
                notes.push(format!(
                    "Left {}: '{}' could not be opened ({e})",
                    done.label(),
                    dependent_dir.display()
                ));
                return Ok(done);
            }
        },
        Some(kind) => {
            notes.push(format!(
                "Left {}: '{}' is a {}, not {dependent}'s checkout",
                done.label(),
                dependent_dir.display(),
                kind_name(kind)
            ));
            return Ok(done);
        }
    };
    let exclude = Exclude::check(&dependent_fd, &dependent_dir);
    match walk(dependent_fd, &dependent_dir, on_the_way) {
        Err(e) => notes.push(format!("No link of {repo} to take away at '{}': {e:#}", target.display())),
        Ok(way) if way.existing < on_the_way.len() => {}
        Ok(way) => {
            let parent = way.dirs.last().expect("the dependent at least");
            let text = link_text(parts.len(), repo);
            match kind_at(parent, name, &target)? {
                None => {}
                Some(FileType::Symlink) if link_reads(parent, name, &text) => {
                    rustix::fs::unlinkat(parent, *name, AtFlags::empty())
                        .map_err(|e| anyhow!(e).context(format!("Failed to remove '{}'", target.display())))?;
                    done.link_removed = true;
                }
                Some(kind) => notes.push(format!(
                    "'{}', where {repo} was linked in, is a {} cactup did not make (the dependent's own, now); \
                     left as it is",
                    target.display(),
                    kind_name(kind)
                )),
            }
        }
    }
    // The exclude pair, after the link: a failure here (a read-only
    // `.git/info`, a full disk) never undoes what went, nor hides it. It is
    // a note, and the pair left behind is still covered by prune, which
    // counts files at a path cactup excluded as the user's.
    let left = match exclude.and_then(|mut exclude| exclude.remove(into)) {
        Ok(removed) => {
            done.exclude_removed = removed;
            return Ok(done);
        }
        Err(e) => e,
    };
    notes.push(format!(
        "{repo}'s exclude line for /{into} in {dependent} was left ({left:#}){}; prune still counts whatever is put \
         at that path as the user's",
        if done.link_removed { ", though its link was taken away" } else { "" }
    ));
    Ok(done)
}

/// Put back what [`unplace`] took away: the link (when the way to it is
/// still there, by descriptor, never replacing anything) and the exclude
/// lines. For a command that dropped placements and then failed.
fn replace(install_root: &Path, root: &str, d: &Dropped) -> Res<()> {
    let parts: Vec<&str> = d.into.split('/').collect();
    let (name, on_the_way) = parts.split_last().expect("checked when dropped");
    let (repos, repos_fd) = open_repos(install_root, root)?;
    let dependent_dir = repos.join(&d.dependent);
    let dependent_fd = open_dir_at(&repos_fd, &d.dependent)
        .map_err(|e| anyhow!(e).context(format!("Failed to open '{}'", dependent_dir.display())))?;
    let mut exclude = Exclude::check(&dependent_fd, &dependent_dir)?;
    if d.link_removed {
        let way = walk(dependent_fd, &dependent_dir, on_the_way)?;
        if way.existing < on_the_way.len() {
            bail!("the way to '{}' is gone", dependent_dir.join(&d.into).display());
        }
        let parent = way.dirs.last().expect("the dependent at least");
        rustix::fs::symlinkat(link_text(parts.len(), &d.repo), parent, *name).map_err(|e| {
            anyhow!(e)
                .context(format!("Failed to link '{}' again", dependent_dir.join(&d.into).display()))
        })?;
    }
    if d.exclude_removed {
        exclude.add(&d.into)?;
    }
    Ok(())
}

/// The placements `before` makes that `after` does not (the package was
/// removed, its library's `into` changed, or the base took the library
/// over), as `(dependent, into, repo)`, compared without case.
pub fn dropped(before: &Thornlist, after: &Thornlist) -> Vec<(String, String, String)> {
    let key = |d: &str, i: &str| (d.to_ascii_lowercase(), i.to_ascii_lowercase());
    let kept: BTreeSet<(String, String)> = after
        .components()
        .iter()
        .filter_map(|c| placement_of(after.root(), c))
        .map(|(d, i)| key(&d, &i))
        .collect();
    let mut out: Vec<(String, String, String)> = Vec::new();
    for c in before.components() {
        if let Some((d, i)) = placement_of(before.root(), c)
            && !kept.contains(&key(&d, &i))
            && !out.iter().any(|(od, oi, _)| key(od, oi) == key(&d, &i))
        {
            out.push((d, i, c.repo.clone()));
        }
    }
    out
}

/// What [`unplace_dropped`] took away, and the notes on what it left.
#[derive(Debug, Default)]
pub struct Unplacing {
    root: String,
    pub dropped: Vec<Dropped>,
    pub notes: Vec<String>,
}

impl Unplacing {
    /// The links taken away, as `dependent/into`.
    pub fn links(&self) -> Vec<String> {
        self.dropped.iter().filter(|d| d.link_removed).map(Dropped::label).collect()
    }

    /// Put everything back, for a command that failed after dropping: what
    /// cannot be put back is said, with the command that restores it.
    pub fn restore(&self, install_root: &Path) -> Vec<String> {
        let mut problems = Vec::new();
        for d in self.dropped.iter().filter(|d| d.link_removed || d.exclude_removed) {
            if let Err(e) = replace(install_root, &self.root, d) {
                let link = install_root.join(&self.root).join("repos").join(&d.dependent).join(&d.into);
                let text = link_text(d.into.split('/').count(), &d.repo);
                // `ln -s` into a directory there would make the link inside it.
                let aside = if std::fs::symlink_metadata(&link).is_ok() {
                    format!("move '{}' aside first (something is there now), then ", link.display())
                } else {
                    String::new()
                };
                problems.push(format!(
                    "{} could not be put back ({e:#}); {aside}restore it with `ln -s {} {}`",
                    d.label(),
                    text.display(),
                    link.display()
                ));
            }
        }
        problems
    }
}

/// The undo of a command's library changes, armed from when it drops
/// placements until it commits (writes the list that drops them). Dropped
/// while armed (an error returned by `?`, a refusal, an interrupt), it takes
/// away the placements the command newly made (those `after` makes and
/// `before` does not) and puts back what it dropped, so a failed command
/// leaves the placements as they were; what it cannot undo it prints, with
/// how.
pub struct PutBack<'a> {
    install_root: &'a Path,
    unplacing: Unplacing,
    /// The placements this run made (`Created` in its fetch report), as
    /// `(root, dependent, into, repo)`: the only ones an undo takes away.
    made: Vec<(String, String, String, String)>,
    armed: bool,
}

impl<'a> PutBack<'a> {
    /// Drop the placements `before` makes and `after` does not
    /// ([`unplace_dropped`]), armed to undo that.
    pub fn drop_placements(install_root: &'a Path, before: &str, after: &str) -> Res<PutBack<'a>> {
        let unplacing = unplace_dropped(install_root, before, after)?;
        Ok(PutBack { install_root, unplacing, made: Vec::new(), armed: true })
    }

    /// Note the placements `report` says this run created (a link that was
    /// there already reads `Unchanged`, and is never this run's to undo).
    pub fn made(&mut self, plan: &super::Plan, report: &super::ExecReport) {
        for p in &plan.libraries {
            let label = p.label();
            if report.links.iter().any(|(l, o)| *l == label && *o == LinkOutcome::Created) {
                self.made
                    .push((plan.root.clone(), p.dependent.clone(), p.into.clone(), p.repo.clone()));
            }
        }
    }

    /// What was dropped, and the notes on what was left.
    pub fn unplacing(&self) -> &Unplacing {
        &self.unplacing
    }

    /// The command went through: nothing to undo.
    pub fn commit(mut self) -> Unplacing {
        self.armed = false;
        std::mem::take(&mut self.unplacing)
    }

    /// Undo now, returning what could not be undone, each with how.
    pub fn undo(&mut self) -> Vec<String> {
        self.armed = false;
        let mut problems = Vec::new();
        for (root, dependent, into, repo) in std::mem::take(&mut self.made) {
            if let Err(e) = unplace(self.install_root, &root, &dependent, &into, &repo, &mut problems) {
                problems.push(format!(
                    "{dependent}/{into}, which this run placed, could not be taken away: {e:#}"
                ));
            }
        }
        problems.extend(self.unplacing.restore(self.install_root));
        problems
    }
}

impl Drop for PutBack<'_> {
    fn drop(&mut self) {
        if self.armed {
            for problem in self.undo() {
                eprintln!("{problem}");
            }
        }
    }
}

/// Take away the placements `before_text` makes and `after_text` does not
/// ([`dropped`], [`unplace`]), before `after_text` is fetched: a stale link
/// left in a dependent would have the next list's sections resolved through
/// it, and a stale exclude line would hide what the user puts there. A
/// `before_text` that does not parse places nothing.
pub fn unplace_dropped(install_root: &Path, before_text: &str, after_text: &str) -> Res<Unplacing> {
    let Ok(before) = crate::thornlist::parse(before_text) else { return Ok(Unplacing::default()) };
    let after = crate::thornlist::parse(after_text)?;
    let mut out = Unplacing { root: before.root().to_owned(), ..Unplacing::default() };
    for (dependent, into, repo) in dropped(&before, &after) {
        let done = unplace(install_root, before.root(), &dependent, &into, &repo, &mut out.notes)
            .with_context(|| format!("{repo} is no longer placed at {dependent}/{into}"));
        match done {
            Ok(d) => out.dropped.push(d),
            Err(e) => {
                // What was already taken away goes back before the error.
                let problems = out.restore(install_root);
                return Err(problems.into_iter().fold(e, |e, p| e.context(p)));
            }
        }
    }
    Ok(out)
}

/// The paths cactup excluded for a library in the repo at `repo_dir` (its
/// comment and path pairs in `.git/info/exclude`) that hold something now
/// other than a link: the user's own files, which git ignores there, so a
/// status walk does not list them. Prune counts them as untracked.
pub fn hidden_by_library_excludes(repo_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read(repo_dir.join(".git/info/exclude")) else { return Vec::new() };
    let lines: Vec<&[u8]> = text.split(|b| *b == b'\n').map(bare).collect();
    let mut out = Vec::new();
    for pair in lines.windows(2) {
        if pair[0] == EXCLUDE_COMMENT.as_bytes()
            && let Some(path) = pair[1].strip_prefix(b"/").and_then(|p| std::str::from_utf8(p).ok())
            && check_into(path).is_ok()
            && std::fs::symlink_metadata(repo_dir.join(path)).is_ok_and(|m| !m.file_type().is_symlink())
        {
            out.push(path.to_owned());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(repo: &str, dependent: &str, into: &str) -> Placement {
        Placement {
            repo: repo.into(),
            url: format!("https://x/{repo}"),
            dependent: dependent.into(),
            dependent_url: format!("https://x/{dependent}"),
            dependent_commit: None,
            into: into.into(),
        }
    }

    #[test]
    fn into_must_be_a_plain_relative_path() {
        for ok in ["src/fuka", "fuka", "a/b/c", "lib-1.2_x"] {
            check_into(ok).unwrap_or_else(|e| panic!("{ok}: {e:#}"));
        }
        for bad in [
            "",
            "/abs/fuka",
            "../fuka",
            "src/../../x",
            "src/..",
            ".",
            "./fuka",
            "src/./fuka",
            "src//fuka",
            "src/",
            "a b",
            "x$y",
            ".git/hooks",
            "src/.GIT",
            "a\\b",
        ] {
            assert!(check_into(bad).is_err(), "{bad:?} must be refused");
        }
        let err = format!("{:#}", check_into("../x").unwrap_err());
        assert!(err.contains("not a plain folder name"), "{err}");
        let err = format!("{:#}", check_into("/x").unwrap_err());
        assert!(err.contains("absolute"), "{err}");
    }

    /// The library listed before its dependent, and a two-level chain listed
    /// backwards: each comes out after the clone it goes inside.
    #[test]
    fn placements_follow_the_chain_whatever_the_list_order() {
        let fuka = placement("fuka", "KadathThorn", "src/fuka");
        let inner = placement("eigen", "fuka", "external/eigen");
        let innermost = placement("blas", "eigen", "deps/blas");
        let got = order(vec![innermost.clone(), inner.clone(), fuka.clone()]).unwrap();
        assert_eq!(got, [fuka.clone(), inner.clone(), innermost.clone()]);
        // Already in order: kept.
        let got = order(vec![fuka.clone(), inner.clone()]).unwrap();
        assert_eq!(got, [fuka, inner]);
    }

    #[test]
    fn a_cycle_of_libraries_is_refused() {
        let err = order(vec![placement("a", "b", "x/a"), placement("b", "a", "y/b")]).unwrap_err();
        assert!(format!("{err:#}").contains("cycle"), "{err:#}");
        let err = order(vec![placement("a", "a", "x/a")]).unwrap_err();
        assert!(format!("{err:#}").contains("cycle"), "{err:#}");
    }

    #[test]
    fn two_libraries_at_one_path_are_refused() {
        let mut other = placement("fuka", "KadathThorn", "src/fuka");
        other.url = "https://y/fuka".into();
        let err = order(vec![placement("fuka", "KadathThorn", "src/fuka"), other]).unwrap_err();
        assert!(format!("{err:#}").contains("Two libraries go to KadathThorn/src/fuka"), "{err:#}");
    }

    #[test]
    fn a_whole_repo_section_inside_repos_is_a_placement() {
        let list = crate::thornlist::parse(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
             !TARGET = $ROOT/repos/KadathThorn/src\n!TYPE = git\n!URL = https://x/fuka\n!NAME = fuka\n!CHECKOUT = .\n\n\
             !TARGET = $ROOT/repos/KadathThorn\n!TYPE = git\n!URL = https://x/other\n!NAME = other\n!CHECKOUT = .\n\n\
             !TARGET = $ROOT/arrangements/Fuka/KadathThorn/src/fuka/../../../../repos/KadathThorn/src/fuka\n!TYPE = git\n\
             !URL = https://x/fuka2\n!CHECKOUT = Cmake src\n",
        )
        .unwrap();
        let got: Vec<_> = list.components().iter().map(|c| placement_of(list.root(), c)).collect();
        assert_eq!(got[0], Some(("KadathThorn".into(), "src/fuka".into())));
        assert_eq!(got[1], Some(("KadathThorn".into(), "other".into())));
        assert!(
            got[2..].iter().all(Option::is_none),
            "the ET's item links are not a placement: {got:?}"
        );
    }

    // On disk: real repos, fetched by `plan` and `execute` as `cactup add`
    // and `refetch` fetch them.

    use crate::fetch::git::testrepo;
    use crate::fetch::tests::commit_files;

    /// Upstreams for KadathThorn (a thorn whose build looks for FUKA at
    /// `src/fuka`) and FUKA, under `<tmp>/up`, and an installation root,
    /// `<tmp>/install`.
    struct Setup {
        tmp: tempfile::TempDir,
        kadath: PathBuf,
        fuka: PathBuf,
    }

    impl Setup {
        fn new() -> Setup {
            let tmp = tempfile::tempdir().unwrap();
            let kadath = tmp.path().join("up/KadathThorn");
            let fuka = tmp.path().join("up/fuka");
            testrepo::init(&kadath);
            testrepo::init(&fuka);
            Setup { tmp, kadath, fuka }
        }

        fn install(&self) -> PathBuf {
            self.tmp.path().join("install")
        }

        fn repos(&self) -> PathBuf {
            self.install().join("Cactus/repos")
        }

        /// The list a package add composes, the library's section first:
        /// the order the sections come in must not matter.
        fn list(&self, kadath: &gix::ObjectId, fuka: &gix::ObjectId, extra: &str) -> String {
            format!(
                "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\n{extra}\
                 !TARGET = $ROOT/repos/KadathThorn/src\n!TYPE = git\n!URL = {}\n!REPO_COMMIT = {fuka}\n!NAME = fuka\n!CHECKOUT = .\n\n\
                 !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {}\n!REPO_PATH = ../$2\n!REPO_COMMIT = {kadath}\n\
                 !CHECKOUT =\nFuka/KadathThorn\n",
                self.fuka.display(),
                self.kadath.display()
            )
        }

        fn fetch(&self, list: &str) -> super::super::ExecReport {
            let list = crate::thornlist::parse(list).unwrap_or_else(|e| panic!("{e:#}\n{list}"));
            let mut progress = prodash::tree::Root::new().add_child("plan");
            let plan = super::super::plan(&list, &self.install(), &mut progress)
                .unwrap_or_else(|e| panic!("{e:#}"));
            assert!(plan.skipped.is_empty(), "nothing is skipped: {:?}", plan.skipped);
            let report = super::super::execute(&plan, &self.install()).unwrap();
            // As `add` and `refetch` do: the record is what lets the next
            // fetch move a pin cactup itself checked out.
            super::super::FetchState::record(&self.install(), &report.records()).unwrap();
            report
        }

        /// The placement of the fetched list, to call [`place`] with directly.
        fn placement(&self, list: &str) -> Placement {
            let list = crate::thornlist::parse(list).unwrap();
            let mut progress = prodash::tree::Root::new().add_child("plan");
            super::super::plan(&list, &self.install(), &mut progress)
                .unwrap()
                .libraries
                .remove(0)
        }

        fn place(&self, p: &Placement) -> Res<LinkOutcome> {
            place(
                &ClonesDone::after_the_pool(),
                &self.install(),
                "Cactus",
                p,
                &BTreeSet::new(),
                &BTreeSet::new(),
            )
        }
    }

    fn outcome<'r>(report: &'r super::super::ExecReport, label: &str) -> &'r LinkOutcome {
        &report
            .links
            .iter()
            .find(|(l, _)| l == label)
            .unwrap_or_else(|| panic!("no {label}: {report:?}"))
            .1
    }

    /// `cactup add et/kadaththorn`'s tree: FUKA at `repos/KadathThorn/src/fuka`
    /// (a link to its own clone, pinned), though its section comes first and
    /// both repos are cloned by the same parallel fetch; KadathThorn's git
    /// ignores it; and a second run changes nothing.
    #[test]
    fn a_library_lands_inside_its_dependent_and_a_rerun_changes_nothing() {
        let s = Setup::new();
        let k = commit_files(&s.kadath, &[("interface.ccl", "kadath\n"), ("src/build.sh", "cd fuka\n")]);
        let f = commit_files(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n"), ("src/kadath.cpp", "x\n")]);
        let list = s.list(&k, &f, "");
        let report = s.fetch(&list);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(outcome(&report, "KadathThorn/src/fuka"), &LinkOutcome::Created);

        let link = s.repos().join("KadathThorn/src/fuka");
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("../../fuka"));
        assert_eq!(std::fs::read_to_string(link.join("Cmake/CMakeLists.txt")).unwrap(), "fuka\n");
        assert_eq!(
            crate::fetch::git::head_of(&s.repos().join("fuka")).unwrap().1,
            f,
            "the library at its pin"
        );
        // Through the arrangement link too, as the build sees it.
        let built = s.install().join("Cactus/arrangements/Fuka/KadathThorn/src/fuka/src/kadath.cpp");
        assert_eq!(std::fs::read_to_string(built).unwrap(), "x\n");
        let exclude = std::fs::read_to_string(s.repos().join("KadathThorn/.git/info/exclude")).unwrap();
        assert!(exclude.lines().any(|l| l == "/src/fuka"), "{exclude}");
        let diff = crate::fetch::git::source_diff(&s.repos().join("KadathThorn")).unwrap();
        assert!(
            diff.untracked.is_empty() && diff.modified.is_empty(),
            "the dependent stays clean: {diff:?}"
        );

        let again = s.fetch(&list);
        assert!(again.failures.is_empty(), "{:?}", again.failures);
        assert_eq!(outcome(&again, "KadathThorn/src/fuka"), &LinkOutcome::Unchanged);
        let exclude2 = std::fs::read_to_string(s.repos().join("KadathThorn/.git/info/exclude")).unwrap();
        assert_eq!(exclude, exclude2, "the exclude is written once");
    }

    /// The pool clones four repos at a time, in the plan's order; here the
    /// dependent is queued behind six others and its library ahead of it.
    /// The library still lands inside the dependent's finished checkout: it
    /// is placed only once the pool is joined (`ClonesDone`), and placing it
    /// checks that the dependent is checked out, at its pin.
    #[test]
    fn a_parallel_fetch_places_the_library_after_its_dependent_is_cloned() {
        let s = Setup::new();
        let k = commit_files(&s.kadath, &[("src/build.sh", "x\n")]);
        let f = commit_files(&s.fuka, &[("src/a.cpp", "x\n")]);
        let mut extra = String::new();
        for name in ["A1", "A2", "A3", "A4", "A5", "A6"] {
            let dir = s.tmp.path().join("up").join(name);
            testrepo::init(&dir);
            let c = commit_files(&dir, &[("Thorn/interface.ccl", "x\n")]);
            extra.push_str(&format!(
                "!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {}\n!REPO_COMMIT = {c}\n!CHECKOUT =\n{name}/Thorn\n\n",
                dir.display()
            ));
        }
        let list = crate::thornlist::parse(&s.list(&k, &f, &extra)).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = super::super::plan(&list, &s.install(), &mut progress).unwrap();
        let queue: Vec<&str> = plan.git.iter().map(|g| g.repo.as_str()).collect();
        let behind = queue.iter().position(|r| *r == "KadathThorn").unwrap();
        assert!(behind >= super::super::WORKERS, "the dependent waits for a free worker: {queue:?}");
        let report = super::super::execute(&plan, &s.install()).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(outcome(&report, "KadathThorn/src/fuka"), &LinkOutcome::Created);
        assert!(s.repos().join("KadathThorn/src/fuka/src/a.cpp").is_file());
    }

    /// The dependent moves to a new commit (one that also drops a file next
    /// to the library): its checkout neither fails on the library nor
    /// deletes it, and the library is still there after. Then the library
    /// moves to a new pin of its own: its clone follows, the link stays.
    #[test]
    fn the_library_survives_its_dependent_moving_and_follows_its_own_pin() {
        let s = Setup::new();
        let k1 = commit_files(
            &s.kadath,
            &[("interface.ccl", "1\n"), ("src/build.sh", "1\n"), ("src/old.txt", "x\n")],
        );
        let f1 = commit_files(&s.fuka, &[("Cmake/CMakeLists.txt", "f1\n")]);
        let report = s.fetch(&s.list(&k1, &f1, ""));
        assert!(report.failures.is_empty(), "{:?}", report.failures);

        let k2 = commit_files(&s.kadath, &[("interface.ccl", "2\n"), ("src/build.sh", "2\n")]);
        let report = s.fetch(&s.list(&k2, &f1, ""));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let kadath = s.repos().join("KadathThorn");
        assert_eq!(crate::fetch::git::head_of(&kadath).unwrap().1, k2);
        assert!(!kadath.join("src/old.txt").exists(), "the dependent's own checkout moved");
        assert_eq!(outcome(&report, "KadathThorn/src/fuka"), &LinkOutcome::Unchanged);
        assert_eq!(
            std::fs::read_to_string(kadath.join("src/fuka/Cmake/CMakeLists.txt")).unwrap(),
            "f1\n"
        );

        let f2 = commit_files(&s.fuka, &[("Cmake/CMakeLists.txt", "f2\n")]);
        let report = s.fetch(&s.list(&k2, &f2, ""));
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(
            std::fs::read_to_string(kadath.join("src/fuka/Cmake/CMakeLists.txt")).unwrap(),
            "f2\n"
        );
    }

    /// A library inside a library, two levels deep, listed innermost first:
    /// each is placed after the clone it goes into, inside that clone.
    #[test]
    fn a_chain_of_libraries_is_placed_in_order() {
        let s = Setup::new();
        let eigen_up = s.tmp.path().join("up/eigen");
        let blas_up = s.tmp.path().join("up/blas");
        testrepo::init(&eigen_up);
        testrepo::init(&blas_up);
        let k = commit_files(&s.kadath, &[("src/build.sh", "x\n")]);
        let f = commit_files(&s.fuka, &[("src/a.cpp", "x\n")]);
        let e = commit_files(&eigen_up, &[("Eigen/Core", "eigen\n")]);
        let b = commit_files(&blas_up, &[("blas.f", "blas\n")]);
        let extra = format!(
            "!TARGET = $ROOT/repos/eigen/deps\n!TYPE = git\n!URL = {}\n!REPO_COMMIT = {b}\n!NAME = blas\n!CHECKOUT = .\n\n\
             !TARGET = $ROOT/repos/fuka/external\n!TYPE = git\n!URL = {}\n!REPO_COMMIT = {e}\n!NAME = eigen\n!CHECKOUT = .\n\n",
            blas_up.display(),
            eigen_up.display()
        );
        let list = s.list(&k, &f, &extra);
        let parsed = crate::thornlist::parse(&list).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = super::super::plan(&parsed, &s.install(), &mut progress).unwrap();
        let order: Vec<String> = plan.libraries.iter().map(Placement::label).collect();
        assert_eq!(order, ["KadathThorn/src/fuka", "fuka/external/eigen", "eigen/deps/blas"]);

        let report = super::super::execute(&plan, &s.install()).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let deepest = s.repos().join("KadathThorn/src/fuka/external/eigen/deps/blas/blas.f");
        assert_eq!(std::fs::read_to_string(deepest).unwrap(), "blas\n");
        let exclude = std::fs::read_to_string(s.repos().join("fuka/.git/info/exclude")).unwrap();
        assert!(exclude.lines().any(|l| l == "/external/eigen"), "{exclude}");
    }

    /// A cycle of libraries never reaches the disk: the plan refuses it.
    #[test]
    fn a_cycle_is_refused_by_the_plan() {
        let s = Setup::new();
        let k = commit_files(&s.kadath, &[("src/build.sh", "x\n")]);
        let f = commit_files(&s.fuka, &[("src/a.cpp", "x\n")]);
        let other = s.tmp.path().join("up/other");
        let extra = format!(
            "!TARGET = $ROOT/repos/fuka/x\n!TYPE = git\n!URL = {}\n!NAME = other\n!CHECKOUT = .\n\n\
             !TARGET = $ROOT/repos/other/y\n!TYPE = git\n!URL = {}\n!REPO_COMMIT = {f}\n!NAME = fuka\n!CHECKOUT = .\n\n",
            other.display(),
            s.fuka.display()
        );
        let list = crate::thornlist::parse(&s.list(&k, &f, &extra)).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let err = super::super::plan(&list, &s.install(), &mut progress).unwrap_err();
        assert!(format!("{err:#}").contains("cycle"), "{err:#}");
        assert!(!s.install().exists(), "nothing written");
    }

    /// The dependent's clone fails: the library is not attempted, and the
    /// failure says why; nothing is created in the dependent's place.
    #[test]
    fn a_dependent_that_fails_to_clone_leaves_its_library_unplaced() {
        let s = Setup::new();
        let k = commit_files(&s.kadath, &[("src/build.sh", "x\n")]);
        let f = commit_files(&s.fuka, &[("src/a.cpp", "x\n")]);
        // Same name, nothing there: its clone fails.
        let gone = s.tmp.path().join("up/gone/KadathThorn");
        let list = s
            .list(&k, &f, "")
            .replace(&s.kadath.display().to_string(), &gone.display().to_string());
        let report = s.fetch(&list);
        let library = report
            .failures
            .iter()
            .find(|x| x.what == "KadathThorn/src/fuka")
            .expect("the library fails too");
        assert!(
            library.error.contains("KadathThorn, which it goes inside, did not fetch"),
            "{}",
            library.error
        );
        assert!(!s.repos().join("KadathThorn").exists(), "no dependent directory made for it");
    }

    /// Each refusal names the path and changes nothing: `..` and absolute
    /// `into`s, a symlinked directory on the way (pointing outside the
    /// checkout), something already at the path (a directory, a foreign
    /// link), a missing dependent checkout, and one from another remote.
    #[test]
    fn every_refusal_names_the_path_and_touches_nothing() {
        let s = Setup::new();
        let k = commit_files(&s.kadath, &[("src/build.sh", "x\n")]);
        let f = commit_files(&s.fuka, &[("src/a.cpp", "x\n")]);
        let list = s.list(&k, &f, "");
        assert!(s.fetch(&list).failures.is_empty());
        let p = s.placement(&list);
        let kadath = s.repos().join("KadathThorn");
        let link = kadath.join("src/fuka");
        std::fs::remove_file(&link).unwrap();
        let err = |p: &Placement| format!("{:#}", s.place(p).unwrap_err());

        for (into, says) in [
            ("../fuka", "not a plain folder name"),
            ("/tmp/fuka", "absolute"),
            ("src/../../fuka", "not a plain folder name"),
        ] {
            let e = err(&Placement { into: into.into(), ..p.clone() });
            assert!(e.contains(says), "{into}: {e}");
        }

        // A symlink on the way, to a directory outside the checkout.
        let outside = s.tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::os::unix::fs::symlink(&outside, kadath.join("ext")).unwrap();
        let e = err(&Placement { into: "ext/fuka".into(), ..p.clone() });
        assert!(
            e.contains("is a symlink") && e.contains(&kadath.join("ext").display().to_string()),
            "{e}"
        );
        assert_eq!(std::fs::read_dir(&outside).unwrap().count(), 0, "nothing written outside");

        // An unrelated directory already at the path.
        std::fs::create_dir(&link).unwrap();
        std::fs::write(link.join("mine.txt"), "mine\n").unwrap();
        let e = err(&p);
        assert!(e.contains("already a directory") && e.contains(&link.display().to_string()), "{e}");
        assert_eq!(std::fs::read_to_string(link.join("mine.txt")).unwrap(), "mine\n");
        std::fs::remove_dir_all(&link).unwrap();

        // A link cactup did not make.
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let e = err(&p);
        assert!(e.contains("cactup did not make"), "{e}");
        assert_eq!(std::fs::read_link(&link).unwrap(), outside);
        std::fs::remove_file(&link).unwrap();

        // The dependent's checkout missing: refused, and not created.
        let aside = s.tmp.path().join("aside");
        std::fs::rename(&kadath, &aside).unwrap();
        let e = err(&p);
        assert!(e.contains("not checked out") && e.contains("never creating it"), "{e}");
        assert!(!kadath.exists());
        std::fs::rename(&aside, &kadath).unwrap();

        // A checkout of another repository in the dependent's place.
        let e = err(&Placement { dependent_url: "https://x/elsewhere.git".into(), ..p.clone() });
        assert!(e.contains("not of KadathThorn's repository"), "{e}");
        // At another commit than the pin.
        let e = err(&Placement { dependent_commit: Some("0".repeat(40)), ..p.clone() });
        assert!(e.contains("the commit the thornlist pins"), "{e}");

        // Nothing above left anything behind, and the real one still places.
        assert!(!link.exists() && std::fs::symlink_metadata(&link).is_err());
        assert_eq!(s.place(&p).unwrap(), LinkOutcome::Created);
    }

    /// A path the dependent tracks files at is the dependent's own.
    #[test]
    fn a_path_the_dependent_tracks_is_refused() {
        let s = Setup::new();
        let k = commit_files(&s.kadath, &[("src/build.sh", "x\n"), ("src/fuka/README", "bundled\n")]);
        let f = commit_files(&s.fuka, &[("src/a.cpp", "x\n")]);
        let report = s.fetch(&s.list(&k, &f, ""));
        let failure = report
            .failures
            .iter()
            .find(|x| x.what == "KadathThorn/src/fuka")
            .expect("refused");
        assert!(failure.error.contains("tracks 'src/fuka/README'"), "{}", failure.error);
        let readme = s.repos().join("KadathThorn/src/fuka/README");
        assert_eq!(
            std::fs::read_to_string(readme).unwrap(),
            "bundled\n",
            "the dependent's file is untouched"
        );
    }

    // From the review of the first version: each of these reproduced a
    // problem (or checked one was not there), and stays as a regression test.

    /// Commit nested entries `(path, content, is_link)` on HEAD, objects
    /// only (no worktree): an upstream for the fetch.
    fn commit_entries(dir: &Path, files: &[(&str, &str, bool)]) -> gix::ObjectId {
        #[derive(Default)]
        struct D<'a> {
            subdirs: BTreeMap<&'a str, D<'a>>,
            files: Vec<(&'a str, &'a str, bool)>,
        }
        fn write(repo: &gix::Repository, d: &D) -> gix::ObjectId {
            use gix::objs::tree::{Entry, EntryKind};
            let mut tree = gix::objs::Tree::empty();
            for (name, sub) in &d.subdirs {
                let oid = write(repo, sub);
                tree.entries
                    .push(Entry { mode: EntryKind::Tree.into(), filename: (*name).into(), oid });
            }
            for (name, content, link) in &d.files {
                let oid = repo.write_blob(content.as_bytes()).unwrap().detach();
                let kind = if *link { EntryKind::Link } else { EntryKind::Blob };
                tree.entries.push(Entry { mode: kind.into(), filename: (*name).into(), oid });
            }
            tree.entries.sort();
            repo.write_object(&tree).unwrap().detach()
        }
        let repo = gix::open(dir).expect("open repo");
        let mut root = D::default();
        for (path, content, link) in files {
            let mut node = &mut root;
            let mut parts = path.split('/').peekable();
            while let Some(part) = parts.next() {
                if parts.peek().is_some() {
                    node = node.subdirs.entry(part).or_default();
                } else {
                    node.files.push((part, content, *link));
                }
            }
        }
        let tree = write(&repo, &root);
        let parents: Vec<gix::ObjectId> = repo
            .head()
            .ok()
            .and_then(|mut h| h.peel_to_commit().ok())
            .map(|c| vec![c.id])
            .unwrap_or_default();
        repo.commit("HEAD", "c", tree, parents).unwrap().detach()
    }

    /// Everything under `dir` but `.git`: directories, files with their
    /// content, links with their text.
    fn snapshot(dir: &Path) -> Vec<String> {
        fn go(base: &Path, d: &Path, out: &mut Vec<String>) {
            let Ok(entries) = std::fs::read_dir(d) else { return };
            for e in entries {
                let p = e.unwrap().path();
                let m = std::fs::symlink_metadata(&p).unwrap();
                let rel = p.strip_prefix(base).unwrap().display().to_string();
                if rel.starts_with(".git") {
                    continue;
                }
                if m.file_type().is_symlink() {
                    out.push(format!("{rel} -> {}", std::fs::read_link(&p).unwrap().display()));
                } else if m.is_dir() {
                    out.push(format!("{rel}/"));
                    go(base, &p, out);
                } else {
                    out.push(format!("{rel} = {}", std::fs::read_to_string(&p).unwrap_or_default()));
                }
            }
        }
        let mut out = Vec::new();
        go(dir, dir, &mut out);
        out.sort();
        out
    }

    /// The Einstein Toolkit's own FUKA section, at `f`, and KadathThorn at `k`:
    /// FUKA's items linked through
    /// `$ARR/Fuka/KadathThorn/src/fuka/../../../../repos/KadathThorn/src/fuka`.
    fn et_list(s: &Setup, k: &gix::ObjectId, f: &gix::ObjectId) -> String {
        format!(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!DEFINE ARR = $ROOT/arrangements\n\n\
             !TARGET = $ARR/Fuka/KadathThorn/src/fuka/../../../../repos/KadathThorn/src/fuka\n!TYPE = git\n!URL = {}\n\
             !REPO_COMMIT = {f}\n!CHECKOUT = Cmake src\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {}\n!REPO_PATH = ../$2\n!REPO_COMMIT = {k}\n\
             !CHECKOUT =\nFuka/KadathThorn\n",
            s.fuka.display(),
            s.kadath.display()
        )
    }

    /// Finding 1, defense in depth: a placed library's link left behind, then
    /// a list carrying FUKA the Einstein Toolkit's way. Resolved through the
    /// stale link, that `!TARGET` climbs out of the installation; the link
    /// pass refuses rather than writing there.
    #[test]
    fn review_et_style_base_after_a_placed_library() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(
            &s.fuka,
            &[("Cmake/CMakeLists.txt", "fuka\n", false), ("src/a.cpp", "a\n", false)],
        );
        assert!(s.fetch(&s.list(&k, &f, "")).failures.is_empty());
        let report = s.fetch(&et_list(&s, &k, &f));
        assert!(
            !s.tmp.path().join("repos").exists(),
            "wrote outside the installation: {:?}",
            snapshot(s.tmp.path())
        );
        let refused: Vec<&str> = report.failures.iter().map(|f| f.error.as_str()).collect();
        assert!(refused.iter().any(|e| e.contains("outside the installation")), "{refused:?}");
    }

    /// Finding 1: a placement the next list drops (the base took the library
    /// over, the package went, or `into` changed) loses its link and exclude
    /// line first, and then the Einstein Toolkit's own FUKA section links
    /// into a real directory there. Only cactup's own link goes: anything
    /// else at the path is left, with a note, and nothing is refused.
    #[test]
    fn a_dropped_placement_takes_its_own_link_away_and_nothing_else() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(
            &s.fuka,
            &[("Cmake/CMakeLists.txt", "fuka\n", false), ("src/a.cpp", "a\n", false)],
        );
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let et = et_list(&s, &k, &f);
        let link = s.repos().join("KadathThorn/src/fuka");
        let exclude = s.repos().join("KadathThorn/.git/info/exclude");

        // Not this list's: nothing to drop.
        assert!(unplace_dropped(&s.install(), &placed, &placed).unwrap().dropped.is_empty());
        let unplacing = unplace_dropped(&s.install(), &placed, &et).unwrap();
        assert_eq!(unplacing.links(), ["KadathThorn/src/fuka"]);
        assert!(unplacing.notes.is_empty(), "{:?}", unplacing.notes);
        assert!(std::fs::symlink_metadata(&link).is_err());
        assert!(!std::fs::read_to_string(&exclude).unwrap().contains("/src/fuka"));
        let report = s.fetch(&et);
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert!(std::fs::symlink_metadata(&link).unwrap().is_dir(), "the ET's own real directory now");
        assert_eq!(std::fs::read_to_string(link.join("Cmake/CMakeLists.txt")).unwrap(), "fuka\n");
        // Dropped again with a real directory there now: left, with a note.
        let unplacing = unplace_dropped(&s.install(), &placed, &et).unwrap();
        assert!(unplacing.links().is_empty());
        assert!(unplacing.notes.iter().any(|n| n.contains("left as it is")), "{:?}", unplacing.notes);
        assert!(link.join("Cmake").exists(), "and left as it is");

        // `into` changed: the old link goes, the new one is made.
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("src/a.cpp", "a\n", false)]);
        let old = s.list(&k, &f, "");
        assert!(s.fetch(&old).failures.is_empty());
        let new = old.replace("$ROOT/repos/KadathThorn/src\n", "$ROOT/repos/KadathThorn/ext\n");
        assert_eq!(unplace_dropped(&s.install(), &old, &new).unwrap().links(), ["KadathThorn/src/fuka"]);
        assert!(s.fetch(&new).failures.is_empty());
        assert!(s.repos().join("KadathThorn/ext/fuka/src/a.cpp").is_file());
        assert!(std::fs::symlink_metadata(s.repos().join("KadathThorn/src/fuka")).is_err());

        // A link cactup did not make, at the dropped path: kept, with a note.
        let elsewhere = s.tmp.path().join("elsewhere");
        std::fs::create_dir(&elsewhere).unwrap();
        let theirs = s.repos().join("KadathThorn/ext/fuka");
        std::fs::remove_file(&theirs).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &theirs).unwrap();
        let unplacing = unplace_dropped(&s.install(), &new, &old).unwrap();
        assert!(unplacing.links().is_empty());
        assert!(
            unplacing
                .notes
                .iter()
                .any(|n| n.contains("cactup did not make") && n.contains("ext/fuka")),
            "{:?}",
            unplacing.notes
        );
        assert_eq!(std::fs::read_link(&theirs).unwrap(), elsewhere);
    }

    /// Finding 2: the dependent tracks `src` as a submodule (a gitlink); a
    /// library at `src/fuka` would sit inside the submodule's tree.
    #[test]
    fn review_into_inside_a_tracked_gitlink() {
        let s = Setup::new();
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let repo = gix::open(&s.kadath).unwrap();
        let blob = repo.write_blob(b"x\n").unwrap().detach();
        let mut tree = gix::objs::Tree::empty();
        use gix::objs::tree::{Entry, EntryKind};
        tree.entries.push(Entry {
            mode: EntryKind::Blob.into(),
            filename: "build.sh".into(),
            oid: blob,
        });
        tree.entries
            .push(Entry { mode: EntryKind::Commit.into(), filename: "src".into(), oid: f });
        tree.entries.sort();
        let t = repo.write_object(&tree).unwrap().detach();
        let k = repo.commit("HEAD", "c", t, Vec::<gix::ObjectId>::new()).unwrap().detach();
        let report = s.fetch(&s.list(&k, &f, ""));
        let failure = report
            .failures
            .iter()
            .find(|x| x.what == "KadathThorn/src/fuka")
            .expect("refused");
        assert!(failure.error.contains("tracks 'src'"), "{}", failure.error);
        assert!(
            std::fs::symlink_metadata(s.repos().join("KadathThorn/src/fuka")).is_err(),
            "placed inside a tracked submodule path"
        );
    }

    /// Finding 3: with no index, what the dependent tracks cannot be told:
    /// refused (here HEAD tracks `src/fuka/README`, deleted on disk).
    #[test]
    fn review_tracked_check_uses_index_only() {
        let s = Setup::new();
        let k = commit_entries(
            &s.kadath,
            &[("src/build.sh", "1\n", false), ("src/fuka/README", "bundled\n", false)],
        );
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let list = s.list(&k, &f, "");
        let _ = s.fetch(&list);
        let kadath = s.repos().join("KadathThorn");
        std::fs::remove_dir_all(kadath.join("src/fuka")).unwrap();
        std::fs::remove_file(kadath.join(".git/index")).unwrap();
        let p = s.placement(&list);
        let r = s.place(&p);
        assert!(r.is_err(), "placed over a path HEAD tracks");
        assert!(format!("{:#}", r.unwrap_err()).contains("no readable index"));
        assert!(std::fs::symlink_metadata(kadath.join("src/fuka")).is_err());
    }

    /// Finding 3: an index that no longer lists the path, while HEAD still
    /// tracks it: HEAD's tree is checked too.
    #[test]
    fn a_path_only_head_tracks_is_refused() {
        let s = Setup::new();
        let k = commit_entries(
            &s.kadath,
            &[("src/build.sh", "1\n", false), ("src/fuka/README", "bundled\n", false)],
        );
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let list = s.list(&k, &f, "");
        let _ = s.fetch(&list);
        let kadath = s.repos().join("KadathThorn");
        std::fs::remove_dir_all(kadath.join("src/fuka")).unwrap();
        // An index of just `src/build.sh`.
        let repo = gix::open(&kadath).unwrap();
        let blob = repo.write_blob(b"1\n").unwrap().detach();
        let mut src = gix::objs::Tree::empty();
        src.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: "build.sh".into(),
            oid: blob,
        });
        let src = repo.write_object(&src).unwrap().detach();
        let mut top = gix::objs::Tree::empty();
        top.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Tree.into(),
            filename: "src".into(),
            oid: src,
        });
        let top = repo.write_object(&top).unwrap().detach();
        let mut index = repo.index_from_tree(&top).unwrap();
        index.write(Default::default()).unwrap();
        let p = s.placement(&list);
        let err = format!("{:#}", s.place(&p).unwrap_err());
        assert!(err.contains("tracks 'src/fuka'"), "{err}");
        assert!(std::fs::symlink_metadata(kadath.join("src/fuka")).is_err());
    }

    /// Finding 4: appending the exclude fails (its directory is read-only):
    /// the link, and the folder made on the way to it, are taken back.
    #[test]
    fn a_failed_exclude_leaves_nothing_behind() {
        use std::os::unix::fs::PermissionsExt;
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("src/a.cpp", "a\n", false)]);
        let list = s.list(&k, &f, "");
        assert!(s.fetch(&list).failures.is_empty());
        let kadath = s.repos().join("KadathThorn");
        std::fs::remove_file(kadath.join("src/fuka")).unwrap();
        let info = kadath.join(".git/info");
        std::fs::remove_file(info.join("exclude")).unwrap();
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(info.join("probe"), "").is_ok() {
            // Running as root: permissions do not bind, nothing to show.
            std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let p = Placement { into: "ext/fuka".into(), ..s.placement(&list) };
        let err = format!("{:#}", s.place(&p).unwrap_err());
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(err.contains("nothing was left behind"), "{err}");
        assert!(std::fs::symlink_metadata(kadath.join("ext")).is_err(), "{:?}", snapshot(&kadath));
    }

    /// Finding 5: a `.git` that is no plain directory (a `gitdir:` file, as
    /// a worktree or submodule has, or a symlink) is never written through:
    /// refused before anything is made.
    #[test]
    fn a_git_that_is_no_plain_directory_is_refused() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("src/a.cpp", "a\n", false)]);
        let list = s.list(&k, &f, "");
        assert!(s.fetch(&list).failures.is_empty());
        let p = s.placement(&list);
        let kadath = s.repos().join("KadathThorn");
        std::fs::remove_file(kadath.join("src/fuka")).unwrap();
        let real = s.tmp.path().join("real-git");
        std::fs::rename(kadath.join(".git"), &real).unwrap();
        let exclude_before = std::fs::read_to_string(real.join("info/exclude")).unwrap();

        std::fs::write(kadath.join(".git"), format!("gitdir: {}\n", real.display())).unwrap();
        let err = format!("{:#}", s.place(&p).unwrap_err());
        assert!(err.contains("not the repository's own directory"), "{err}");
        assert!(std::fs::symlink_metadata(kadath.join("src/fuka")).is_err());

        std::fs::remove_file(kadath.join(".git")).unwrap();
        std::os::unix::fs::symlink(&real, kadath.join(".git")).unwrap();
        let err = format!("{:#}", s.place(&p).unwrap_err());
        assert!(err.contains("symlink"), "{err}");
        assert!(std::fs::symlink_metadata(kadath.join("src/fuka")).is_err());
        assert_eq!(std::fs::read_to_string(real.join("info/exclude")).unwrap(), exclude_before);
    }

    /// Finding 5: `.git/info/exclude` a symlink: refused, and the file it
    /// points at is untouched.
    #[test]
    fn review_exclude_through_symlink() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let list = s.list(&k, &f, "");
        assert!(s.fetch(&list).failures.is_empty());
        let p = s.placement(&list);
        let kadath = s.repos().join("KadathThorn");
        std::fs::remove_file(kadath.join("src/fuka")).unwrap();
        let victim = s.tmp.path().join("victim");
        std::fs::write(&victim, "precious\n").unwrap();
        std::fs::remove_file(kadath.join(".git/info/exclude")).unwrap();
        std::os::unix::fs::symlink(&victim, kadath.join(".git/info/exclude")).unwrap();
        assert!(s.place(&p).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious\n");
        assert!(std::fs::symlink_metadata(kadath.join("src/fuka")).is_err(), "nothing written");
    }

    /// Finding 5: the exclude is appended to, never rewritten: what was in
    /// it stays byte for byte, a last line without a newline included.
    #[test]
    fn the_exclude_is_appended_to() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("src/a.cpp", "a\n", false)]);
        let list = s.list(&k, &f, "");
        assert!(s.fetch(&list).failures.is_empty());
        let p = s.placement(&list);
        let kadath = s.repos().join("KadathThorn");
        std::fs::remove_file(kadath.join("src/fuka")).unwrap();
        std::fs::write(kadath.join(".git/info/exclude"), "*.o\nbuild").unwrap();
        assert_eq!(s.place(&p).unwrap(), LinkOutcome::Created);
        let text = std::fs::read_to_string(kadath.join(".git/info/exclude")).unwrap();
        assert_eq!(text, format!("*.o\nbuild\n{EXCLUDE_COMMENT}\n/src/fuka\n"));
    }

    /// Finding 6: the way swapped for a symlink to outside the checkout after
    /// every check and before the write. The write goes by the descriptors
    /// already held, so the link lands in the directory that was checked
    /// (now renamed), and nothing outside is touched.
    #[test]
    fn a_way_swapped_after_the_checks_is_not_followed() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("src/a.cpp", "a\n", false)]);
        let list = s.list(&k, &f, "");
        assert!(s.fetch(&list).failures.is_empty());
        let p = s.placement(&list);
        let kadath = s.repos().join("KadathThorn");
        std::fs::remove_file(kadath.join("src/fuka")).unwrap();
        let outside = s.tmp.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let swap = || {
            std::fs::rename(kadath.join("src"), kadath.join("src.checked")).unwrap();
            std::os::unix::fs::symlink(&outside, kadath.join("src")).unwrap();
        };
        let outcome =
            place_between(&s.install(), "Cactus", &p, &BTreeSet::new(), &BTreeSet::new(), &swap)
                .unwrap();
        assert_eq!(outcome, LinkOutcome::Created);
        assert!(snapshot(&outside).is_empty(), "nothing outside: {:?}", snapshot(&outside));
        assert_eq!(
            std::fs::read_link(kadath.join("src.checked/fuka")).unwrap(),
            Path::new("../../fuka")
        );
    }

    /// Finding 7: on a case-insensitive filesystem `src/FUKA` and `src/fuka`
    /// are one path, and `FUKA` and `fuka` one clone.
    #[test]
    fn names_differing_only_in_case_collide() {
        let err = order(vec![
            placement("fuka", "KadathThorn", "src/fuka"),
            placement("FUKA", "KadathThorn", "src/FUKA"),
        ])
        .unwrap_err();
        assert!(format!("{err:#}").contains("Two libraries go to"), "{err:#}");
        let list = crate::thornlist::parse(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/fuka\n!CHECKOUT = A/B\n\n\
             !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://y/FUKA\n!CHECKOUT = C/D\n",
        )
        .unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let err = super::super::plan(&list, tmp.path(), &mut progress).unwrap_err();
        assert!(format!("{err:#}").contains("differ only in case"), "{err:#}");
    }

    /// Finding 9: no part of `into`, so no clone name, may start with `-`.
    #[test]
    fn a_name_starting_with_a_dash_is_refused() {
        for bad in ["-rf", "src/-fuka", "-/x"] {
            assert!(check_into(bad).is_err(), "{bad}");
        }
        check_into("src/fu-ka").unwrap();
    }

    /// The dependent moves to a commit tracking files under the link's path:
    /// its checkout never writes through the link into the library's clone.
    #[test]
    fn review_dependent_upgrade_tracking_under_the_link() {
        let s = Setup::new();
        let k1 = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        assert!(s.fetch(&s.list(&k1, &f, "")).failures.is_empty());
        let before = snapshot(&s.repos().join("fuka"));
        let k2 = commit_entries(
            &s.kadath,
            &[
                ("src/build.sh", "2\n", false),
                ("src/fuka/evil.txt", "EVIL\n", false),
                ("src/fuka/Cmake/CMakeLists.txt", "CLOBBERED\n", false),
            ],
        );
        let list = crate::thornlist::parse(&s.list(&k2, &f, "")).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = super::super::plan(&list, &s.install(), &mut progress).unwrap();
        let _ = super::super::execute(&plan, &s.install()).unwrap();
        assert_eq!(
            before,
            snapshot(&s.repos().join("fuka")),
            "library clone changed by the dependent's checkout"
        );
    }

    /// The dependent's new commit tracks `src/fuka` as a file.
    #[test]
    fn review_dependent_upgrade_tracking_the_link_path_as_file() {
        let s = Setup::new();
        let k1 = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        assert!(s.fetch(&s.list(&k1, &f, "")).failures.is_empty());
        let before = snapshot(&s.repos().join("fuka"));
        let k2 = commit_entries(
            &s.kadath,
            &[("src/build.sh", "2\n", false), ("src/fuka", "a file\n", false)],
        );
        let list = crate::thornlist::parse(&s.list(&k2, &f, "")).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = super::super::plan(&list, &s.install(), &mut progress).unwrap();
        let _ = super::super::execute(&plan, &s.install()).unwrap();
        assert_eq!(before, snapshot(&s.repos().join("fuka")));
    }

    /// The dependent's new commit makes `src` a tracked symlink to outside:
    /// nothing lands there.
    #[test]
    fn review_dependent_upgrade_src_becomes_symlink_outside() {
        let s = Setup::new();
        let outside = s.tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("precious"), "keep\n").unwrap();
        let k1 = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        assert!(s.fetch(&s.list(&k1, &f, "")).failures.is_empty());
        let k2 = commit_entries(&s.kadath, &[("src", &outside.display().to_string(), true)]);
        let list = crate::thornlist::parse(&s.list(&k2, &f, "")).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = super::super::plan(&list, &s.install(), &mut progress).unwrap();
        let _ = super::super::execute(&plan, &s.install()).unwrap();
        assert_eq!(snapshot(&outside), ["precious = keep\n"]);
    }

    /// A symlink planted at `repos/<dependent>`: refused, nothing written
    /// where it points.
    #[test]
    fn review_symlinked_clones_in_repos() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let list = s.list(&k, &f, "");
        assert!(s.fetch(&list).failures.is_empty());
        let p = s.placement(&list);
        let kadath = s.repos().join("KadathThorn");
        std::fs::remove_file(kadath.join("src/fuka")).unwrap();
        let real = s.tmp.path().join("realkadath");
        std::fs::rename(&kadath, &real).unwrap();
        std::os::unix::fs::symlink(&real, &kadath).unwrap();
        assert!(s.place(&p).is_err());
        assert!(std::fs::symlink_metadata(real.join("src/fuka")).is_err());
    }

    /// A whole-repo section's clone is its `!NAME`.
    #[test]
    fn review_repo_of_a_named_whole_repo_section() {
        let list = crate::thornlist::parse(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\
             !TARGET = $ROOT/repos/KadathThorn/src\n!TYPE = git\n!URL = https://x/fuka.git\n!NAME = libfuka\n!CHECKOUT = .\n",
        )
        .unwrap();
        assert_eq!(list.components()[0].repo, "libfuka");
    }

    // From the follow-up review.

    /// The list placing FUKA for KadathThorn at `k`, without the placement.
    fn without_fuka(s: &Setup, k: &gix::ObjectId) -> String {
        format!(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {}\n\
             !REPO_PATH = ../$2\n!REPO_COMMIT = {k}\n!CHECKOUT =\nFuka/KadathThorn\n",
            s.kadath.display()
        )
    }

    /// Item 3: the dependent moved to a commit that tracks `src/fuka` (its
    /// checkout replaced cactup's link), or that makes `src` a link. Dropping
    /// the placement then takes nothing away and refuses nothing: those are
    /// the dependent's own. It says so in a note.
    #[test]
    fn review2_unplace_blocks_when_the_dependent_now_tracks_the_path() {
        let s = Setup::new();
        let k1 = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        assert!(s.fetch(&s.list(&k1, &f, "")).failures.is_empty());
        let k2 = commit_entries(
            &s.kadath,
            &[("src/build.sh", "2\n", false), ("src/fuka/README", "bundled\n", false)],
        );
        let v2 = s.list(&k2, &f, "");
        let _ = s.fetch(&v2); // the placement is refused (tracked); the dependent is now at k2
        let readme = s.repos().join("KadathThorn/src/fuka/README");
        assert!(readme.is_file());
        let unplacing = unplace_dropped(&s.install(), &v2, &without_fuka(&s, &k2)).unwrap();
        assert!(unplacing.links().is_empty());
        assert!(unplacing.notes.iter().any(|n| n.contains("left as it is")), "{:?}", unplacing.notes);
        assert_eq!(
            std::fs::read_to_string(&readme).unwrap(),
            "bundled\n",
            "nothing of the dependent's removed"
        );

        // `src` becomes a tracked link.
        let s = Setup::new();
        let k1 = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        assert!(s.fetch(&s.list(&k1, &f, "")).failures.is_empty());
        let k2 = commit_entries(&s.kadath, &[("src", "lib", true), ("lib/x", "x\n", false)]);
        let v2 = s.list(&k2, &f, "");
        let _ = s.fetch(&v2);
        let unplacing = unplace_dropped(&s.install(), &v2, &without_fuka(&s, &k2)).unwrap();
        assert!(unplacing.links().is_empty());
        assert!(unplacing.notes.iter().any(|n| n.contains("is a symlink")), "{:?}", unplacing.notes);
        assert!(s.repos().join("KadathThorn/lib/x").is_file());
        assert_eq!(std::fs::read_link(s.repos().join("KadathThorn/src")).unwrap(), Path::new("lib"));
    }

    /// Item 1: once the placement is dropped, its exclude line goes too, so
    /// what the user puts at that path later is untracked to git again, and
    /// to the probe that decides whether a pruned orphan needs a backup.
    #[test]
    fn review2_stale_exclude_hides_user_files_from_the_prune_probe() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let unplacing = unplace_dropped(&s.install(), &placed, &without_fuka(&s, &k)).unwrap();
        assert_eq!(unplacing.links(), ["KadathThorn/src/fuka"]);
        let mine = s.repos().join("KadathThorn/src/fuka");
        std::fs::create_dir(&mine).unwrap();
        std::fs::write(mine.join("my-work.cpp"), "weeks of work\n").unwrap();
        std::fs::write(s.repos().join("KadathThorn/src/other.cpp"), "x\n").unwrap();
        let probe = crate::fetch::git::probe(
            &s.repos().join("KadathThorn"),
            &s.kadath.display().to_string(),
            "",
        );
        // Listed (git names the new directory whole) rather than hidden.
        assert!(
            probe.untracked.iter().any(|u| u.starts_with("src/fuka")),
            "user file hidden: {:?}",
            probe.untracked
        );
        assert!(hidden_by_library_excludes(&s.repos().join("KadathThorn")).is_empty());
    }

    /// Item 1, defense in depth: a pair left in the exclude (by an older
    /// cactup) with the user's files at its path is named, so prune counts
    /// them; cactup's own link there is not.
    #[test]
    fn files_under_a_library_exclude_are_found() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        assert!(s.fetch(&s.list(&k, &f, "")).failures.is_empty());
        let kadath = s.repos().join("KadathThorn");
        assert!(hidden_by_library_excludes(&kadath).is_empty(), "the link itself hides nothing");
        std::fs::remove_file(kadath.join("src/fuka")).unwrap();
        std::fs::create_dir(kadath.join("src/fuka")).unwrap();
        std::fs::write(kadath.join("src/fuka/mine.cpp"), "x\n").unwrap();
        assert_eq!(hidden_by_library_excludes(&kadath), ["src/fuka"]);
    }

    /// Item 1: taking the pair away leaves every other line of the exclude
    /// byte for byte, and rewrites it in one rename (no temporary left).
    #[test]
    fn unplacing_takes_only_its_own_exclude_lines() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let info = s.repos().join("KadathThorn/.git/info");
        let text = std::fs::read_to_string(info.join("exclude")).unwrap();
        // The user's lines around cactup's pair, and a lone `/src/fuka` of
        // their own (no comment above it), which stays.
        let mine = format!("*.o\r\n{text}/src/fuka\n# mine\n");
        std::fs::write(info.join("exclude"), &mine).unwrap();
        unplace_dropped(&s.install(), &placed, &without_fuka(&s, &k)).unwrap();
        let after = std::fs::read_to_string(info.join("exclude")).unwrap();
        assert_eq!(after, mine.replace(&format!("{EXCLUDE_COMMENT}\n/src/fuka\n"), ""));
        assert!(after.contains("/src/fuka\n# mine"), "the user's own line stays");
        let names: Vec<String> = std::fs::read_dir(&info)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["exclude"], "no temporary left");
    }

    /// Item 2: what was taken away is put back for a command that failed
    /// after dropping placements.
    #[test]
    fn a_failed_command_gets_its_placements_back() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let exclude = s.repos().join("KadathThorn/.git/info/exclude");
        let before = std::fs::read_to_string(&exclude).unwrap();
        let unplacing = unplace_dropped(&s.install(), &placed, &without_fuka(&s, &k)).unwrap();
        assert!(std::fs::symlink_metadata(s.repos().join("KadathThorn/src/fuka")).is_err());
        assert!(unplacing.restore(&s.install()).is_empty());
        assert_eq!(
            std::fs::read_link(s.repos().join("KadathThorn/src/fuka")).unwrap(),
            Path::new("../../fuka")
        );
        assert_eq!(std::fs::read_to_string(&exclude).unwrap(), before);
    }

    /// Item 4: `repos/` or `arrangements/` symlinked out of the installation
    /// (to scratch space), or the installation reached through a symlink:
    /// all link as before. The escape through a stale link stays refused
    /// (`review_et_style_base_after_a_placed_library`).
    #[test]
    fn review2_link_refusal_layouts() {
        // The installation reached through a symlink.
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(
            &s.fuka,
            &[("Cmake/CMakeLists.txt", "fuka\n", false), ("src/a.cpp", "a\n", false)],
        );
        std::fs::create_dir_all(s.tmp.path().join("real")).unwrap();
        std::os::unix::fs::symlink(s.tmp.path().join("real"), s.install()).unwrap();
        let r = s.fetch(&et_list(&s, &k, &f));
        assert!(r.failures.is_empty(), "symlinked installation: {:?}", r.failures);

        // `repos/` symlinked out.
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(
            &s.fuka,
            &[("Cmake/CMakeLists.txt", "fuka\n", false), ("src/a.cpp", "a\n", false)],
        );
        let scratch = s.tmp.path().join("scratch/repos");
        std::fs::create_dir_all(&scratch).unwrap();
        std::fs::create_dir_all(s.install().join("Cactus")).unwrap();
        std::os::unix::fs::symlink(&scratch, s.repos()).unwrap();
        let r = s.fetch(&et_list(&s, &k, &f));
        assert!(r.failures.is_empty(), "repos symlinked out: {:?}", r.failures);
        assert!(scratch.join("KadathThorn/src/fuka/Cmake/CMakeLists.txt").is_file());

        // `arrangements/` symlinked out, with a placed library too.
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("src/a.cpp", "a\n", false)]);
        let arr = s.tmp.path().join("scratch/arrangements");
        std::fs::create_dir_all(&arr).unwrap();
        std::fs::create_dir_all(s.install().join("Cactus")).unwrap();
        std::os::unix::fs::symlink(&arr, s.install().join("Cactus/arrangements")).unwrap();
        let r = s.fetch(&s.list(&k, &f, ""));
        assert!(r.failures.is_empty(), "arrangements symlinked out: {:?}", r.failures);
        assert!(arr.join("Fuka/KadathThorn/src/fuka/src/a.cpp").is_file());
    }

    // From the third review.

    /// The exclude is bytes (git reads it so): a line that is not UTF-8 does
    /// not stop the pair from going, and stays byte for byte.
    #[test]
    fn review3_non_utf8_exclude() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let ex = s.repos().join("KadathThorn/.git/info/exclude");
        let mut bytes = std::fs::read(&ex).unwrap();
        bytes.extend_from_slice(b"caf\xe9.o\n");
        std::fs::write(&ex, &bytes).unwrap();
        let unplacing = unplace_dropped(&s.install(), &placed, &without_fuka(&s, &k)).unwrap();
        assert_eq!(unplacing.links(), ["KadathThorn/src/fuka"]);
        assert!(unplacing.dropped[0].exclude_removed, "{:?}", unplacing.notes);
        let pair = format!("{EXCLUDE_COMMENT}\n/src/fuka\n");
        let expected: Vec<u8> = {
            let at = bytes.windows(pair.len()).position(|w| w == pair.as_bytes()).unwrap();
            [&bytes[..at], &bytes[at + pair.len()..]].concat()
        };
        assert_eq!(std::fs::read(&ex).unwrap(), expected);
        // And back again, as bytes too.
        assert!(unplacing.restore(&s.install()).is_empty());
        assert_eq!(std::fs::read(&ex).unwrap(), [&expected[..], pair.as_bytes()].concat());
    }

    /// The exclude cannot be rewritten (a read-only `.git/info`) after the
    /// link went: no error with the link silently gone, but a note that says
    /// the link was taken away and the line left (prune still covers it).
    #[test]
    fn an_exclude_that_cannot_be_rewritten_is_a_note() {
        use std::os::unix::fs::PermissionsExt;
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let info = s.repos().join("KadathThorn/.git/info");
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::write(info.join("probe"), "").is_ok() {
            std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o755)).unwrap();
            return; // root: permissions do not bind
        }
        let unplacing = unplace_dropped(&s.install(), &placed, &without_fuka(&s, &k));
        std::fs::set_permissions(&info, std::fs::Permissions::from_mode(0o755)).unwrap();
        let unplacing = unplacing.unwrap();
        assert_eq!(unplacing.links(), ["KadathThorn/src/fuka"]);
        assert!(
            unplacing.notes.iter().any(|n| n.contains("was left")
                && n.contains("its link was taken away")
                && n.contains("prune")),
            "{:?}",
            unplacing.notes
        );
        assert!(std::fs::read_to_string(info.join("exclude")).unwrap().contains("/src/fuka"));
    }

    /// A temporary a crash left beside the exclude (a file, or a link to
    /// somewhere else) is cleared, never written through, and none is left.
    #[test]
    fn a_stale_exclude_temporary_is_cleared() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let info = s.repos().join("KadathThorn/.git/info");
        let victim = s.tmp.path().join("victim");
        std::fs::write(&victim, "precious\n").unwrap();
        // A dead process's temporary (a link to somewhere else), and an
        // older cactup's fixed name: both cleared, never written through.
        // A live process's (this one's pid) is left alone.
        std::os::unix::fs::symlink(&victim, info.join(".exclude.cactup-tmp")).unwrap();
        std::os::unix::fs::symlink(&victim, info.join(format!("{EXCLUDE_TEMP}999999999-0"))).unwrap();
        let live = format!("{EXCLUDE_TEMP}{}-999999", std::process::id());
        std::fs::write(info.join(&live), "in use\n").unwrap();
        unplace_dropped(&s.install(), &placed, &without_fuka(&s, &k)).unwrap();
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "precious\n");
        assert!(!std::fs::read_to_string(info.join("exclude")).unwrap().contains("/src/fuka"));
        let mut names: Vec<String> = std::fs::read_dir(&info)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names, [live.as_str(), "exclude"]);
    }

    /// Putting a link back where something sits now says to move that aside
    /// first: a bare `ln -s` into a directory would make the link inside it.
    #[test]
    fn a_restore_blocked_by_something_there_says_to_move_it_aside() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let unplacing = unplace_dropped(&s.install(), &placed, &without_fuka(&s, &k)).unwrap();
        let path = s.repos().join("KadathThorn/src/fuka");
        std::fs::create_dir(&path).unwrap();
        let problems = unplacing.restore(&s.install());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(
            problems[0].contains("aside first") && problems[0].contains("ln -s ../../fuka"),
            "{}",
            problems[0]
        );
    }

    /// The undo of a command (dropped while armed): the placement the
    /// command newly made goes, the one it dropped comes back, and the
    /// exclude is as it was. Committed, nothing is undone.
    #[test]
    fn an_armed_undo_restores_the_prior_placements() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("src/a.cpp", "a\n", false)]);
        let old = s.list(&k, &f, "");
        assert!(s.fetch(&old).failures.is_empty());
        let exclude = s.repos().join("KadathThorn/.git/info/exclude");
        let before = std::fs::read_to_string(&exclude).unwrap();
        let new = old.replace("$ROOT/repos/KadathThorn/src\n", "$ROOT/repos/KadathThorn/ext\n");
        let install = s.install();
        {
            let mut undo = PutBack::drop_placements(&install, &old, &new).unwrap();
            assert_eq!(undo.unplacing().links(), ["KadathThorn/src/fuka"]);
            let list = crate::thornlist::parse(&new).unwrap();
            let mut progress = prodash::tree::Root::new().add_child("plan");
            let plan = super::super::plan(&list, &install, &mut progress).unwrap();
            let report = super::super::execute(&plan, &install).unwrap();
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            undo.made(&plan, &report);
            assert!(s.repos().join("KadathThorn/ext/fuka").exists());
            // A failure here: the undo runs as it goes out of scope.
        }
        assert!(
            std::fs::symlink_metadata(s.repos().join("KadathThorn/ext/fuka")).is_err(),
            "the new one went"
        );
        assert_eq!(
            std::fs::read_link(s.repos().join("KadathThorn/src/fuka")).unwrap(),
            Path::new("../../fuka")
        );
        assert_eq!(std::fs::read_to_string(&exclude).unwrap(), before);

        let undo = PutBack::drop_placements(&install, &old, &new).unwrap();
        assert!(s.fetch(&new).failures.is_empty());
        undo.commit();
        assert!(s.repos().join("KadathThorn/ext/fuka").exists());
        assert!(std::fs::symlink_metadata(s.repos().join("KadathThorn/src/fuka")).is_err());
    }

    // From the fourth review.

    /// A link that was there before the command, in a placement `before`
    /// does not name but `after` does: an armed undo leaves it, since this
    /// run did not make it (the fetch report would say `Unchanged`).
    #[test]
    fn review4_undo_removes_a_link_it_did_not_make() {
        let s = Setup::new();
        let k = commit_entries(&s.kadath, &[("src/build.sh", "1\n", false)]);
        let f = commit_entries(&s.fuka, &[("Cmake/CMakeLists.txt", "fuka\n", false)]);
        let placed = s.list(&k, &f, "");
        assert!(s.fetch(&placed).failures.is_empty());
        let link = s.repos().join("KadathThorn/src/fuka");
        let root = s.install();
        {
            let mut undo = PutBack::drop_placements(&root, &without_fuka(&s, &k), &placed).unwrap();
            // The run fetches `placed` again: the link reads Unchanged.
            let list = crate::thornlist::parse(&placed).unwrap();
            let mut progress = prodash::tree::Root::new().add_child("plan");
            let plan = super::super::plan(&list, &root, &mut progress).unwrap();
            let report = super::super::execute(&plan, &root).unwrap();
            undo.made(&plan, &report);
            // ...and then fails: the undo runs as it goes out of scope.
        }
        assert!(std::fs::symlink_metadata(&link).is_ok(), "pre-existing link removed by undo");
    }
}
