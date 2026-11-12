//! `cactup add owner/pkg[/Thorn][@range]…` (spec §8.11): put Cactarium
//! packages' thorns into the active installation, at the commit the registry
//! froze for the version.
//!
//! The installation records what was added (`installation.toml`'s
//! `[[added]]`), and its live thornlist is always the pristine base list with
//! those packages composed onto it: the base byte for byte, then one
//! cactup-managed block of generated sections. So the base stays exactly what
//! was installed (a stack release, say), refetch's hand-edit guard compares
//! the live list against that composition rather than mistaking an `add` for
//! a hand edit, and a refetch of a new base re-applies every added package.

use super::Ctx;
use crate::installation::{AddedLibrary, AddedPackage, AddedThorn, Installation};
use crate::thornlist::{self, Component, ComponentType};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use cactup_ecosystem_common::slug::Slug;
use colored::Colorize;
use std::collections::BTreeSet;

/// The line that opens the managed block; everything from it on is
/// generated, and rewritten on every composition.
pub const BLOCK_HEADER: &str = "# Added by `cactup add` (generated; change it with cactup add, not by hand)";

/// The live thornlist for `base` with `added` composed onto it: the base,
/// with the checkouts of any repo a package replaces dropped (by parsed
/// component), then one section per added package, each pinned. No packages:
/// the base itself. The result must parse — a checkout two places claim is
/// the parser's duplicate error, not a silent winner.
pub fn compose(base: &str, added: &[AddedPackage]) -> Res<String> {
    if added.is_empty() {
        return Ok(base.to_owned());
    }
    let list = thornlist::parse(base).context("The base thornlist does not parse")?;
    let replaced: BTreeSet<&str> = added.iter().flat_map(|a| a.replaces.iter().map(String::as_str)).collect();
    let claimed: BTreeSet<&str> =
        added.iter().flat_map(|a| a.thorns.iter().map(|t| t.checkout.as_str())).collect();
    let drop: BTreeSet<&str> = list
        .components()
        .iter()
        .filter(|c| replaced.contains(c.repo.as_str()) && claimed.contains(c.checkout.as_str()))
        .map(|c| c.checkout.as_str())
        .collect();
    let mut out = crate::sim::trim::drop_checkouts(base, &list, &drop, false)?;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push('\n');
    out.push_str(BLOCK_HEADER);
    out.push('\n');
    let root = list.root();
    let components: Vec<Component> = added
        .iter()
        .flat_map(|a| {
            a.thorns.iter().map(move |t| {
                let repo = clone_of(&a.repo, t);
                // A branch tip follows its default branch (named in `tag`):
                // no pin, so a refetch moves it with the tip.
                let tip = is_tip(&a.version);
                Component {
                    ty: ComponentType::Git,
                    target: format!("{root}/arrangements"),
                    checkout: t.checkout.clone(),
                    name: Some(repo.clone()),
                    url: Some(a.url.clone()),
                    auth_url: None,
                    anon_user: None,
                    anon_pass: None,
                    repo_path: (!t.repo_path.is_empty()).then(|| t.repo_path.clone()),
                    branch: a.tag.clone(),
                    commit: (!tip).then(|| a.commit.clone()),
                    checksum: None,
                    fetch_only: false,
                    repo,
                }
            })
        })
        .collect();
    let mut components = components;
    components.extend(library_sections(root, &list, added)?);
    let emitted = cactup_ecosystem_common::crl_emit::emit(root, &components, &[], None);
    let sections = emitted.find("!TARGET").map(|i| &emitted[i..]).unwrap_or("");
    out.push_str(sections);
    thornlist::parse(&out).context("The thornlist with the added packages does not parse")?;
    Ok(out)
}

/// One section per library the added packages build against (see
/// [`crate::fetch::library`]): the whole repo (`!CHECKOUT = .`, on the `!`
/// line, so Cactus never reads it as a thorn), cloned as `repos/<name>` and
/// linked in as `<name>` at `!TARGET`, inside the clone it goes into, pinned
/// at its version's commit. Refused up front, rather than left to a parse
/// error or a fetch that picks a winner: an `into` that is not a plain
/// relative path; two libraries at one path; two in one directory of a clone
/// (the CRL reads two whole-repo checkouts at one `!TARGET` as one path);
/// and a library clone whose name another source already uses. The same
/// library at the same commit wanted twice is one section.
fn library_sections(root: &str, base: &thornlist::Thornlist, added: &[AddedPackage]) -> Res<Vec<Component>> {
    use std::collections::BTreeMap;
    let mut sections: Vec<Component> = Vec::new();
    // Each clone's source, so a name used twice must mean one source: the
    // base's repos, the packages' clones, then the libraries'. Names (and
    // paths) are compared without case: on a case-insensitive filesystem
    // `Fuka` and `fuka` are one directory.
    let fold = |s: &str| s.to_ascii_lowercase();
    let mut sources: BTreeMap<String, (String, Option<String>, String)> = BTreeMap::new();
    let normalized = |u: &str| crate::fetch::git::normalize_url(u);
    for c in base.components() {
        if let Some(url) = &c.url {
            let source = (normalized(url), c.commit.clone(), "the installation's thornlist".to_owned());
            sources.entry(fold(&c.repo)).or_insert(source);
        }
    }
    for a in added {
        for clone in package_repos(a) {
            let commit = (!is_tip(&a.version)).then(|| a.commit.clone());
            sources.entry(fold(&clone)).or_insert((normalized(&a.url), commit, a.package.clone()));
        }
    }
    let mut at: BTreeMap<String, (String, &AddedLibrary)> = BTreeMap::new();
    // A library whose repository the base checks out stays where the base
    // puts it (a new base, say a stack release that now carries it), and so
    // does what goes inside it.
    let in_base: BTreeSet<String> = base.components().iter().filter_map(|c| c.url.as_deref()).map(normalized).collect();
    let mut left: BTreeSet<String> = BTreeSet::new();
    for a in added {
        for l in &a.libraries {
            crate::fetch::library::check_into(&l.into)
                .with_context(|| format!("{} places its library {}", a.package, l.package))?;
            crate::fetch::library::check_into(&l.inside)
                .with_context(|| format!("{}'s library {} goes inside \"{}\"", a.package, l.package, l.inside))?;
            if l.inside.contains('/') {
                bail!("{}'s library {} goes inside \"{}\", which is no clone's name", a.package, l.package, l.inside);
            }
            let name = l.clone_name();
            if in_base.contains(&normalized(&l.url)) || left.contains(&fold(&l.inside)) {
                left.insert(fold(name));
                continue;
            }
            let (dir, _) = l.into.rsplit_once('/').unwrap_or(("", name));
            let target = match dir {
                "" => format!("{root}/repos/{}", l.inside),
                dir => format!("{root}/repos/{}/{dir}", l.inside),
            };
            let path = format!("{}/{}", l.inside, l.into);
            if let Some((other, them)) = at.get(&fold(&path)) {
                if them.url == l.url && them.commit == l.commit {
                    continue;
                }
                bail!(
                    "{other} and {} both put a library at {path} ({} and {}); one path holds one checkout",
                    a.package,
                    them.package,
                    l.package
                );
            }
            if let Some((other, them)) = at.values().find(|(_, them)| {
                them.inside.eq_ignore_ascii_case(&l.inside)
                    && them.into.rsplit_once('/').map_or("", |(d, _)| d).eq_ignore_ascii_case(dir)
            }) {
                bail!(
                    "{}'s library {} and {}'s library {} both go in {}/{dir}; cactup places one library per \
                     directory of a repository",
                    other, them.package, a.package, l.package, l.inside
                );
            }
            let source = (normalized(&l.url), Some(l.commit.clone()), l.package.clone());
            match sources.get(&fold(name)) {
                Some((url, commit, who)) if (url, commit) != (&source.0, &source.1) => bail!(
                    "{}'s library {} would be cloned as repos/{name}, a name {who} already uses for another source; \
                     one clone holds one source",
                    a.package,
                    l.package
                ),
                _ => {
                    sources.insert(fold(name), source);
                }
            }
            at.insert(fold(&path), (a.package.clone(), l));
            sections.push(Component {
                ty: ComponentType::Git,
                target,
                checkout: ".".into(),
                name: Some(name.to_owned()),
                url: Some(l.url.clone()),
                auth_url: None,
                anon_user: None,
                anon_pass: None,
                repo_path: None,
                branch: l.tag.clone(),
                commit: Some(l.commit.clone()),
                checksum: None,
                fetch_only: true,
                repo: name.to_owned(),
            });
        }
    }
    Ok(sections)
}

/// The clone thorn `t` of a package is fetched into. `t.repo_path` is the
/// `!REPO_PATH` written for it, verbatim (the Cactarium derives it from where
/// the thorn sits in the repository, or it is the base list's own, when the
/// base already clones the repository). GetComponents links the checkout to
/// `repos/<clone>/<REPO_PATH>`, so a path that starts `../<name>` leaves the
/// clone and comes back in through `<name>`: the clone must then be named
/// that, `$1`/`$2` filled in from the checkout, as for the Einstein
/// Toolkit's ExternalLibraries (`../$1-$2`). Any other path is inside the
/// package's own clone, `package_repo`.
fn clone_of(package_repo: &str, t: &crate::installation::AddedThorn) -> String {
    let Some(rest) = t.repo_path.strip_prefix("../") else { return package_repo.to_owned() };
    let (arrangement, thorn) = t.checkout.rsplit_once('/').unwrap_or(("", &t.checkout));
    let name = rest.split('/').next().unwrap_or(rest);
    name.replace("$1", arrangement).replace("$2", thorn)
}

/// A base that already clones the package's repository (`url`, normalized)
/// keeps its clone: the package's thorns go into it, laid out as the base's
/// section lays that repository out (its clone's name and `!REPO_PATH`),
/// rather than into a second clone of the same repository.
fn reuse_base_clone(
    base: &thornlist::Thornlist,
    url: &mut String,
    repo: &mut String,
    thorns: &mut [crate::installation::AddedThorn],
) -> bool {
    let normalized = crate::fetch::git::normalize_url(url);
    let Some(same) = base
        .components()
        .iter()
        .find(|c| c.url.as_deref().is_some_and(|u| crate::fetch::git::normalize_url(u) == normalized))
    else {
        return false;
    };
    // The base's own spelling of the URL too: one clone has one source, and
    // the fetch planner compares them as written.
    url.clone_from(same.url.as_ref().expect("matched on it"));
    repo.clone_from(&same.repo);
    for t in thorns {
        t.repo_path = same.repo_path.clone().unwrap_or_default();
    }
    true
}

/// A package going into a clone the base already has (see
/// [`reuse_base_clone`]): the base's other checkouts of that clone stay as the
/// base has them, so the package must match their source.
fn share_base_clone(base: &thornlist::Thornlist, repo: &str, w: &mut Wanted) -> Res<()> {
    // One clone is at one commit: the base's other checkouts of it
    // stay at the base's pin, so the package must be at the same one.
    let claimed: BTreeSet<&str> = w.thorns.iter().map(|t| t.checkout.as_str()).collect();
    let stay: Vec<&thornlist::Component> =
        base.components().iter().filter(|c| c.repo == repo && !claimed.contains(c.checkout.as_str())).collect();
    // The fetch planner wants one source per clone: URL, branch and
    // commit alike. A tip follows a branch, so it must be the same
    // branch, unpinned; a version must be at the same commit, and
    // then takes the base's branch label (only a label beside a pin).
    let tip = is_tip(&w.version);
    let at = if tip { None } else { Some(w.commit.as_str()) };
    if let Some(other) =
        stay.iter().find(|c| c.commit.as_deref() != at || (tip && c.branch.as_deref() != w.tag.as_deref()))
    {
        let names: Vec<&str> = stay.iter().map(|c| c.checkout.as_str()).collect();
        bail!(
            "{} would share the clone {repo} with the base's {}, which stays at {}; one clone holds one \
             commit. Add a version at that commit, or replace those thorns too",
            w.slug,
            names.join(", "),
            other.commit.as_deref().map_or_else(
                || format!("branch {}", other.branch.as_deref().unwrap_or("(default)")),
                |c| c[..12.min(c.len())].to_owned()
            )
        );
    }
    if let Some(first) = stay.first()
        && !tip
    {
        w.tag.clone_from(&first.branch);
    }
    Ok(())
}

