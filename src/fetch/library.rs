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
//! The one write inside the dependent, the link, is made by [`place`], after
//! every clone of the fetch has finished (see `fetch::execute`), in the order
//! [`order`] gives, and only when the checks there pass: a plain relative
//! `into`; a dependent that is really checked out, at its source; no symlink
//! or non-directory on the way; nothing tracked at the path; and nothing at
//! all at the path except the link cactup made. It never overwrites, moves or
//! deletes anything.

use super::link::LinkOutcome;
use crate::thornlist::Component;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use std::collections::{BTreeMap, BTreeSet};
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
/// names (no empty, `.` or `..` part, no root), none of them `.git`, and of
/// characters a thornlist `!TARGET` and `!NAME` carry verbatim. The registry
/// checks this too; cactup does not take its word for it, since the path is
/// written to disk.
pub fn check_into(into: &str) -> Res<()> {
    let plain = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
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
    let mut at: BTreeMap<(String, String), &Placement> = BTreeMap::new();
    for p in &placements {
        if let Some(other) = at.insert((p.dependent.clone(), p.into.clone()), p) {
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
    let libraries: BTreeSet<&str> = placements.iter().map(|p| p.repo.as_str()).collect();
    let mut done: Vec<bool> = vec![false; placements.len()];
    let mut placed_repos: BTreeMap<&str, usize> = BTreeMap::new();
    let mut out = Vec::with_capacity(placements.len());
    let count = |repo: &str| placements.iter().filter(|p| p.repo == repo).count();
    while out.len() < placements.len() {
        let ready = (0..placements.len()).find(|&i| {
            let p = &placements[i];
            !done[i]
                && (!libraries.contains(p.dependent.as_str())
                    || placed_repos.get(p.dependent.as_str()).copied().unwrap_or(0)
                        == count(&p.dependent))
        });
        let Some(i) = ready else {
            let stuck: Vec<String> = (0..placements.len())
                .filter(|&i| !done[i])
                .map(|i| format!("{} inside {}", placements[i].repo, placements[i].dependent))
                .collect();
            bail!("Libraries placed inside each other in a cycle: {}", stuck.join(", "));
        };
        done[i] = true;
        *placed_repos.entry(placements[i].repo.as_str()).or_default() += 1;
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

/// Whether the dependent's repo tracks anything at `into` or under it.
fn tracked_at(repo: &gix::Repository, into: &str) -> Res<Option<String>> {
    let index = repo.index_or_empty().context("Failed to read the dependent's index")?;
    let under = format!("{into}/");
    Ok(index
        .entries()
        .iter()
        .map(|e| e.path(&index).to_string())
        .find(|p| p == into || p.starts_with(&under)))
}

/// What is at `path`, without following a symlink there.
enum Found {
    Nothing,
    Dir,
    Link(PathBuf),
    Other(&'static str),
}

fn found(path: &Path) -> Res<Found> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Found::Nothing),
        Err(e) => Err(anyhow!(e).context(format!("Failed to read '{}'", path.display()))),
        Ok(m) if m.file_type().is_symlink() => Ok(Found::Link(std::fs::read_link(path)?)),
        Ok(m) if m.is_dir() => Ok(Found::Dir),
        Ok(m) if m.is_file() => Ok(Found::Other("file")),
        Ok(_) => Ok(Found::Other("special file")),
    }
}

/// Walk `into` below `dependent` one part at a time, without following
/// symlinks: every part before the last that exists must be a plain
/// directory. Returns how many of those parts exist (the rest are missing).
fn walk(dependent: &Path, into: &str) -> Res<usize> {
    let parts: Vec<&str> = into.split('/').collect();
    let mut dir = dependent.to_path_buf();
    for (i, part) in parts[..parts.len() - 1].iter().enumerate() {
        dir.push(part);
        match found(&dir)? {
            Found::Dir => {}
            Found::Nothing => return Ok(i),
            Found::Link(to) => bail!(
                "'{}' is a symlink (to '{}'); cactup places a library only through plain directories",
                dir.display(),
                to.display()
            ),
            Found::Other(kind) => {
                bail!("'{}' is a {kind}, not a directory, so nothing can go inside it", dir.display())
            }
        }
    }
    Ok(parts.len() - 1)
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

/// Make sure the dependent's git ignores `/<into>`, via its
/// `.git/info/exclude` (never a tracked file such as `.gitignore`), so the
/// link does not show as an untracked file of the dependent in `git status`
/// or `cactup inst delta`.
fn exclude(repo: &gix::Repository, into: &str) -> Res<()> {
    let info = repo.git_dir().join("info");
    let file = info.join("exclude");
    let line = format!("/{into}");
    if let Found::Link(_) | Found::Other(_) = found(&info)? {
        bail!("'{}' is not a plain directory; not writing the exclude for {line} there", info.display());
    }
    let text = match found(&file)? {
        Found::Nothing => String::new(),
        Found::Other("file") => std::fs::read_to_string(&file)
            .with_context(|| format!("Failed to read '{}'", file.display()))?,
        _ => bail!("'{}' is not a plain file; not writing the exclude for {line} there", file.display()),
    };
    if text.lines().any(|l| l.trim_end() == line) {
        return Ok(());
    }
    std::fs::create_dir_all(&info).with_context(|| format!("Failed to create '{}'", info.display()))?;
    let mut out = text;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("# A library cactup placed (a link to its own clone)\n");
    out.push_str(&line);
    out.push('\n');
    std::fs::write(&file, out).with_context(|| format!("Failed to write '{}'", file.display()))
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

/// Place one library: `repos/<dependent>/<into>` becomes a relative link to
/// `repos/<repo>`. Run after every clone of the fetch has finished (hence
/// [`ClonesDone`]). `failed` names what failed to fetch in this run, and
/// `unplaced` the libraries not placed: a placement whose dependent or
/// library is among them is not attempted.
///
/// Refused, with the path named, and nothing written: an `into` that is not a
/// plain relative path; a dependent that is not checked out, or not at its
/// source (`origin` and pin); a library clone likewise; any symlink or
/// non-directory on the way; a path the dependent tracks files at; and
/// anything already at the path other than the link this would make. The
/// path is checked again, part by part, immediately before the link is made,
/// and the link is created without replacing anything.
pub fn place(
    _clones_done: &ClonesDone,
    install_root: &Path,
    root: &str,
    p: &Placement,
    failed: &BTreeSet<String>,
    unplaced: &BTreeSet<String>,
) -> Res<LinkOutcome> {
    check_into(&p.into)?;
    let parts: Vec<&str> = p.into.split('/').collect();
    let name = *parts.last().expect("check_into: not empty");
    if name != p.repo {
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

    let repos = install_root.join(root).join("repos");
    let repos = std::fs::canonicalize(&repos)
        .with_context(|| format!("No repos directory at '{}'", repos.display()))?;
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
    if let Some(path) = tracked_at(&dependent, &p.into)? {
        bail!(
            "Not placed: {} tracks '{path}', so '{}' is the dependent's own; cactup will not put a library over it",
            p.dependent,
            dependent_dir.join(&p.into).display()
        );
    }

    let target = dependent_dir.join(&p.into);
    let want = repos.join(&p.repo);
    let existing = walk(&dependent_dir, &p.into)?;
    let parent_depth = parts.len(); // the dependent itself plus every part before the last
    if existing == parts.len() - 1 {
        match found(&target)? {
            Found::Nothing => {}
            Found::Link(to) => {
                let resolved = cactup_ecosystem_common::crl::lexical_canonicalize(
                    &target.parent().expect("inside the dependent").join(&to).to_string_lossy(),
                );
                if Path::new(&resolved) == want {
                    exclude(&dependent, &p.into)?;
                    return Ok(LinkOutcome::Unchanged);
                }
                bail!(
                    "Not placed: '{}' is already a link, to '{}', which cactup did not make for {}; move it aside",
                    target.display(),
                    to.display(),
                    p.repo
                );
            }
            Found::Dir => bail!(
                "Not placed: '{}' is already a directory; cactup never overwrites one (a library is a link to its \
                 own clone, '{}'); move it aside",
                target.display(),
                want.display()
            ),
            Found::Other(kind) => {
                bail!(
                    "Not placed: '{}' is already a {kind}; cactup never overwrites one; move it aside",
                    target.display()
                )
            }
        }
    }

    // The folders on the way that are missing, one at a time: each must be
    // new (create_dir fails on anything already there).
    let mut dir = dependent_dir.clone();
    for (i, part) in parts[..parts.len() - 1].iter().enumerate() {
        dir.push(part);
        if i >= existing {
            std::fs::create_dir(&dir)
                .with_context(|| format!("Failed to create '{}'", dir.display()))?;
        }
    }

    // Immediately before writing: the whole way again, by lstat, and the
    // parent, resolved, still inside the dependent, itself inside repos/.
    if walk(&dependent_dir, &p.into)? != parts.len() - 1 {
        bail!("Not placed: the way to '{}' changed while it was being made", target.display());
    }
    let parent = target.parent().expect("inside the dependent");
    let real_parent = std::fs::canonicalize(parent)?;
    let real_dependent = std::fs::canonicalize(&dependent_dir)?;
    if !real_parent.starts_with(&real_dependent) || real_dependent.parent() != Some(repos.as_path()) {
        bail!(
            "Not placed: '{}' resolves to '{}', outside {}'s checkout",
            parent.display(),
            real_parent.display(),
            p.dependent
        );
    }
    // symlink(2) never replaces: anything that appeared at the path since is an error.
    std::os::unix::fs::symlink(link_text(parent_depth, &p.repo), &target)
        .with_context(|| format!("Failed to link '{}' to {}", target.display(), want.display()))?;
    exclude(&dependent, &p.into)?;
    Ok(LinkOutcome::Created)
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
}