/// Whether an added package's version is a branch tip (`head`, `master`):
/// its default branch, followed rather than pinned.
pub(crate) fn is_tip(version: &str) -> bool {
    matches!(version, "head" | "master")
}

/// Whether an added package's version is a full commit id: the package as
/// it was at a commit no published version names, pinned there.
pub(crate) fn is_commit(version: &str) -> bool {
    cactup_ecosystem_common::slug::is_commit_id(version)
}

/// An added package as people read it: `owner/pkg@1.2.0`, `owner/pkg@head`,
/// or `owner/pkg at commit 0123456789ab`.
pub(crate) fn describe(package: &str, version: &str) -> String {
    if is_commit(version) {
        format!("{package} at commit {}", &version[..12])
    } else {
        format!("{package}@{version}")
    }
}

/// The names the Cactarium gives a thorn become paths on disk and words in a
/// thornlist (which GetComponents also hands to a shell, single-quoted), so
/// they are checked before anything is written: an arrangement and a thorn
/// are single plain path components; a `!REPO_PATH` is plain path
/// characters, `$1`/`$2`, and whatever clone name it implies is a single
/// plain component too.
fn check_registry_names(slug: &str, arrangement: &str, thorn: &str, repo_path: &str) -> Res<()> {
    let plain = |s: &str| {
        !s.is_empty()
            && s != "."
            && s != ".."
            && !s.starts_with('-')
            && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if !plain(arrangement) || !plain(thorn) {
        bail!("The Cactarium names a thorn of {slug} \"{arrangement}/{thorn}\", which is not a plain Arrangement/Thorn");
    }
    // `..` only as the leading `../<clone>` that reaches another clone by
    // name; anywhere else it would climb out of the clone (or of `repos/`).
    let segments: Vec<&str> = repo_path.split('/').collect();
    let dots_ok = segments.iter().enumerate().all(|(i, seg)| *seg != ".." || (i == 0 && segments.len() > 1 && segments[1] != ".."));
    let path_ok = dots_ok && repo_path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-/$".contains(&b));
    let checkout = format!("{arrangement}/{thorn}");
    let clone = clone_of("package", &crate::installation::AddedThorn { checkout, repo_path: repo_path.to_owned() });
    if !path_ok || !plain(&clone) {
        bail!("The Cactarium gives {arrangement}/{thorn} of {slug} the repo-path \"{repo_path}\", which cactup will not write");
    }
    Ok(())
}

/// The source the Cactarium gives a package goes into its thornlist section,
/// which GetComponents hands to a shell (`git clone --branch '<label>'`,
/// `git checkout <label>` unquoted): the URL holds no whitespace, quote or
/// control character, the commit is hex, and the label (the version's tag,
/// or a tip's default branch, which the package's author controls) is plain
/// ref characters.
fn check_registry_source(slug: &str, url: &str, commit: &str, label: Option<&str>) -> Res<()> {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control() || "'\"`$;\\".contains(c)) {
        bail!("The Cactarium gives {slug} the repository \"{}\", which cactup will not write", url.escape_debug());
    }
    if commit.is_empty() || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("The Cactarium gives {slug} the commit \"{}\", which is not a commit id", commit.escape_debug());
    }
    if let Some(label) = label
        && (label.is_empty()
            || label.starts_with('-')
            || label.contains("..")
            || !label.bytes().all(|b| b.is_ascii_alphanumeric() || b"._/+-".contains(&b)))
    {
        bail!("{slug} names the branch or tag \"{}\", which cactup will not write", label.escape_debug());
    }
    Ok(())
}

/// Every clone a package is fetched into.
fn package_repos(a: &AddedPackage) -> BTreeSet<String> {
    a.thorns.iter().map(|t| clone_of(&a.repo, t)).collect()
}

/// `e`, the failure of a command after it dropped library placements, with
/// those put back first; what could not be put back is said, with how.
pub(crate) fn put_back(e: anyhow::Error, unplacing: &crate::fetch::library::Unplacing, root: &std::path::Path) -> anyhow::Error {
    let problems = unplacing.restore(root);
    if problems.is_empty() { e } else { anyhow!("{e:#}\n{}", problems.join("\n")) }
}

/// Narrow `plan` to `repos`: their git work and links, and only the library
/// placements of or into them (never every placement of the list); no
/// downloads, externals or skips. Nothing else in the installation moves.
fn narrow(plan: &mut crate::fetch::Plan, repos: &BTreeSet<&str>) {
    plan.git.retain(|g| repos.contains(g.repo.as_str()));
    plan.links.retain(|c| repos.contains(c.repo.as_str()));
    plan.libraries.retain(|p| repos.contains(p.repo.as_str()) || repos.contains(p.dependent.as_str()));
    plan.downloads.clear();
    plan.external.clear();
    plan.skipped.clear();
}

/// A library's clone goes at `repos/<name>`, a name the registry chooses
/// (the last part of `into`). One already on disk must be a clone cactup
/// fetched from that same repository (its fetch record says so), never a
/// directory of the user's that a forced refetch would then retarget.
fn check_library_dirs(install_root: &std::path::Path, root: &str, added: &[AddedPackage]) -> Res<()> {
    let state = crate::fetch::FetchState::read(install_root)?;
    let repos = install_root.join(root).join("repos");
    for a in added {
        for l in &a.libraries {
            let name = l.clone_name();
            let dir = repos.join(name);
            if std::fs::symlink_metadata(&dir).is_err() {
                continue;
            }
            let url = crate::fetch::git::normalize_url(&l.url);
            let record = state.as_ref().and_then(|s| s.repos.get(name));
            match record {
                Some(r) if crate::fetch::git::normalize_url(&r.url) == url => {}
                Some(r) => bail!(
                    "{}'s library {} would be cloned at '{}', but cactup fetched that directory from {}, not from \
                     {}: another repository. Move '{}' aside (it is that other repository's checkout), then run \
                     the add again",
                    a.package,
                    l.package,
                    dir.display(),
                    r.url,
                    l.url,
                    dir.display()
                ),
                None => bail!(
                    "{}'s library {} would be cloned at '{}', which is already there but has no fetch record in \
                     this installation: cactup did not fetch it, or an older cactup or an import did, and cactup \
                     will not take over a directory it cannot tell is its own. If it is a checkout of {} you no \
                     longer need, delete or move it aside; if it holds work of yours, move it elsewhere first. \
                     Then run the add again, and cactup clones the library there",
                    a.package,
                    l.package,
                    dir.display(),
                    l.url
                ),
            }
        }
    }
    Ok(())
}

/// Every clone a package is fetched into, its libraries' included.
fn all_repos(a: &AddedPackage) -> BTreeSet<String> {
    let mut repos = package_repos(a);
    repos.extend(a.libraries.iter().map(|l| l.clone_name().to_owned()));
    repos
}

/// `package`'s libraries (as resolved, `inside` empty for its own clone),
/// placed: those inside the package go inside its clone, the one its thorns
/// are fetched into. A library whose repository the base already checks out
/// is left where the base puts it (a stack's or sim's own thornlist places
/// its libraries itself), with a note in `notes`, and so are the libraries
/// that go inside it.
fn libraries_inside(
    base: &thornlist::Thornlist,
    package: &AddedPackage,
    libraries: Vec<AddedLibrary>,
    notes: &mut Vec<String>,
) -> Res<Vec<AddedLibrary>> {
    if libraries.is_empty() {
        return Ok(libraries);
    }
    let clones = package_repos(package);
    let mut clones = clones.iter();
    let (Some(own), None) = (clones.next(), clones.next()) else {
        bail!(
            "{} builds against libraries, which go inside its repository, but its thorns come from several clones \
             ({}), so cactup cannot tell which one is its repository",
            package.package,
            package_repos(package).into_iter().collect::<Vec<_>>().join(", ")
        );
    };
    let in_base = |url: &str| {
        let url = crate::fetch::git::normalize_url(url);
        base.components().iter().find(|c| c.url.as_deref().is_some_and(|u| crate::fetch::git::normalize_url(u) == url))
    };
    let mut left: BTreeSet<String> = BTreeSet::new();
    let mut out = Vec::new();
    for mut library in libraries {
        if library.inside.is_empty() {
            own.clone_into(&mut library.inside);
        }
        if left.contains(&library.inside) {
            left.insert(library.clone_name().to_owned());
            continue;
        }
        if let Some(c) = in_base(&library.url) {
            notes.push(format!(
                "{}'s library {} stays where the installation's thornlist puts it: it already checks out its \
                 repository (as {}).",
                package.package, library.package, c.repo
            ));
            left.insert(library.clone_name().to_owned());
            continue;
        }
        out.push(library);
    }
    Ok(out)
}

/// A repo's name as a thornlist derives it from its URL (no `!NAME`): the
/// last path segment, `.git` stripped.
fn repo_name(url: &str) -> String {
    let stripped = url.trim_end_matches('/').trim_end_matches(".git");
    stripped.rsplit(['/', ':']).next().unwrap_or(stripped).to_owned()
}

/// One package an `add` names, its specs resolved and merged: which version,
/// at which commit, and which of its thorns.
struct Wanted {
    entry: String,
    /// `owner/name@version`, as the Cactarium names the resolved version.
    slug: String,
    version: String,
    tag: Option<String>,
    commit: String,
    url: String,
    thorns: Vec<AddedThorn>,
    contact: Option<String>,
    /// Only some thorns were named, so a package added before keeps the
    /// thorns it had.
    only_named: bool,
    /// The libraries the version builds against, each with `inside` empty
    /// when it goes inside the package's own clone (which is named only
    /// once the base has been looked at).
    libraries: Vec<AddedLibrary>,
}

/// Resolve every spec, merging the ones of one package: several thorns of
/// it (`owner/pkg/A owner/pkg/B`) are one add of those thorns, and naming
/// the package whole anywhere adds all of them. Two specs that resolve to
/// different versions of one package are an error, not a silent winner.
fn resolve_all(
    client: &crate::cactarium::Client,
    specs: &[String],
    allow_untrusted: bool,
    interactive: bool,
) -> Res<Vec<Wanted>> {
    use crate::cactarium::Unreviewed;
    let mut wanted: Vec<Wanted> = Vec::new();
    let mut listings: std::collections::HashMap<String, cactup_ecosystem_common::api::PackageVersions> =
        std::collections::HashMap::new();
    for spec in specs {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let slug: Slug =
            spec.parse().map_err(|e| anyhow!("\"{spec}\" is not a package (owner/name[/Thorn][@version|#commit]): {e}"))?;
        let entry = slug.entry();
        let req = match (&slug.at, slug.sha()) {
            (Some(at), _) => format!("{entry}@{at}"),
            (None, Some(sha)) => format!("{entry}#{sha}"),
            (None, None) => entry.clone(),
        };
        let resolved = client.resolve(&req)?;
        if resolved.kind != "package" {
            bail!("{} is a {}, not a package; install a stack with `cactup install`", resolved.slug, resolved.kind);
        }
        crate::cactarium::warn_if_yanked(&resolved.slug, &resolved.version);
        let commit =
            resolved.version.commit.clone().ok_or_else(|| anyhow!("The Cactarium gave {} no commit", resolved.slug))?;
        let url = resolved.url.clone().ok_or_else(|| anyhow!("The Cactarium gave {} no repository", resolved.slug))?;
        // A branch tip (`@head`, `@master`) is no version in the list: its
        // thorns come at the tip's commit, with whether anyone vouches for it.
        let tip_version;
        // The label recorded with it: the tag a version was cut from, or the
        // default branch a tip follows, named so a refetch knows which branch
        // to move along (and never mistakes an earlier pin's tag for one).
        let mut label = resolved.version.tag.clone();
        // `#<sha>` names only a commit (full or abbreviated); `@<x>` is read
        // as one only when it names no version, range or track. A commit no
        // published version was frozen at comes back as the package read
        // there, `owner/pkg#<full id>`, recorded at that full id; one a
        // version was frozen at comes back as that version, added as any is.
        // A `#<sha>` answered with anything (the package there, or the
        // version frozen there) must be at a commit that id names.
        if let Some(sha) = slug.sha()
            && !commit.starts_with(sha)
        {
            bail!("Asked the Cactarium for {req}, and it answered with {} at commit {commit}", resolved.slug);
        }
        let read_at_commit = resolved.slug.contains('#') || is_commit(&resolved.version.version);
        let wanted_version = if read_at_commit { commit.clone() } else { resolved.version.version.clone() };
        let version = if read_at_commit {
            // What was asked must be what came back: a prefix of the full id.
            let asked = slug.sha().or_else(|| slug.commit_fallback());
            if !is_commit(&commit) || !asked.is_some_and(|prefix| commit.starts_with(prefix)) {
                bail!("Asked the Cactarium for {req}, and it answered with {} at commit {commit}", resolved.slug);
            }
            let (version, untrusted) = client.package_at(slug.owner.as_str(), slug.name.as_str(), &commit)?;
            // A version frozen at that commit since `/resolve` answers here as
            // that version: not the read this was asked for, so start over.
            if version.version.version != commit {
                bail!(
                    "The Cactarium published {}@{} at commit {commit} while it was being added; run the add again",
                    entry,
                    version.version.version
                );
            }
            if untrusted {
                let what = Unreviewed::Commit;
                crate::cactarium::confirm_untrusted_on(&resolved.slug, "package", what, allow_untrusted, interactive)?;
            }
            // Pinned, so no branch to follow: no label beside the pin.
            label = None;
            tip_version = version;
            &tip_version
        } else if let Some(tip) = slug.at.as_deref().filter(|_| slug.tip().is_some()) {
            let (version, untrusted) = client.package_at(slug.owner.as_str(), slug.name.as_str(), tip)?;
            if untrusted {
                let what = Unreviewed::Tip;
                crate::cactarium::confirm_untrusted_on(&resolved.slug, "package", what, allow_untrusted, interactive)?;
            }
            let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
            let line = crate::progress::Line::over(
                progress.add_child(resolved.slug.clone()),
                &resolved.slug,
                crate::progress::Layout::for_names([resolved.slug.as_str()]),
            );
            line.phase("asking its repository for its default branch");
            let branch = crate::fetch::git::ask_remote_default_branch(&url);
            drop(line);
            if let Some(renderer) = renderer {
                renderer.shutdown_and_wait();
            }
            label = Some(branch?);
            tip_version = version;
            &tip_version
        } else {
            if !listings.contains_key(&entry) {
                let versions = client.package_versions(slug.owner.as_str(), slug.name.as_str())?;
                listings.insert(entry.clone(), versions);
            }
            listings[&entry]
                .versions
                .iter()
                .find(|v| v.version.version == resolved.version.version)
                .ok_or_else(|| anyhow!("The Cactarium lists no thorns for {}", resolved.slug))?
        };
        check_registry_source(&resolved.slug, &url, &commit, label.as_deref())?;
        // A package with no thorns by design is no empty package, and has
        // nothing to put in the thornlist on its own.
        if version.library {
            bail!(
                "{} is a library, which has no thorns: it is checked out where a package that builds against it \
                 says (the `into` of that package's dependency on it). Add that package, and the library comes with it",
                resolved.slug
            );
        }
        if version.flesh {
            bail!(
                "{} is a Cactus flesh, which has no thorns, and an installation has its flesh already; to build on \
                 another one, install a stack that uses it (`cactup install`)",
                resolved.slug
            );
        }
        let thorns: Vec<AddedThorn> = version
            .thorns
            .iter()
            .filter(|t| slug.thorn.as_deref().is_none_or(|want| want.eq_ignore_ascii_case(&t.thorn)))
            .map(|t| AddedThorn { checkout: format!("{}/{}", t.arrangement, t.thorn), repo_path: t.repo_path.clone() })
            .collect();
        for t in &version.thorns {
            check_registry_names(&resolved.slug, &t.arrangement, &t.thorn, &t.repo_path)?;
        }
        // The package's own clone is named from its URL: a plain component too.
        let clone = repo_name(&url);
        if clone.is_empty()
            || clone == "."
            || clone == ".."
            || clone.starts_with('-')
            || !clone.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            bail!("The Cactarium gives {} the repository {url}, whose name cactup will not use as a directory", resolved.slug);
        }
        if let Some(t) = thorns.iter().find(|t| t.repo_path.trim().is_empty()) {
            bail!(
                "The Cactarium gave no repo-path for {} in {}, so cactup cannot tell where it sits in the repository \
                 (a problem on the Cactarium's side; report it there)",
                t.checkout,
                resolved.slug
            );
        }
        if thorns.is_empty() {
            let all: Vec<&str> = version.thorns.iter().map(|t| t.thorn.as_str()).collect();
            bail!(
                "{} has no thorn {} (its thorns: {})",
                resolved.slug,
                slug.thorn.as_deref().unwrap_or("at all"),
                all.join(", ")
            );
        }
        let only_named = slug.thorn.is_some();
        // A package named twice is one version (checked below), so one set
        // of libraries: resolved the first time only.
        let libraries = if wanted.iter().any(|w| w.entry == entry) {
            Vec::new()
        } else {
            let claims = version.claims.clone();
            let mut chain = vec![entry.clone()];
            let mut out = Vec::new();
            let mut ask = Asker { client, allow_untrusted, interactive, listings: &mut listings };
            resolve_libraries(&mut ask, &resolved.slug, &claims, "", &mut chain, &mut out)?;
            out
        };
        match wanted.iter_mut().find(|w| w.entry == entry) {
            Some(w) if w.version != wanted_version => bail!(
                "{} and {} are two versions of {entry}; name one",
                w.slug,
                resolved.slug
            ),
            Some(w) => {
                for t in thorns {
                    if !w.thorns.iter().any(|have| have.checkout == t.checkout) {
                        w.thorns.push(t);
                    }
                }
                w.only_named &= only_named;
            }
            None => wanted.push(Wanted {
                entry,
                slug: resolved.slug.to_string(),
                version: wanted_version,
                tag: label,
                commit,
                url,
                thorns,
                contact: resolved.contact.clone(),
                only_named,
                libraries,
            }),
        }
    }
    // A package named whole is all of its thorns, whatever else named some.
    // (A tip's thorns came whole from the tip itself; it has no listing.)
    for w in &mut wanted {
        if !w.only_named
            && let Some(listing) = listings.get(&w.entry)
        {
            if let Some(v) = listing.versions.iter().find(|v| v.version.version == w.version) {
                w.thorns = v
                    .thorns
                    .iter()
                    .map(|t| AddedThorn { checkout: format!("{}/{}", t.arrangement, t.thorn), repo_path: t.repo_path.clone() })
                    .collect();
            }
        }
    }
    Ok(wanted)
}

/// What resolving a library asks the Cactarium with: the client, the trust
/// flags, and the version listings already fetched.
struct Asker<'a> {
    client: &'a crate::cactarium::Client,
    allow_untrusted: bool,
    interactive: bool,
    listings: &'a mut std::collections::HashMap<String, cactup_ecosystem_common::api::PackageVersions>,
}

/// The libraries `claims` (of `dependent`, a resolved package or library)
/// place: every dependency with `into`, resolved to a version (or commit)
/// and checked to be a library, then that library's own libraries, which go
/// inside its clone, through the whole chain. `inside` is the clone the
/// claims' `into` is relative to (empty: the added package's own); `chain`
/// holds the packages above, to refuse a cycle. `into` is checked here too:
/// it becomes a path on disk.
fn resolve_libraries(
    ask: &mut Asker,
    dependent: &str,
    claims: &[cactup_ecosystem_common::api::ClaimInfo],
    inside: &str,
    chain: &mut Vec<String>,
    out: &mut Vec<AddedLibrary>,
) -> Res<()> {
    use crate::cactarium::Unreviewed;
    for claim in claims.iter().filter(|c| c.kind == "dependency") {
        let Some(into) = &claim.into else { continue };
        crate::fetch::library::check_into(into)
            .with_context(|| format!("{dependent} places {} at \"{}\"", claim.target, into.escape_debug()))?;
        let target: Slug = claim
            .target
            .parse()
            .map_err(|e| anyhow!("{dependent} depends on \"{}\", which is not a package: {e}", claim.target))?;
        let entry = target.entry();
        if chain.contains(&entry) {
            bail!("Libraries that go inside each other in a cycle: {} -> {entry}", chain.join(" -> "));
        }
        let req = match (&claim.range, &claim.sha) {
            (_, Some(sha)) => format!("{entry}#{sha}"),
            (Some(range), None) => format!("{entry}@{range}"),
            (None, None) => entry.clone(),
        };
        let resolved = ask.client.resolve(&req).with_context(|| format!("{dependent} builds against {req}"))?;
        if resolved.kind != "package" {
            bail!("{dependent} builds against {}, a {}, not a library package", resolved.slug, resolved.kind);
        }
        crate::cactarium::warn_if_yanked(&resolved.slug, &resolved.version);
        let commit =
            resolved.version.commit.clone().ok_or_else(|| anyhow!("The Cactarium gave {} no commit", resolved.slug))?;
        let url = resolved.url.clone().ok_or_else(|| anyhow!("The Cactarium gave {} no repository", resolved.slug))?;
        if let Some(sha) = &claim.sha
            && !commit.starts_with(sha.as_str())
        {
            bail!("Asked the Cactarium for {req}, and it answered with {} at commit {commit}", resolved.slug);
        }
        // A commit no published version names comes back as the package
        // read there, as for `cactup add owner/pkg#<sha>`.
        let read_at_commit = resolved.slug.contains('#') || is_commit(&resolved.version.version);
        let (version, label, read) = if read_at_commit {
            if !is_commit(&commit) {
                bail!("Asked the Cactarium for {req}, and it answered with {} at commit {commit}", resolved.slug);
            }
            let (at, untrusted) = ask.client.package_at(target.owner.as_str(), target.name.as_str(), &commit)?;
            if untrusted {
                let what = Unreviewed::Commit;
                let (allow, interactive) = (ask.allow_untrusted, ask.interactive);
                crate::cactarium::confirm_untrusted_on(&resolved.slug, "package", what, allow, interactive)?;
            }
            (commit.clone(), None, at)
        } else {
            if !ask.listings.contains_key(&entry) {
                let versions = ask.client.package_versions(target.owner.as_str(), target.name.as_str())?;
                ask.listings.insert(entry.clone(), versions);
            }
            let read = ask.listings[&entry]
                .versions
                .iter()
                .find(|v| v.version.version == resolved.version.version)
                .cloned()
                .ok_or_else(|| anyhow!("The Cactarium does not list {}", resolved.slug))?;
            (resolved.version.version.clone(), resolved.version.tag.clone(), read)
        };
        check_registry_source(&resolved.slug, &url, &commit, label.as_deref())?;
        if !read.library {
            bail!(
                "{dependent} says where {} goes (into = \"{into}\"), but {} is not a library; only a library is \
                 placed by the package that uses it (a problem with {dependent}'s manifest or the Cactarium)",
                claim.target,
                resolved.slug
            );
        }
        let inside = inside.to_owned();
        let library = AddedLibrary { package: entry.clone(), version, tag: label, commit, url, inside, into: into.clone() };
        let clone = library.clone_name().to_owned();
        out.push(library);
        chain.push(entry);
        resolve_libraries(ask, &resolved.slug, &read.claims, &clone, chain, out)?;
        chain.pop();
    }
    Ok(())
}

pub fn dispatch(ctx: &Ctx, specs: &[String], replace: bool, force: bool) -> Res<()> {
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let wanted =
        resolve_all(&client, specs, ctx.globals.risky_allow_untrusted_artifacts, crate::cactarium::interactive())?;

    let inst = Installation::resolve(ctx)?;
    let locked = inst.locked()?;
    let mut meta = locked.meta()?;
    let pristine = std::fs::read_to_string(inst.source_thornlist_to_read())
        .context("This installation has no pristine thornlist to add to")?;
    let live_path = inst.live_thornlist();
    let live = std::fs::read_to_string(inst.live_thornlist_to_read()).unwrap_or_default();
    // The live list must be what cactup composed, or the add would silently
    // discard edits made to it by hand.
    let before = compose(&pristine, &meta.added)?;
    let edits = super::refetch::component_divergence(&live, &before);
    if !edits.is_empty() && !force {
        bail!(
            "'{}' has hand edits, which adding would overwrite:\n  {}\nMove them into a thornlist of your own, or pass -f",
            live_path.display(),
            edits.join("\n  ")
        );
    }

    let base = thornlist::parse(&pristine)?;
    let mut added: Vec<AddedPackage> = Vec::new();
    let mut notes: Vec<String> = Vec::new();
    for w in wanted {
        // What in the base the package collides with: the same repository,
        // or a thorn it also checks out.
        let mut repo = repo_name(&w.url);
        let mut thorns = w.thorns;
        let mut url = w.url;
        let reused = reuse_base_clone(&base, &mut url, &mut repo, &mut thorns);
        let mut w = Wanted { thorns, url, ..w };
        if reused {
            share_base_clone(&base, &repo, &mut w)?;
        }
        let want: BTreeSet<&str> = w.thorns.iter().map(|t| t.checkout.as_str()).collect();
        let normalized = crate::fetch::git::normalize_url(&w.url);
        let conflicts: BTreeSet<String> = base
            .components()
            .iter()
            .filter(|c| {
                c.repo == repo
                    || c.url.as_deref().is_some_and(|u| crate::fetch::git::normalize_url(u) == normalized)
                    || want.contains(c.checkout.as_str())
            })
            .map(|c| c.repo.clone())
            .collect();
        if !conflicts.is_empty() && !replace {
            bail!(
                "The installation's thornlist already has {} (repo {}); pass --replace to use {} in its place",
                want.iter().copied().collect::<Vec<_>>().join(", "),
                conflicts.iter().cloned().collect::<Vec<_>>().join(", "),
                w.slug
            );
        }

        // Record it: a second add of the same package updates it, adding
        // thorns to the ones already enabled when only thorns were named.
        let mut package = AddedPackage {
            package: w.entry.clone(),
            version: w.version,
            tag: w.tag,
            commit: w.commit,
            url: w.url,
            repo,
            thorns: w.thorns,
            replaces: conflicts.into_iter().collect(),
            contact: w.contact,
            libraries: Vec::new(),
        };
        package.libraries = libraries_inside(&base, &package, w.libraries, &mut notes)?;
        if let Some(i) = meta.added.iter().position(|a| a.package == w.entry) {
            let old = meta.added.remove(i);
            if w.only_named {
                for t in old.thorns {
                    if !package.thorns.iter().any(|n| n.checkout == t.checkout) {
                        package.thorns.push(t);
                    }
                }
            }
            for r in old.replaces {
                if !package.replaces.contains(&r) {
                    package.replaces.push(r);
                }
            }
        }
        meta.added.push(package.clone());
        added.push(package);
    }
    let composed = compose(&pristine, &meta.added)?;

    // Fetch those repos (each package's own, and its libraries') — never
    // the rest of the installation.
    let repos: BTreeSet<String> = added.iter().flat_map(all_repos).collect();
    let repos: BTreeSet<&str> = repos.iter().map(String::as_str).collect();
    let list = thornlist::parse(&composed)?;
    check_library_dirs(&inst.root, list.root(), &added)?;
    let (progress, renderer) = crate::manifest::setup_prodash();
    let mut classify = progress.add_child("classify repos");
    let mut plan = crate::fetch::plan(&list, &inst.root, &mut classify)?;
    drop(classify);
    renderer.shutdown_and_wait();
    if let Some(skipped) = plan.skipped.iter().find(|s| repos.contains(s.repo.as_str())) {
        bail!("Repo {} in the installation has {}; it was left alone", skipped.repo, skipped.reason.describe());
    }
    narrow(&mut plan, &repos);
    // After every refusal, just before the fetch: a placement the add drops
    // (its `into` changed, or the base took the library over) loses its link
    // and exclude line, since a stale link would have the new list's
    // sections resolved through it. A failed fetch puts them back.
    let unplacing = crate::fetch::library::unplace_dropped(&inst.root, &before, &composed)?;
    let report = match crate::fetch::execute(&plan, &inst.root) {
        Ok(report) => report,
        Err(e) => return Err(put_back(e, &unplacing, &inst.root)),
    };
    if !report.failures.is_empty() {
        // What did fetch is recorded all the same, so a later prune knows
        // cactup made it.
        crate::fetch::FetchState::record(&inst.root, &report.records())?;
        let why: Vec<String> = report.failures.iter().map(|f| format!("{}: {}", f.what, f.error)).collect();
        let e = anyhow!("Failed to fetch {}:\n  {}", repos.iter().copied().collect::<Vec<_>>().join(", "), why.join("\n  "));
        return Err(put_back(e, &unplacing, &inst.root));
    }
    crate::fetch::FetchState::record(&inst.root, &report.records())?;

    std::fs::write(&live_path, &composed).with_context(|| format!("Failed to write '{}'", live_path.display()))?;
    locked.set_meta(&meta)?;
    drop(locked);

    for package in &added {
        let checkouts: Vec<&str> = package.thorns.iter().map(|t| t.checkout.as_str()).collect();
        let short = &package.commit[..12.min(package.commit.len())];
        let at = if is_tip(&package.version) {
            format!(" following its default branch, now at {short}")
        } else if is_commit(&package.version) {
            String::new()
        } else {
            format!(" at {short}")
        };
        println!(
            "Added {} ({}){at}{}.",
            describe(&package.package, &package.version).bold(),
            checkouts.join(", "),
            if package.replaces.is_empty() { String::new() } else { format!(", in place of {}", package.replaces.join(", ")) }
        );
        for library in &package.libraries {
            println!(
                "  with the library {} at {}/{} (its clone: {}).",
                describe(&library.package, &library.version).bold(),
                library.inside,
                library.into,
                library.clone_name()
            );
        }
    }
    for note in &notes {
        println!("{note}");
    }
    for link in unplacing.links() {
        println!("Took away the old library link at {link}.");
    }
    for note in &unplacing.notes {
        println!("{note}");
    }
    println!("Build a config to compile them, e.g. `{}`.", "cactup build <config>".bold());
    Ok(())
}

/// `cactup remove owner/name` (§8.11): undo an add. The package's record
/// goes, the live list is recomposed without it, and only the repos it
/// touched are fetched again — its own (left on disk, now unlisted; `inst
/// refetch --prune` removes it) and any base repo it replaced, which goes
/// back to the base's pin.
pub fn remove(ctx: &Ctx, package: &str, force: bool) -> Res<()> {
    let slug: Slug = package.parse().map_err(|e| anyhow!("\"{package}\" is not a package (owner/name): {e}"))?;
    if slug.at.is_some() || slug.sha.is_some() || slug.thorn.is_some() {
        bail!("Name the package as owner/name; remove takes the whole package");
    }
    let entry = format!("{}/{}", slug.owner, slug.name);
    let inst = Installation::resolve(ctx)?;
    let locked = inst.locked()?;
    let mut meta = locked.meta()?;
    let Some(i) = meta.added.iter().position(|a| a.package == entry) else {
        let added: Vec<&str> = meta.added.iter().map(|a| a.package.as_str()).collect();
        bail!(
            "{entry} was not added to this installation (added: {})",
            if added.is_empty() { "nothing".to_owned() } else { added.join(", ") }
        );
    };
    let pristine = std::fs::read_to_string(inst.source_thornlist_to_read())
        .context("This installation has no pristine thornlist")?;
    let live_path = inst.live_thornlist();
    let live = std::fs::read_to_string(inst.live_thornlist_to_read()).unwrap_or_default();
    let before = compose(&pristine, &meta.added)?;
    let edits = super::refetch::component_divergence(&live, &before);
    if !edits.is_empty() && !force {
        bail!(
            "'{}' has hand edits, which removing would overwrite:\n  {}\nPass -f to discard them",
            live_path.display(),
            edits.join("\n  ")
        );
    }
    let removed = meta.added.remove(i);
    let composed = compose(&pristine, &meta.added)?;

    // The replaced base repos go back to what the base pins.
    let touched: BTreeSet<&str> = removed.replaces.iter().map(String::as_str).collect();
    let mut restore = None;
    if !touched.is_empty() {
        let list = thornlist::parse(&composed)?;
        let (progress, renderer) = crate::manifest::setup_prodash();
        let mut classify = progress.add_child("classify repos");
        let mut plan = crate::fetch::plan(&list, &inst.root, &mut classify)?;
        drop(classify);
        renderer.shutdown_and_wait();
        if let Some(skipped) = plan.skipped.iter().find(|s| touched.contains(s.repo.as_str())) {
            bail!("Repo {} has {}; it was left alone, and {entry} is still added", skipped.repo, skipped.reason.describe());
        }
        narrow(&mut plan, &touched);
        restore = Some(plan);
    }
    // After every refusal: its libraries' links and exclude lines go with it
    // (only what placing made; whatever else is at those paths is left, with
    // a note). A failed restore of the base's repos puts them back.
    let unplacing = crate::fetch::library::unplace_dropped(&inst.root, &before, &composed)?;
    if let Some(plan) = restore {
        let report = match crate::fetch::execute(&plan, &inst.root) {
            Ok(report) => report,
            Err(e) => return Err(put_back(e, &unplacing, &inst.root)),
        };
        if !report.failures.is_empty() {
            let why: Vec<String> = report.failures.iter().map(|f| format!("{}: {}", f.what, f.error)).collect();
            let e = anyhow!("Failed to restore {}:\n  {}", removed.replaces.join(", "), why.join("\n  "));
            return Err(put_back(e, &unplacing, &inst.root));
        }
        crate::fetch::FetchState::record(&inst.root, &report.records())?;
    }
    std::fs::write(&live_path, &composed).with_context(|| format!("Failed to write '{}'", live_path.display()))?;
    locked.set_meta(&meta)?;
    drop(locked);

    println!("Removed {} from the thornlist.", removed.package.bold());
    for link in unplacing.links() {
        println!("Took away its library link at {link}.");
    }
    for note in &unplacing.notes {
        println!("{note}");
    }
    if touched.is_empty() {
        let repos: Vec<String> = all_repos(&removed).into_iter().collect();
        println!("Its repo {} stays on disk; `cactup inst refetch --prune` removes it.", repos.join(", "));
    } else {
        println!("Restored {} to the base's commit.", removed.replaces.join(", "));
    }
    println!("Rebuild configs that compiled its thorns, e.g. `{}`.", "cactup build <config>".bold());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn package(repo: &str, checkouts: &[(&str, &str)], replaces: &[&str]) -> AddedPackage {
        AddedPackage {
            libraries: Vec::new(),
            package: format!("max/{repo}"),
            version: "mixed-precision:1.0.0".into(),
            tag: Some("mixed-precision/v1.0.0".into()),
            commit: "5".repeat(40),
            url: format!("https://x/{repo}.git"),
            repo: repo.into(),
            thorns: checkouts
                .iter()
                .map(|(c, p)| AddedThorn { checkout: (*c).into(), repo_path: (*p).into() })
                .collect(),
            replaces: replaces.iter().map(|r| (*r).to_string()).collect(),
            contact: None,
        }
    }

    const BASE: &str = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\n\
                        !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/carpetx.git\n!REPO_PATH = $2\n\
                        !CHECKOUT =\nCarpetX/CarpetX\nCarpetX/Loop\n\n\
                        !TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://x/cactusbase.git\n!REPO_PATH = $2\n\
                        !CHECKOUT =\nCactusBase/IOUtil\n";

    /// The base stays byte for byte; the added package follows in the
    /// managed block, pinned, one thorn per line; and with no packages the
    /// base is the whole list.
    #[test]
    fn compose_appends_a_pinned_block_and_keeps_the_base() {
        assert_eq!(compose(BASE, &[]).unwrap(), BASE);
        let out = compose(BASE, &[package("spacetimex", &[("SpacetimeX/Z4c", "$2"), ("SpacetimeX/ADM", "$2")], &[])]).unwrap();
        assert!(out.starts_with(BASE), "{out}");
        let list = thornlist::parse(&out).unwrap();
        let added: Vec<_> = list.components().iter().filter(|c| c.repo == "spacetimex").collect();
        assert_eq!(added.len(), 2);
        assert!(added.iter().all(|c| c.commit.as_deref() == Some(&*"5".repeat(40))));
        assert!(out.contains("!CHECKOUT   =\nSpacetimeX/Z4c\nSpacetimeX/ADM\n"), "{out}");
    }

    /// `--replace`: the base's checkouts of the replaced repo that the
    /// package claims are dropped, so the package's own stand in their
    /// place; without the record, the duplicate is an error.
    #[test]
    fn a_replacing_package_takes_its_checkouts_over_from_the_base() {
        let fork = package("carpetx-fork", &[("CarpetX/CarpetX", "$2"), ("CarpetX/Loop", "$2")], &["carpetx"]);
        let out = compose(BASE, std::slice::from_ref(&fork)).unwrap();
        let list = thornlist::parse(&out).unwrap();
        let loop_from: Vec<&str> =
            list.components().iter().filter(|c| c.checkout == "CarpetX/Loop").map(|c| c.repo.as_str()).collect();
        assert_eq!(loop_from, ["carpetx-fork"]);
        assert!(list.components().iter().any(|c| c.checkout == "CactusBase/IOUtil"), "the rest of the base stays");

        let unrecorded = AddedPackage { replaces: vec![], ..fork };
        assert!(compose(BASE, &[unrecorded]).is_err(), "two repos claiming one checkout must not compose");
    }

    /// `!REPO_PATH` is the Cactarium's value, verbatim, for every layout,
    /// and the clone follows from it: `../$1-$2` (a repository that is one
    /// thorn) reaches a clone named `<Arrangement>-<Thorn>`, anything else
    /// the package's own. Checked by fetching real repositories and
    /// following the links: each thorn lands at `arrangements/<Arr>/<Thorn>`.
    #[test]
    fn the_cactarium_s_repo_path_places_every_layout() {
        use crate::fetch::git::testrepo;
        let tmp = tempfile::tempdir().unwrap();
        let upstream = |name: &str, files: &[(&str, &str)], thorns: &[(&str, &str)]| -> AddedPackage {
            let dir = tmp.path().join("up").join(name);
            testrepo::init(&dir);
            let commit = crate::fetch::tests::commit_files(&dir, files);
            AddedPackage {
                libraries: Vec::new(),
                package: format!("et/{name}"),
                version: "1.0.0".into(),
                tag: None,
                commit: commit.to_string(),
                url: dir.to_string_lossy().into_owned(),
                repo: repo_name(&dir.to_string_lossy()),
                thorns: thorns.iter().map(|(c, p)| AddedThorn { checkout: (*c).into(), repo_path: (*p).into() }).collect(),
                replaces: Vec::new(),
                contact: None,
            }
        };
        let packages = [
            upstream("fakelib-moved", &[("interface.ccl", "fakelib\n")], &[("ExternalLibraries/FakeLib", "../$1-$2")]),
            upstream(
                "tools",
                &[("Foo/interface.ccl", "foo\n"), ("src/Bar/interface.ccl", "bar\n"), ("Tools/Baz/interface.ccl", "baz\n")],
                &[("Tools/Foo", "$2"), ("Tools/Bar", "src/$2"), ("Tools/Baz", "$1/$2")],
            ),
        ];
        let text = compose("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n", &packages).unwrap();
        if let Some(out) = std::env::var_os("CACTUP_TEST_DUMP_ADDED") {
            std::fs::write(out, &text).unwrap();
        }
        assert!(text.contains("!REPO_PATH   = ../$1-$2\n"), "{text}");
        assert!(text.contains("!NAME        = ExternalLibraries-FakeLib\n"), "{text}");
        assert!(text.contains("!REPO_PATH   = src/$2\n") && text.contains("!REPO_PATH   = $1/$2\n"), "{text}");
        assert_eq!(package_repos(&packages[0]), ["ExternalLibraries-FakeLib".to_string()].into_iter().collect());
        assert_eq!(package_repos(&packages[1]), ["tools".to_string()].into_iter().collect());

        let list = thornlist::parse(&text).unwrap();
        let root = tmp.path().join("install");
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = crate::fetch::plan(&list, &root, &mut progress).unwrap();
        let report = crate::fetch::execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        for (checkout, content) in
            [("ExternalLibraries/FakeLib", "fakelib\n"), ("Tools/Foo", "foo\n"), ("Tools/Bar", "bar\n"), ("Tools/Baz", "baz\n")]
        {
            let ccl = root.join("Cactus/arrangements").join(checkout).join("interface.ccl");
            assert_eq!(std::fs::read_to_string(&ccl).unwrap_or_else(|e| panic!("{checkout}: {e}")), content);
        }
    }

    /// A base that already clones the repository keeps its clone: an ET
    /// list's ExternalLibraries-HDF5 section is reused, name and layout,
    /// whatever the Cactarium derived; a repository the base does not have
    /// keeps the package's own.
    #[test]
    fn a_repository_the_base_already_clones_is_reused() {
        let base = thornlist::parse(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://github.com/EinsteinToolkit/$1-$2.git\n!REPO_PATH = ../$1-$2\n!CHECKOUT = ExternalLibraries/HDF5\n",
        )
        .unwrap();
        let mut repo = "hdf5-mirror".to_owned();
        let mut thorns = vec![AddedThorn { checkout: "ExternalLibraries/HDF5".into(), repo_path: "$2".into() }];
        let mut url = "https://github.com/EinsteinToolkit/ExternalLibraries-HDF5".to_owned();
        assert!(reuse_base_clone(&base, &mut url, &mut repo, &mut thorns));
        assert_eq!(url, "https://github.com/EinsteinToolkit/ExternalLibraries-HDF5.git", "the base's spelling");
        assert_eq!(repo, "ExternalLibraries-HDF5");
        assert_eq!(thorns[0].repo_path, "../$1-$2");

        let mut other = "carpetx".to_owned();
        let mut kept = vec![AddedThorn { checkout: "CarpetX/Arith".into(), repo_path: "$2".into() }];
        assert!(!reuse_base_clone(&base, &mut "https://x/carpetx.git".to_owned(), &mut other, &mut kept));
        assert_eq!((other.as_str(), kept[0].repo_path.as_str()), ("carpetx", "$2"));
    }

    /// A branch tip (`@head`, `@master`) follows its default branch: its
    /// section names that branch and no commit, so a refetch moves it.
    #[test]
    fn a_branch_tip_is_written_unpinned() {
        let mut tip = package("carpetx", &[("CarpetX/Arith", "$2")], &[]);
        tip.version = "head".into();
        tip.tag = Some("main".into());
        let out = compose(BASE, &[tip]).unwrap();
        let added = &out[out.find(BLOCK_HEADER).unwrap()..];
        assert!(!added.contains("!REPO_COMMIT"), "{added}");
        let list = thornlist::parse(&out).unwrap();
        let arith = list.components().iter().find(|c| c.checkout == "CarpetX/Arith").unwrap();
        assert_eq!((arith.commit.as_deref(), arith.branch.as_deref()), (None, Some("main")));
        assert!(is_tip("master") && !is_tip("1.0.0"));
    }

    /// Registry names become paths and thornlist words: anything but plain
    /// components (and a repo-path implying a clone named `..`) is refused.
    #[test]
    fn registry_sources_must_be_plain() {
        let c = "a".repeat(40);
        check_registry_source("x", "https://github.com/x/y.git", &c, Some("mixed-precision/v1.4.0")).unwrap();
        check_registry_source("x", "file:///repos/y.git", &c, None).unwrap();
        assert!(check_registry_source("x", "https://x/y\n!DEFINE ROOT = /", &c, None).is_err());
        assert!(check_registry_source("x", "https://x/y'", &c, None).is_err());
        assert!(check_registry_source("x", "https://x/y", "main", None).is_err(), "not a commit");
        for label in ["x';touch${IFS}pwn;'", "a b", "-rf", "a..b", ""] {
            assert!(check_registry_source("x", "https://x/y", &c, Some(label)).is_err(), "{label}");
        }
    }

    #[test]
    fn registry_names_must_be_plain() {
        check_registry_names("et/x", "ExternalLibraries", "HDF5", "../$1-$2").unwrap();
        check_registry_names("et/x", "CarpetX", "Arith", "$2").unwrap();
        for (arr, thorn, path) in
            [("..", "T", "$2"), ("A/B", "T", "$2"), ("A", "T'", "$2"), ("A", "T", "../.."), ("A", "T", "x/../../.."), ("A", "T", "../$1-$2/../.."), ("A", "T", "$2;rm"), ("A", "T", "a b")]
        {
            assert!(check_registry_names("et/x", arr, thorn, path).is_err(), "{arr} {thorn} {path}");
        }
    }

    /// A whole package at `@head` takes every thorn of the tip (it used to
    /// panic: a tip has no version listing to look the thorns up in), and
    /// follows the remote's default branch, asked of the repository itself.
    #[test]
    fn a_whole_package_tip_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("carpetx");
        crate::fetch::git::testrepo::init(&repo);
        crate::fetch::git::testrepo::commit_tree(&repo, &[("README", "x\n", false)]);
        let (branch, _) = crate::fetch::git::head_of(&repo).unwrap();
        let url = repo.to_string_lossy().into_owned();
        let resolved = format!(
            r#"{{"slug":"max/carpetx@head","kind":"package","url":"{url}","contact":null,"version":"head","track":null,
            "tag":null,"commit":"4715","tree":null,"yanked":false,"yank-reason":null,"state":"ok","curated":false,"published":0}}"#
        );
        let facts = serde_json::to_string(&cactup_ecosystem_common::ccl::ThornFacts::default()).unwrap();
        let tip = format!(
            r#"{{"version":"head","track":null,"tag":null,"commit":"4715","tree":null,"yanked":false,"yank-reason":null,
            "state":"ok","curated":false,"published":0,"url":"{url}","claims":[],
            "thorns":[{{"arrangement":"CarpetX","thorn":"Arith","repo-path":"$2","facts":{facts}}},
                      {{"arrangement":"CarpetX","thorn":"Loop","repo-path":"$2","facts":{facts}}}]}}"#
        );
        let (base, _) = crate::fetch::download::test_server::serve_with(2, move |path| {
            let body = if path.contains("/resolve") { resolved.clone() } else { tip.clone() };
            ("200 OK", "api-generation: 1\r\nx-cactarium-untrusted: false\r\n".into(), body.into_bytes())
        });
        let wanted = resolve_all(&crate::cactarium::Client::at(&base), &["max/carpetx@head".to_owned()], false, false)
            .unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(wanted.len(), 1);
        let thorns: Vec<&str> = wanted[0].thorns.iter().map(|t| t.checkout.as_str()).collect();
        assert_eq!(thorns, ["CarpetX/Arith", "CarpetX/Loop"]);
        assert_eq!(wanted[0].tag.as_deref(), Some(branch.as_str()), "the default branch, by name");
    }

    /// A Cactarium serving `max/carpetx` read at commit `commit` (no
    /// published version there, so it answers `max/carpetx#<commit>`),
    /// answering `untrusted` in the header.
    fn serve_commit(commit: &str, untrusted: bool) -> String {
        let facts = serde_json::to_string(&cactup_ecosystem_common::ccl::ThornFacts::default()).unwrap();
        let resolved = format!(
            r#"{{"slug":"max/carpetx#{commit}","kind":"package","url":"https://x/carpetx.git","contact":null,
            "version":"{commit}","track":null,"tag":null,"commit":"{commit}","tree":null,"yanked":false,
            "yank-reason":null,"state":"ok","curated":false,"published":0}}"#
        );
        let read = format!(
            r#"{{"version":"{commit}","track":null,"tag":null,"commit":"{commit}","tree":null,"yanked":false,
            "yank-reason":null,"state":"ok","curated":false,"published":0,"url":"https://x/carpetx.git","claims":[],
            "thorns":[{{"arrangement":"CarpetX","thorn":"Arith","repo-path":"$2","facts":{facts}}}]}}"#
        );
        let headers = format!("api-generation: 1\r\nx-cactarium-untrusted: {untrusted}\r\n");
        let (base, _) = crate::fetch::download::test_server::serve_with(2, move |path| {
            let body = if path.contains("/resolve") { resolved.clone() } else { read.clone() };
            ("200 OK", headers.clone(), body.into_bytes())
        });
        base
    }

    /// `owner/pkg#<sha>` (full or abbreviated), or `owner/pkg@<sha>` the
    /// Cactarium reads as a commit, that no published version was frozen
    /// at: the package read there, pinned to the full commit id with no
    /// branch label, and the untrusted confirmation applied as for a tip.
    #[test]
    fn a_package_at_a_commit_is_pinned_there() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let client = |base: &str| crate::cactarium::Client::at(base);
        let add = |spec: &str, untrusted: bool| {
            resolve_all(&client(&serve_commit(commit, untrusted)), &[spec.to_owned()], false, false)
        };

        let specs = [format!("max/carpetx#{commit}"), "max/carpetx#0123456".to_owned(), "max/carpetx@0123456789ab".to_owned()];
        for spec in specs {
            let wanted = add(&spec, false).unwrap_or_else(|e| panic!("{spec}: {e:#}"));
            let w = &wanted[0];
            assert_eq!((w.version.as_str(), w.commit.as_str(), w.tag.as_deref()), (commit, commit, None), "{spec}");
        }
        let w = &add("max/carpetx#0123456", false).unwrap()[0];
        let added = AddedPackage {
            libraries: Vec::new(),
            package: "max/carpetx".to_owned(),
            version: w.version.clone(),
            tag: w.tag.clone(),
            commit: w.commit.clone(),
            url: w.url.clone(),
            repo: repo_name(&w.url),
            contact: None,
            thorns: w.thorns.clone(),
            replaces: Vec::new(),
        };
        let base = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n";
        let text = compose(base, &[added]).unwrap();
        assert!(text.contains(&format!("!REPO_COMMIT = {commit}")) && !text.contains("!REPO_BRANCH"), "{text}");
        assert_eq!(describe("max/carpetx", commit), "max/carpetx at commit 0123456789ab");

        let err = format!("{:#}", add("max/carpetx#0123456", true).map(drop).unwrap_err());
        assert!(err.contains("untrusted commit") && err.contains("risky-allow-untrusted-artifacts"), "{err}");
        // An answer at a commit the request does not name is refused.
        let err = format!("{:#}", add("max/carpetx#fffffff", false).map(drop).unwrap_err());
        assert!(err.contains("answered with"), "{err}");

        // A `#<sha>` answered with a version at an unrelated commit, too.
        let resolved = format!(
            r#"{{"slug":"max/carpetx@1.2.0","kind":"package","url":"https://x/carpetx.git","contact":null,
            "version":"1.2.0","track":null,"tag":"v1.2.0","commit":"{}","tree":null,"yanked":false,
            "yank-reason":null,"state":"ok","curated":false,"published":0}}"#,
            "f".repeat(40)
        );
        let (base, _) = crate::fetch::download::test_server::serve_with(1, move |_| {
            ("200 OK", "api-generation: 1\r\n".into(), resolved.clone().into_bytes())
        });
        let err = format!("{:#}", resolve_all(&client(&base), &["max/carpetx#0123456".to_owned()], false, false)
            .map(drop)
            .unwrap_err());
        assert!(err.contains("answered with max/carpetx@1.2.0"), "{err}");
    }

    /// A package sharing a clone the base keeps other thorns of must match
    /// that clone's source as the fetch planner compares it: same commit
    /// (taking the base's branch label, which used to fail planning), or for
    /// a tip, the same branch unpinned. Anything else is refused up front.
    #[test]
    fn a_shared_clone_matches_the_base_s_source() {
        let c = "c".repeat(40);
        let base_text = format!(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://x/carpetx.git\n!REPO_PATH = $2\n!REPO_BRANCH = master\n!REPO_COMMIT = {c}\n\
             !CHECKOUT =\nCarpetX/Loop\nCarpetX/Arith\n"
        );
        let base = thornlist::parse(&base_text).unwrap();
        let wanted = |version: &str, tag: &str, commit: &str| Wanted {
            libraries: Vec::new(),
            entry: "max/carpetx".into(),
            slug: format!("max/carpetx@{version}"),
            version: version.into(),
            tag: Some(tag.into()),
            commit: commit.into(),
            url: "https://x/carpetx".into(),
            thorns: vec![AddedThorn { checkout: "CarpetX/Arith".into(), repo_path: "$2".into() }],
            contact: None,
            only_named: true,
        };
        // Same commit, other label: takes the base's label, and plans.
        let mut w = wanted("1.0.0", "v1.0.0", &c);
        let (mut url, mut repo, mut thorns) = (w.url.clone(), String::new(), w.thorns.clone());
        assert!(reuse_base_clone(&base, &mut url, &mut repo, &mut thorns));
        w.url = url;
        w.thorns = thorns;
        share_base_clone(&base, &repo, &mut w).unwrap();
        assert_eq!(w.tag.as_deref(), Some("master"));
        let package = AddedPackage {
            libraries: Vec::new(),
            package: w.entry.clone(),
            version: w.version.clone(),
            tag: w.tag.clone(),
            commit: w.commit.clone(),
            url: w.url.clone(),
            repo: repo.clone(),
            thorns: w.thorns.clone(),
            replaces: vec![repo.clone()],
            contact: None,
        };
        let list = thornlist::parse(&compose(&base_text, &[package]).unwrap()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        crate::fetch::plan(&list, tmp.path(), &mut progress).unwrap_or_else(|e| panic!("{e:#}"));

        // Another commit: refused.
        let mut other = wanted("1.1.0", "v1.1.0", &"d".repeat(40));
        assert!(format!("{:#}", share_base_clone(&base, &repo, &mut other).unwrap_err()).contains("one clone holds one commit"));
        // A tip over a pinned base clone: refused.
        let mut tip = wanted("head", "master", "e");
        assert!(share_base_clone(&base, &repo, &mut tip).is_err());
    }

    #[test]
    fn repo_names_follow_the_url() {
        assert_eq!(repo_name("file:///repos/carpetx.git"), "carpetx");
        assert_eq!(repo_name("git@github.com:max/SpacetimeX.git"), "SpacetimeX");
        assert_eq!(repo_name("https://x/y/z/"), "z");
    }

    // Library packages: et/kadaththorn builds against et/fuka, which goes
    // at `src/fuka` in KadathThorn's repo.

    fn kadath(libraries: Vec<AddedLibrary>) -> AddedPackage {
        AddedPackage {
            package: "et/kadaththorn".into(),
            version: "2026.5.0".into(),
            tag: Some("ET_2026_05".into()),
            commit: "a".repeat(40),
            url: "https://x/KadathThorn.git".into(),
            repo: "KadathThorn".into(),
            thorns: vec![AddedThorn { checkout: "Fuka/KadathThorn".into(), repo_path: "../$2".into() }],
            replaces: Vec::new(),
            contact: None,
            libraries,
        }
    }

    fn fuka(inside: &str, into: &str) -> AddedLibrary {
        AddedLibrary {
            package: "et/fuka".into(),
            version: "2026.5.0".into(),
            tag: Some("ET_2026_05".into()),
            commit: "b".repeat(40),
            url: "https://bitbucket.org/fukaws/fuka".into(),
            inside: inside.into(),
            into: into.into(),
        }
    }

    /// A library is a whole-repo section placed inside its dependent's
    /// clone, pinned, on the `!CHECKOUT` line (so Cactus never reads it as a
    /// thorn), and no thorn of the list.
    #[test]
    fn compose_places_a_library_inside_its_dependent() {
        let out = compose(BASE, &[kadath(vec![fuka("KadathThorn", "src/fuka")])]).unwrap();
        assert!(out.starts_with(BASE));
        assert!(out.contains("!TARGET      = Cactus/repos/KadathThorn/src\n"), "{out}");
        assert!(out.contains("!NAME        = fuka\n!CHECKOUT   = .\n"), "{out}");
        assert!(out.contains(&format!("!REPO_COMMIT = {}\n", "b".repeat(40))), "{out}");
        assert!(!out.lines().any(|l| l.trim() == "."), "never a bare line Cactus would read: {out}");
        let list = thornlist::parse(&out).unwrap();
        let lib = list.components().iter().find(|c| c.repo == "fuka").unwrap();
        assert_eq!((lib.checkout.as_str(), lib.fetch_only), (".", true));
        assert!(!list.thorn_providers().contains_key("fuka"), "a library is no thorn");
        assert_eq!(
            crate::fetch::library::placement_of(list.root(), lib),
            Some(("KadathThorn".into(), "src/fuka".into()))
        );
        // At the dependent's root, the target is the clone itself.
        let out = compose(BASE, &[kadath(vec![fuka("KadathThorn", "fuka")])]).unwrap();
        assert!(out.contains("!TARGET      = Cactus/repos/KadathThorn\n"), "{out}");
    }

    /// What compose refuses before anything is parsed or fetched.
    #[test]
    fn compose_refuses_libraries_that_collide() {
        let e = |libraries: Vec<AddedLibrary>| format!("{:#}", compose(BASE, &[kadath(libraries)]).unwrap_err());
        let other = AddedLibrary { package: "et/other".into(), url: "https://x/other".into(), ..fuka("KadathThorn", "src/fuka") };
        assert!(e(vec![fuka("KadathThorn", "src/fuka"), other]).contains("both put a library at KadathThorn/src/fuka"));
        let sibling = AddedLibrary { package: "et/other".into(), url: "https://x/other".into(), ..fuka("KadathThorn", "src/other") };
        assert!(e(vec![fuka("KadathThorn", "src/fuka"), sibling]).contains("one library per directory"));
        // The clone's name taken by another source in the base.
        assert!(e(vec![fuka("KadathThorn", "src/carpetx")]).contains("would be cloned as repos/carpetx"));
        // Paths the registry (or a hand-edited installation.toml) got wrong.
        for into in ["../fuka", "/fuka", "src/./fuka", ".git/fuka"] {
            let msg = e(vec![fuka("KadathThorn", into)]);
            assert!(msg.contains("places its library et/fuka"), "{into}: {msg}");
        }
        // The same library at the same commit, wanted twice: one section.
        let out = compose(BASE, &[kadath(vec![fuka("KadathThorn", "src/fuka"), fuka("KadathThorn", "src/fuka")])]).unwrap();
        assert_eq!(out.matches("!NAME        = fuka").count(), 1, "{out}");
    }

    /// Libraries go inside the package's own clone; one the base already
    /// checks out stays where the base puts it (with what goes inside it).
    #[test]
    fn libraries_go_inside_the_package_unless_the_base_has_them() {
        let base = thornlist::parse(BASE).unwrap();
        let resolved = vec![fuka("", "src/fuka"), AddedLibrary { package: "et/eigen".into(), url: "https://x/eigen".into(), ..fuka("fuka", "ext/eigen") }];
        let mut notes = Vec::new();
        let got = libraries_inside(&base, &kadath(Vec::new()), resolved.clone(), &mut notes).unwrap();
        assert_eq!(got.iter().map(|l| (l.inside.as_str(), l.into.as_str())).collect::<Vec<_>>(), [("KadathThorn", "src/fuka"), ("fuka", "ext/eigen")]);
        assert!(notes.is_empty());

        let with_fuka = format!("{BASE}\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = https://bitbucket.org/fukaws/fuka.git\n!CHECKOUT = Cmake\n");
        let base = thornlist::parse(&with_fuka).unwrap();
        let got = libraries_inside(&base, &kadath(Vec::new()), resolved, &mut notes).unwrap();
        assert!(got.is_empty(), "{got:?}");
        assert!(notes[0].contains("stays where the installation's thornlist puts it"), "{notes:?}");
        // A new base that carries it (a refetch to a newer stack release):
        // the recorded library is left to the base, not composed twice.
        let out = compose(&with_fuka, &[kadath(vec![fuka("KadathThorn", "src/fuka")])]).unwrap();
        assert!(!out.contains("!NAME        = fuka"), "{out}");
    }

    /// A Cactarium serving et/kadaththorn (whose version depends on
    /// `target` with `into`) and et/fuka (a library unless `fuka_is_library`
    /// is false).
    fn serve_kadath(target: &str, into: &str, fuka_is_library: bool, requests: usize) -> String {
        let facts = serde_json::to_string(&cactup_ecosystem_common::ccl::ThornFacts::default()).unwrap();
        let info = |version: &str, commit: &str, tag: &str| {
            format!(
                r#""version":"{version}","track":null,"tag":"{tag}","commit":"{commit}","tree":null,"yanked":false,
                "yank-reason":null,"state":"ok","curated":false,"published":0"#
            )
        };
        let (kc, fc) = ("a".repeat(40), "b".repeat(40));
        let resolve_kadath = format!(
            r#"{{"slug":"et/kadaththorn@2026.5.0","kind":"package","url":"https://x/KadathThorn.git","contact":null,{}}}"#,
            info("2026.5.0", &kc, "ET_2026_05")
        );
        let kadath_versions = format!(
            r#"{{"slug":"et/kadaththorn","versions":[{{{},"url":"https://x/KadathThorn.git",
            "thorns":[{{"arrangement":"Fuka","thorn":"KadathThorn","repo-path":"../$2","facts":{facts}}}],
            "claims":[{{"kind":"dependency","target":"{target}","range":"^2026.5.0","into":"{into}","note":null}}]}}]}}"#,
            info("2026.5.0", &kc, "ET_2026_05")
        );
        let resolve_fuka = format!(
            r#"{{"slug":"et/fuka@2026.5.1","kind":"package","url":"https://bitbucket.org/fukaws/fuka","contact":null,{}}}"#,
            info("2026.5.1", &fc, "ET_2026_05_v1")
        );
        let fuka_versions = format!(
            r#"{{"slug":"et/fuka","versions":[{{{},"url":"https://bitbucket.org/fukaws/fuka","library":{fuka_is_library},
            "thorns":[],"claims":[]}}]}}"#,
            info("2026.5.1", &fc, "ET_2026_05_v1")
        );
        let (base, _) = crate::fetch::download::test_server::serve_with(requests, move |path| {
            let body = match path {
                p if p.contains("/resolve") && p.contains("kadaththorn") => resolve_kadath.clone(),
                p if p.contains("/resolve") => resolve_fuka.clone(),
                p if p.contains("/packages/et/kadaththorn/") => kadath_versions.clone(),
                _ => fuka_versions.clone(),
            };
            ("200 OK", "api-generation: 1\r\n".into(), body.into_bytes())
        });
        base
    }

    /// `cactup add et/kadaththorn`: its dependency on the library et/fuka
    /// comes along, resolved to a version and its commit, placed at the
    /// dependency's `into`.
    #[test]
    fn adding_a_package_brings_its_library() {
        let base = serve_kadath("et/fuka", "src/fuka", true, 4);
        let wanted = resolve_all(&crate::cactarium::Client::at(&base), &["et/kadaththorn".to_owned()], false, false)
            .unwrap_or_else(|e| panic!("{e:#}"));
        let libs = &wanted[0].libraries;
        assert_eq!(libs.len(), 1);
        let l = &libs[0];
        assert_eq!(
            (l.package.as_str(), l.version.as_str(), l.commit.as_str(), l.inside.as_str(), l.into.as_str(), l.tag.as_deref()),
            ("et/fuka", "2026.5.1", &*"b".repeat(40), "", "src/fuka", Some("ET_2026_05_v1"))
        );
        assert_eq!(l.clone_name(), "fuka");
        let thorns: Vec<&str> = wanted[0].thorns.iter().map(|t| t.checkout.as_str()).collect();
        assert_eq!(thorns, ["Fuka/KadathThorn"], "the library adds no thorns");
    }

    /// The registry's `into` is checked again here: a path out of the
    /// dependent is refused before anything else is asked.
    #[test]
    fn a_registry_into_that_leaves_the_repo_is_refused() {
        let base = serve_kadath("et/fuka", "../../escape", true, 2);
        let err = resolve_all(&crate::cactarium::Client::at(&base), &["et/kadaththorn".to_owned()], false, false).map(drop).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("places et/fuka at \"../../escape\"") && err.contains("not a plain folder name"), "{err}");
    }

    /// `into` on a dependency whose target is no library is refused.
    #[test]
    fn into_on_a_package_with_thorns_is_refused() {
        let base = serve_kadath("et/fuka", "src/fuka", false, 4);
        let err = resolve_all(&crate::cactarium::Client::at(&base), &["et/kadaththorn".to_owned()], false, false).map(drop).unwrap_err();
        assert!(format!("{err:#}").contains("is not a library"), "{err:#}");
    }

    /// `cactup add et/fuka`: a library on its own has nowhere to go; said
    /// as that, not as an empty package.
    #[test]
    fn adding_a_library_alone_says_what_it_is() {
        let base = serve_kadath("et/fuka", "src/fuka", true, 2);
        let err = resolve_all(&crate::cactarium::Client::at(&base), &["et/fuka".to_owned()], false, false).map(drop).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("et/fuka@2026.5.1 is a library, which has no thorns") && err.contains("Add that package"), "{err}");
        assert!(!err.contains("has no thorn "), "{err}");
    }

    /// The whole way, on disk: KadathThorn added with FUKA as its library,
    /// composed, planned and fetched as `cactup add` fetches; FUKA is at
    /// `repos/KadathThorn/src/fuka`, where KadathThorn's build looks.
    #[test]
    fn an_added_package_s_library_is_fetched_into_place() {
        use crate::fetch::git::testrepo;
        let tmp = tempfile::tempdir().unwrap();
        let up = |name: &str, files: &[(&str, &str)]| {
            let dir = tmp.path().join("up").join(name);
            testrepo::init(&dir);
            (dir.to_string_lossy().into_owned(), crate::fetch::tests::commit_files(&dir, files).to_string())
        };
        let (kurl, kc) = up("KadathThorn", &[("interface.ccl", "kadath\n"), ("src/build.sh", "cd fuka\n")]);
        let (furl, fc) = up("fuka", &[("Cmake/CMakeLists.txt", "fuka\n")]);
        let library = AddedLibrary { url: furl, commit: fc, ..fuka("KadathThorn", "src/fuka") };
        let package = AddedPackage { url: kurl, commit: kc, ..kadath(vec![library]) };
        let text = compose("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n", &[package]).unwrap();
        let list = thornlist::parse(&text).unwrap();
        let root = tmp.path().join("install");
        let mut progress = prodash::tree::Root::new().add_child("plan");
        let plan = crate::fetch::plan(&list, &root, &mut progress).unwrap();
        assert_eq!(plan.libraries.len(), 1);
        let report = crate::fetch::execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        let cmake = root.join("Cactus/repos/KadathThorn/src/fuka/Cmake/CMakeLists.txt");
        assert_eq!(std::fs::read_to_string(cmake).unwrap(), "fuka\n");
        let thorn = root.join("Cactus/arrangements/Fuka/KadathThorn/src/build.sh");
        assert_eq!(std::fs::read_to_string(thorn).unwrap(), "cd fuka\n");
    }

    /// Review finding 7: clone names are compared without case, as a
    /// case-insensitive filesystem would see them.
    #[test]
    fn library_clone_names_collide_without_case() {
        let e = |libraries: Vec<AddedLibrary>| format!("{:#}", compose(BASE, &[kadath(libraries)]).unwrap_err());
        assert!(e(vec![fuka("KadathThorn", "src/CarpetX")]).contains("would be cloned as repos/CarpetX"));
        let upper = AddedLibrary { package: "et/other".into(), url: "https://x/other".into(), ..fuka("KadathThorn", "src/FUKA") };
        assert!(e(vec![fuka("KadathThorn", "src/fuka"), upper]).contains("both put a library at"));
    }

    /// Review finding 8: narrowing a plan to some repos keeps only the
    /// placements of or into them, never every placement of the list.
    #[test]
    fn a_narrowed_plan_keeps_only_its_own_placements() {
        let placement = |repo: &str, dependent: &str| crate::fetch::library::Placement {
            repo: repo.into(),
            url: format!("https://x/{repo}"),
            dependent: dependent.into(),
            dependent_url: format!("https://x/{dependent}"),
            dependent_commit: None,
            into: format!("src/{repo}"),
        };
        let mut plan = crate::fetch::Plan {
            git: Vec::new(),
            skipped: Vec::new(),
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            libraries: vec![placement("fuka", "KadathThorn"), placement("eigen", "Other"), placement("blas", "carpetx")],
            root: "Cactus".into(),
        };
        narrow(&mut plan, &["carpetx", "fuka"].into_iter().collect());
        let kept: Vec<&str> = plan.libraries.iter().map(|p| p.repo.as_str()).collect();
        assert_eq!(kept, ["fuka", "blas"]);
        narrow(&mut plan, &BTreeSet::new());
        assert!(plan.libraries.is_empty());
    }

    /// Review finding 9: a library's clone name already on disk must be a
    /// clone cactup fetched from that repository, by its fetch record.
    #[test]
    fn a_library_clone_name_taken_on_disk_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repos = tmp.path().join("Cactus/repos");
        let package = kadath(vec![fuka("KadathThorn", "src/fuka")]);
        check_library_dirs(tmp.path(), "Cactus", std::slice::from_ref(&package)).unwrap();
        std::fs::create_dir_all(repos.join("fuka")).unwrap();
        let err = format!("{:#}", check_library_dirs(tmp.path(), "Cactus", std::slice::from_ref(&package)).unwrap_err());
        assert!(err.contains("has no fetch record") && err.contains("move it aside"), "{err}");
        let record = |url: &str| crate::fetch::FetchRecords {
            repos: vec![("fuka".into(), crate::fetch::RepoRecord { url: url.into(), branch: None, head: "b".repeat(40), commit: None })],
            downloads: Vec::new(),
        };
        crate::fetch::FetchState::record(tmp.path(), &record("https://elsewhere/fuka")).unwrap();
        assert!(check_library_dirs(tmp.path(), "Cactus", std::slice::from_ref(&package)).is_err(), "another repository's");
        crate::fetch::FetchState::record(tmp.path(), &record("https://bitbucket.org/fukaws/fuka.git")).unwrap();
        check_library_dirs(tmp.path(), "Cactus", std::slice::from_ref(&package)).unwrap();
    }

    /// Review finding 9: a registry name that would start a clone's name
    /// with `-` is refused.
    #[test]
    fn registry_names_never_start_with_a_dash() {
        assert!(check_registry_names("et/x", "Arr", "Thorn", "../-$2").is_err());
        assert!(check_registry_names("et/x", "-Arr", "Thorn", "$2").is_err());
        check_registry_names("et/x", "Arr", "Thorn", "../$1-$2").unwrap();
    }

    // Command-level: `add`, `remove` and `refetch` taking a dropped
    // placement away end to end, on an installation of real repos.

    /// An installation (alias `t`, active) with et/kadaththorn added and FUKA
    /// placed at `KadathThorn/<into>`, fetched: what `cactup add` leaves.
    /// `base_extra` goes into the base list, and `replaces` into the package.
    struct Installed {
        tmp: tempfile::TempDir,
        ctx: Ctx,
        inst: Installation,
        kadath_url: String,
        fuka_url: String,
        k: String,
        f: String,
    }

    impl Installed {
        fn new(into: &str, base_extra: &str, replaces: &[&str]) -> Installed {
            use crate::fetch::git::testrepo;
            let tmp = tempfile::tempdir().unwrap();
            let up = |name: &str, files: &[(&str, &str)]| {
                let dir = tmp.path().join("up").join(name);
                testrepo::init(&dir);
                (dir.display().to_string(), crate::fetch::tests::commit_files(&dir, files).to_string())
            };
            let (kadath_url, k) = up("KadathThorn", &[("interface.ccl", "kadath\n"), ("src/build.sh", "cd fuka\n")]);
            let (fuka_url, f) = up("fuka", &[("Cmake/CMakeLists.txt", "fuka\n"), ("src/a.cpp", "a\n")]);
            let root = tmp.path().join("inst");
            let db = crate::database::Db::in_dir(&tmp.path().join("db"));
            let path = root.display().to_string();
            db.update(move |d| {
                d.installations.insert(
                    "t".into(),
                    crate::database::CactusInstallation {
                        alias: "t".into(),
                        release: None,
                        path,
                        thornlist: None,
                        simfile: None,
                        stack: None,
                        current_release: None,
                        current_thornlist: None,
                        stack_contact: None,
                        unfetched_repos: Default::default(),
                    },
                );
                d.active_installation = Some("t".into());
                Ok(())
            })
            .unwrap();
            let ctx = Ctx {
                globals: crate::args::GlobalOpts {
                    verbose: false,
                    trace: false,
                    manifest_url: String::new(),
                    mdb_path: None,
                    machine: None,
                    installation: None,
                    hostname: None,
                    knob: Vec::new(),
                    risky_allow_untrusted_artifacts: false,
                },
                db,
            };
            let inst = Installation::resolve(&ctx).unwrap();
            let pristine = format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{base_extra}");
            std::fs::create_dir_all(&root).unwrap();
            std::fs::write(inst.source_thornlist(), &pristine).unwrap();
            let library = AddedLibrary {
                url: fuka_url.clone(),
                commit: f.clone(),
                tag: None,
                ..fuka("KadathThorn", into)
            };
            let package = AddedPackage {
                url: kadath_url.clone(),
                commit: k.clone(),
                tag: None,
                replaces: replaces.iter().map(|r| (*r).to_owned()).collect(),
                ..kadath(vec![library])
            };
            let locked = inst.locked().unwrap();
            let mut meta = locked.meta().unwrap();
            meta.added = vec![package];
            locked.set_meta(&meta).unwrap();
            drop(locked);
            let composed = compose(&pristine, &meta.added).unwrap();
            let it = Installed { tmp, ctx, inst, kadath_url, fuka_url, k, f };
            it.fetch(&composed);
            std::fs::create_dir_all(it.inst.live_thornlist().parent().unwrap()).unwrap();
            std::fs::write(it.inst.live_thornlist(), &composed).unwrap();
            it
        }

        fn fetch(&self, text: &str) {
            let list = thornlist::parse(text).unwrap();
            let mut progress = prodash::tree::Root::new().add_child("plan");
            let plan = crate::fetch::plan(&list, &self.inst.root, &mut progress).unwrap();
            let report = crate::fetch::execute(&plan, &self.inst.root).unwrap();
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            crate::fetch::FetchState::record(&self.inst.root, &report.records()).unwrap();
        }

        fn kadath_dir(&self) -> std::path::PathBuf {
            self.inst.root.join("Cactus/repos/KadathThorn")
        }

        fn exclude(&self) -> String {
            std::fs::read_to_string(self.kadath_dir().join(".git/info/exclude")).unwrap_or_default()
        }
    }

    /// `cactup remove` refused (a base repo it replaced has local edits, so it
    /// cannot go back): the package stays added, and its library's link and
    /// exclude line stay with it.
    #[test]
    fn a_refused_remove_leaves_the_library_placed() {
        use crate::fetch::git::testrepo;
        let other_up = tempfile::tempdir().unwrap();
        testrepo::init(other_up.path());
        let c = crate::fetch::tests::commit_files(other_up.path(), &[("Oth/Thorn/interface.ccl", "x\n")]);
        let base = format!(
            "!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {}\n!NAME = Other\n!REPO_PATH = $1/$2\n!REPO_COMMIT = {c}\n\
             !CHECKOUT =\nOth/Thorn\n",
            other_up.path().display()
        );
        let it = Installed::new("src/fuka", &base, &["Other"]);
        let link = it.kadath_dir().join("src/fuka");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        std::fs::write(it.inst.root.join("Cactus/repos/Other/Oth/Thorn/interface.ccl"), "edited\n").unwrap();

        let err = format!("{:#}", remove(&it.ctx, "et/kadaththorn", false).unwrap_err());
        assert!(err.contains("is still added"), "{err}");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink(), "the link stays");
        assert!(it.exclude().contains("/src/fuka"), "and its exclude line");
        assert_eq!(it.inst.meta().unwrap().added.len(), 1);
    }

    /// `cactup remove` done: the link and its exclude line are gone (the
    /// exclude's other lines kept), and FUKA's clone stays for prune.
    #[test]
    fn a_remove_takes_the_library_link_and_its_exclude_line_away() {
        let it = Installed::new("src/fuka", "", &[]);
        let exclude = it.kadath_dir().join(".git/info/exclude");
        let before = std::fs::read_to_string(&exclude).unwrap();
        assert!(before.contains("/src/fuka"));
        remove(&it.ctx, "et/kadaththorn", false).unwrap_or_else(|e| panic!("{e:#}"));
        assert!(std::fs::symlink_metadata(it.kadath_dir().join("src/fuka")).is_err());
        let after = std::fs::read_to_string(&exclude).unwrap();
        assert!(!after.contains("/src/fuka") && !after.contains("cactup placed"), "{after}");
        assert_eq!(after, before.replace(&format!("{}\n/src/fuka\n", crate::fetch::library::EXCLUDE_COMMENT), ""));
        assert!(it.inst.root.join("Cactus/repos/fuka/src/a.cpp").is_file(), "the clone stays for prune");
        assert!(it.inst.meta().unwrap().added.is_empty());
    }

    /// `cactup inst refetch` to a new base that carries FUKA itself, the
    /// Einstein Toolkit's way (items linked through KadathThorn's
    /// arrangement link into `src/fuka`): the package's own placement is
    /// dropped and its link taken away first, so the base's sections land
    /// in a real `src/fuka` inside the installation.
    #[test]
    fn a_refetch_to_a_base_that_carries_the_library_takes_the_link_away() {
        let it = Installed::new("src/fuka", "", &[]);
        let new_base = format!(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!DEFINE ARR = $ROOT/arrangements\n\n\
             !TARGET = $ARR/Fuka/KadathThorn/src/fuka/../../../../repos/KadathThorn/src/fuka\n!TYPE = git\n!URL = {}\n\
             !REPO_COMMIT = {}\n!CHECKOUT = Cmake src\n",
            it.fuka_url, it.f
        );
        let file = it.tmp.path().join("new-base.th");
        std::fs::write(&file, &new_base).unwrap();
        let args = match <crate::args::Args as clap::Parser>::try_parse_from([
            "cactup",
            "inst",
            "refetch",
            file.to_str().unwrap(),
        ])
        .unwrap()
        .command
        {
            crate::args::Commands::Inst(crate::args::InstallationCommand::Refetch(args)) => args,
            other => panic!("{other:?}"),
        };
        super::super::refetch::dispatch(&it.ctx, args).unwrap_or_else(|e| panic!("{e:#}"));
        let dir = it.kadath_dir().join("src/fuka");
        let meta = std::fs::symlink_metadata(&dir).unwrap();
        assert!(meta.is_dir() && !meta.file_type().is_symlink(), "the base's own directory now");
        assert_eq!(std::fs::read_to_string(dir.join("Cmake/CMakeLists.txt")).unwrap(), "fuka\n");
        assert!(!it.exclude().contains("/src/fuka"), "{}", it.exclude());
        assert!(!it.tmp.path().join("repos").exists(), "nothing outside the installation");
        let _ = (&it.kadath_url, &it.k);
    }

    /// `cactup add` again, at a version whose dependency puts FUKA at
    /// `ext/fuka` instead: the old link and exclude line go, the new ones
    /// come.
    #[test]
    fn an_add_that_moves_the_library_takes_the_old_link_away() {
        let it = Installed::new("src/fuka", "", &[]);
        let facts = serde_json::to_string(&cactup_ecosystem_common::ccl::ThornFacts::default()).unwrap();
        let info = |version: &str, commit: &str| {
            format!(
                r#""version":"{version}","track":null,"tag":null,"commit":"{commit}","tree":null,"yanked":false,
                "yank-reason":null,"state":"ok","curated":false,"published":0"#
            )
        };
        let (kurl, furl) = (it.kadath_url.clone(), it.fuka_url.clone());
        let resolve_kadath = format!(
            r#"{{"slug":"et/kadaththorn@2026.6.0","kind":"package","url":"{kurl}","contact":null,{}}}"#,
            info("2026.6.0", &it.k)
        );
        let kadath_versions = format!(
            r#"{{"slug":"et/kadaththorn","versions":[{{{},"url":"{kurl}",
            "thorns":[{{"arrangement":"Fuka","thorn":"KadathThorn","repo-path":"../$2","facts":{facts}}}],
            "claims":[{{"kind":"dependency","target":"et/fuka","range":"^2026.5.0","into":"ext/fuka","note":null}}]}}]}}"#,
            info("2026.6.0", &it.k)
        );
        let resolve_fuka = format!(
            r#"{{"slug":"et/fuka@2026.5.0","kind":"package","url":"{furl}","contact":null,{}}}"#,
            info("2026.5.0", &it.f)
        );
        let fuka_versions = format!(
            r#"{{"slug":"et/fuka","versions":[{{{},"url":"{furl}","library":true,"thorns":[],"claims":[]}}]}}"#,
            info("2026.5.0", &it.f)
        );
        let (base, _) = crate::fetch::download::test_server::serve_with(4, move |path| {
            let body = match path {
                p if p.contains("/resolve") && p.contains("kadaththorn") => resolve_kadath.clone(),
                p if p.contains("/resolve") => resolve_fuka.clone(),
                p if p.contains("/packages/et/kadaththorn/") => kadath_versions.clone(),
                _ => fuka_versions.clone(),
            };
            ("200 OK", "api-generation: 1\r\n".into(), body.into_bytes())
        });
        it.ctx
            .db
            .update(move |d| {
                d.set_knob(crate::cactarium::URL_KNOB, base);
                Ok(())
            })
            .unwrap();
        dispatch(&it.ctx, &["et/kadaththorn".to_owned()], false, false).unwrap_or_else(|e| panic!("{e:#}"));
        assert!(std::fs::symlink_metadata(it.kadath_dir().join("src/fuka")).is_err(), "the old link is gone");
        assert_eq!(std::fs::read_link(it.kadath_dir().join("ext/fuka")).unwrap(), std::path::Path::new("../../fuka"));
        let exclude = it.exclude();
        assert!(!exclude.contains("/src/fuka") && exclude.contains("/ext/fuka"), "{exclude}");
        assert_eq!(it.inst.meta().unwrap().added[0].libraries[0].into, "ext/fuka");
    }
}
