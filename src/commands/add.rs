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
///
/// A package that only adds thorns to a clone the base already has (the
/// same repository, replacing nothing) follows the base's source for that
/// clone, branch and pin: its thorns live at whatever commit the base has
/// the clone at, so a refetch that moves the base moves them too, and one
/// clone keeps one source.
///
/// A flesh package takes the place of the base's flesh, whatever its name or
/// URL: every component of the base's flesh repos is dropped, and the
/// package's section checks out what the base's flesh did into the root.
pub fn compose(base: &str, added: &[AddedPackage]) -> Res<String> {
    if added.is_empty() {
        return Ok(base.to_owned());
    }
    let list = thornlist::parse(base).context("The base thornlist does not parse")?;
    let replaced: BTreeSet<&str> = added.iter().flat_map(|a| a.replaces.iter().map(String::as_str)).collect();
    let claimed: BTreeSet<&str> =
        added.iter().flat_map(|a| a.thorns.iter().map(|t| t.checkout.as_str())).collect();
    let flesh_gone = flesh_replaced(&list, added);
    let drop: BTreeSet<&str> = list
        .components()
        .iter()
        .filter(|c| {
            (replaced.contains(c.repo.as_str()) && claimed.contains(c.checkout.as_str()))
                || flesh_gone.contains(c.repo.as_str())
        })
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
            let follows = base_clone_of(a, &list).map(|c| (c.url.clone(), c.branch.clone(), c.commit.clone()));
            // The flesh's items, checked out straight into the root, never built.
            let commit = (!is_tip(&a.version)).then(|| a.commit.clone());
            let flesh = a.flesh.iter().map(move |item| Component {
                ty: ComponentType::Git,
                target: root.to_owned(),
                checkout: item.clone(),
                name: Some(a.repo.clone()),
                url: Some(a.url.clone()),
                auth_url: None,
                anon_user: None,
                anon_pass: None,
                repo_path: None,
                branch: a.tag.clone(),
                commit: commit.clone(),
                checksum: None,
                fetch_only: true,
                repo: a.repo.clone(),
            });
            flesh.chain(a.thorns.iter().map(move |t| {
                let repo = clone_of(&a.repo, t);
                // A branch tip follows its default branch (named in `tag`):
                // no pin, so a refetch moves it with the tip.
                let tip = is_tip(&a.version);
                let (url, branch, commit) = match &follows {
                    Some(source) => source.clone(),
                    None => (Some(a.url.clone()), a.tag.clone(), (!tip).then(|| a.commit.clone())),
                };
                Component {
                    ty: ComponentType::Git,
                    target: format!("{root}/arrangements"),
                    checkout: t.checkout.clone(),
                    name: Some(repo.clone()),
                    url,
                    auth_url: None,
                    anon_user: None,
                    anon_pass: None,
                    repo_path: (!t.repo_path.is_empty()).then(|| t.repo_path.clone()),
                    branch,
                    commit,
                    checksum: None,
                    fetch_only: false,
                    repo,
                }
            }))
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

/// The base's clone an added package only adds thorns to, when it is one: a
/// package that replaces nothing, whose clone the base list has under the
/// same name, from the same repository (compared as Cactarium compares
/// them). Its thorns live in that clone, at whatever the base has it at:
/// `compose` writes them with the base's source for it (URL, branch, pin),
/// and `present` reads them there too (as the base package's, when the
/// lookup says the clone is that package). A package from another repository
/// that happens to share the clone's name is no such thing: it keeps its own
/// source, and the fetch planner's conflict over the name says so loudly.
/// Libraries are never placed through one (a plan places libraries only for
/// a release it brings in or moves, and more thorns of a base package move
/// nothing).
pub(crate) fn base_clone_of<'l>(a: &AddedPackage, base: &'l thornlist::Thornlist) -> Option<&'l Component> {
    if !a.replaces.is_empty() || a.is_flesh() {
        return None;
    }
    base.components().iter().find(|c| {
        c.repo == a.repo
            && c.ty == ComponentType::Git
            && c.url.as_deref().is_some_and(|u| cactup_ecosystem_common::url::same_repo_lenient(u, &a.url))
    })
}

/// The base repos that are the Cactus flesh: each with a component for which
/// [`crate::fetch::is_flesh`] holds. A flesh package takes the place of every
/// component of each.
pub(crate) fn base_flesh_repos(base: &thornlist::Thornlist) -> BTreeSet<&str> {
    let flesh = |c: &&Component| crate::fetch::is_flesh(base.root(), c);
    base.components().iter().filter(flesh).map(|c| c.repo.as_str()).collect()
}

/// The base's flesh repos when one of `added` is a flesh package (it takes
/// their place); none otherwise.
pub(crate) fn flesh_replaced<'l>(base: &'l thornlist::Thornlist, added: &[AddedPackage]) -> BTreeSet<&'l str> {
    match added.iter().any(AddedPackage::is_flesh) {
        true => base_flesh_repos(base),
        false => BTreeSet::new(),
    }
}

/// What the base's flesh checks out into the Cactus root, in list order: what
/// a flesh package checks out in its place.
fn base_flesh_items(base: &thornlist::Thornlist) -> Vec<String> {
    let repos = base_flesh_repos(base);
    let root = base.root().trim_end_matches('/');
    let mut items: Vec<String> = Vec::new();
    for c in base.components() {
        let at_root = c.target.trim_end_matches('/') == root;
        if repos.contains(c.repo.as_str()) && at_root && !items.contains(&c.checkout) {
            items.push(c.checkout.clone());
        }
    }
    items
}

/// Warnings for packages that only added thorns to a base clone the new
/// base (`new`, after a refetch) no longer has: they are now fetched on
/// their own, at the commit they were added at, which nobody chose.
pub(crate) fn extensions_left_behind(
    old: &thornlist::Thornlist,
    new: &thornlist::Thornlist,
    added: &[AddedPackage],
) -> Vec<String> {
    (added.iter())
        .filter(|a| base_clone_of(a, old).is_some() && base_clone_of(a, new).is_none())
        .map(|a| {
            let thorns: Vec<&str> = a.thorns.iter().map(|t| t.checkout.as_str()).collect();
            format!(
                "{} added thorns {} to base repo {}, which the new base lacks.\nThey are now fetched on their own, \
                 at {}.\nRemove it with `cactup remove {}`, then add again what needs them.",
                a.package,
                thorns.join(", "),
                a.repo,
                &a.commit[..12.min(a.commit.len())],
                a.package
            )
        })
        .collect()
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
                bail!("{}'s library {} goes inside \"{}\", which is not a clone name", a.package, l.package, l.inside);
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
                    "{other} and {} both put a library at {path} ({} and {})",
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
                    "{}'s library {} and {}'s library {} both go in {}/{dir}; one library per directory",
                    other, them.package, a.package, l.package, l.inside
                );
            }
            let source = (normalized(&l.url), Some(l.commit.clone()), l.package.clone());
            match sources.get(&fold(name)) {
                Some((url, commit, who)) if (url, commit) != (&source.0, &source.1) => bail!(
                    "{}'s library {} would be cloned as repos/{name}, which {who} already uses for another source",
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
/// `!REPO_PATH` written for it, verbatim (Cactarium derives it from where
/// the thorn sits in the repository, or it is the base list's own, when the
/// base already clones the repository). GetComponents links the checkout to
/// `repos/<clone>/<REPO_PATH>`, so a path that starts `../<name>` leaves the
/// clone and comes back in through `<name>`: the clone must then be named
/// that, `$1`/`$2` filled in from the checkout, as for the Einstein
/// Toolkit's ExternalLibraries (`../$1-$2`). Any other path is inside the
/// package's own clone, `package_repo`.
pub(crate) fn clone_of(package_repo: &str, t: &crate::installation::AddedThorn) -> String {
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
    // Compared as Cactarium compares repositories: a forge that ignores
    // path case is one repository however it is spelled.
    let same_repo = |u: &str| cactup_ecosystem_common::url::same_repo_lenient(u, url);
    let Some(same) = base.components().iter().find(|c| c.url.as_deref().is_some_and(same_repo)) else {
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
/// base has them, so the package must match their source. `slug` names the
/// package in the refusal.
fn share_base_clone(base: &thornlist::Thornlist, repo: &str, slug: &str, w: &mut AddedPackage) -> Res<()> {
    // One clone is at one commit: the base's other checkouts of it
    // stay at the base's pin, so the package must be at the same one. A
    // flesh package takes every checkout of the base's flesh.
    let claimed: BTreeSet<&str> = w.thorns.iter().map(|t| t.checkout.as_str()).collect();
    let flesh = if w.is_flesh() { base_flesh_repos(base) } else { BTreeSet::new() };
    let stay: Vec<&thornlist::Component> = (base.components().iter())
        .filter(|c| c.repo == repo && !claimed.contains(c.checkout.as_str()) && !flesh.contains(c.repo.as_str()))
        .collect();
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
            "{slug} would share the clone {repo} with {}, which the base keeps at {}.\nAdd a version at that \
             commit, or replace those thorns too.",
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

/// The names given to a thorn by Cactarium become paths on disk and words in a
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
        bail!("Cactarium names a thorn of {slug} \"{arrangement}/{thorn}\", which is not a plain Arrangement/Thorn");
    }
    // `..` only as the leading `../<clone>` that reaches another clone by
    // name; anywhere else it would climb out of the clone (or of `repos/`).
    let segments: Vec<&str> = repo_path.split('/').collect();
    let dots_ok = segments.iter().enumerate().all(|(i, seg)| *seg != ".." || (i == 0 && segments.len() > 1 && segments[1] != ".."));
    let path_ok = dots_ok && repo_path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-/$".contains(&b));
    let checkout = format!("{arrangement}/{thorn}");
    let clone = clone_of("package", &crate::installation::AddedThorn { checkout, repo_path: repo_path.to_owned() });
    if !path_ok || !plain(&clone) {
        bail!("Cactarium gives {arrangement}/{thorn} of {slug} the repo-path \"{repo_path}\", which cactup will not write");
    }
    Ok(())
}

/// The source given to a package by Cactarium goes into its thornlist section,
/// which GetComponents hands to a shell (`git clone --branch '<label>'`,
/// `git checkout <label>` unquoted): the URL holds no whitespace, quote or
/// control character, the commit is hex, and the label (the version's tag,
/// or a tip's default branch, which the package's author controls) is plain
/// ref characters.
fn check_registry_source(slug: &str, url: &str, commit: &str, label: Option<&str>) -> Res<()> {
    if url.is_empty() || url.chars().any(|c| c.is_whitespace() || c.is_control() || "'\"`$;\\".contains(c)) {
        bail!("Cactarium gives {slug} the repository \"{}\", which cactup will not write", url.escape_debug());
    }
    if commit.is_empty() || !commit.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("Cactarium gives {slug} the commit \"{}\", which is not a commit id", commit.escape_debug());
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

/// Every clone a package is fetched into: its own, for a flesh package.
fn package_repos(a: &AddedPackage) -> BTreeSet<String> {
    let mut repos: BTreeSet<String> = a.thorns.iter().map(|t| clone_of(&a.repo, t)).collect();
    if a.is_flesh() {
        repos.insert(a.repo.clone());
    }
    repos
}

/// Narrow `plan` to `repos`: their git work and links, and only the library
/// placements of or into them (never every placement of the list); no
/// downloads, externals or skips. Nothing else in the installation moves.
fn narrow(plan: &mut crate::fetch::Plan, repos: &BTreeSet<&str>) {
    narrow_linking(plan, repos, &BTreeSet::new());
}

/// [`narrow`], where the clones `linked` get their links only: no fetch,
/// pin or fast-forward of the clone (more thorns of a base package live at
/// whatever commit the base has its clone at).
fn narrow_linking(plan: &mut crate::fetch::Plan, repos: &BTreeSet<&str>, linked: &BTreeSet<&str>) {
    plan.git.retain(|g| repos.contains(g.repo.as_str()) && !linked.contains(g.repo.as_str()));
    plan.links.retain(|c| repos.contains(c.repo.as_str()) || linked.contains(c.repo.as_str()));
    plan.libraries.retain(|p| repos.contains(p.repo.as_str()) || repos.contains(p.dependent.as_str()));
    plan.downloads.clear();
    plan.external.clear();
    plan.skipped.clear();
}

/// What restoring the base repos `touched` (that removed packages replaced)
/// takes, or a refusal when one is left alone; `None` with nothing to
/// restore.
fn restore_plan(
    inst: &crate::installation::Installation,
    composed: &str,
    touched: &BTreeSet<&str>,
    gone: &[String],
    removed: &[AddedPackage],
) -> Res<Option<crate::fetch::Plan>> {
    if touched.is_empty() {
        return Ok(None);
    }
    let list = thornlist::parse(composed)?;
    let (progress, renderer) = crate::manifest::setup_prodash();
    let mut classify = progress.add_child("classify repos");
    // Only the clones being restored: no other is looked at, nor its remote asked.
    let fetch_plan = crate::fetch::plan_only(&list, &inst.root, &mut classify, Some(touched));
    drop(classify);
    renderer.shutdown_and_wait();
    let mut fetch_plan = fetch_plan?;
    // A clone cactup itself left detached, at the commit a removed package
    // that replaced it in the base's own clone was pinned at: cactup's state
    // to move back, onto the base's pin or its branch, as for a fresh base.
    // Detached anywhere else, local changes, local commits: the user's.
    let ours = |s: &crate::fetch::SkippedRepo| {
        matches!(&s.reason, crate::fetch::git::DirtyReason::DetachedHead(head)
            if removed.iter().any(|a| a.replaces.contains(&s.repo) && a.commit == head.to_string()))
    };
    // Where one that follows a branch goes is the fetch's own answer (`branch_to_follow`), as
    // for any clone cactup left detached; it is what `returning_base` read the base at.
    let back = fetch_plan.force_where(|s| touched.contains(s.repo.as_str()) && ours(s))?;
    for g in fetch_plan.git.iter_mut().filter(|g| back.contains(&g.repo)) {
        g.forced = None;
    }
    // Any clone being restored that no branch can be found for (forced or planned): refused here,
    // before anyone is asked, never left to fail the fetch after.
    let lost = |g: &&crate::fetch::GitRepoPlan| {
        touched.contains(g.repo.as_str()) && g.commit.is_none() && g.branch.is_none() && !matches!(g.action, crate::fetch::GitAction::Clone)
    };
    if let Some(g) = fetch_plan.git.iter().find(lost) {
        bail!(
            "Repo {} has no branch to go back to ({}).\n{} is still added.",
            g.repo,
            g.note.as_deref().unwrap_or("nothing names one"),
            gone.join(", ")
        );
    }
    if let Some(skipped) = fetch_plan.skipped.iter().find(|s| touched.contains(s.repo.as_str())) {
        bail!(
            "Repo {} has {}.\n{} is still added.",
            skipped.repo,
            skipped.reason.describe(),
            gone.join(", ")
        );
    }
    narrow(&mut fetch_plan, touched);
    Ok(Some(fetch_plan))
}

#[cfg(test)]
thread_local! {
    /// What a test does to the installation while the prompt waits (`add`).
    pub(crate) static AFTER_PROMPT: std::cell::Cell<Option<fn(&std::path::Path)>> = const { std::cell::Cell::new(None) };
}

/// What restoring the base repos did, one line each: back at the base's
/// pin, or on the branch a base that follows one has.
fn restored_lines(plan: &crate::fetch::Plan) -> Vec<String> {
    // A clone made again says so on its own line.
    (plan.git.iter())
        .filter(|g| !matches!(g.action, crate::fetch::GitAction::Clone))
        .map(|g| match (&g.commit, &g.branch) {
            (None, Some(branch)) => format!("Restored {} to the base's branch {branch}.", g.repo),
            _ => format!("Restored {} to the base's commit.", g.repo),
        })
        .collect()
}

/// A refusal of the fetch made under the lock, as the clones changing while
/// the plan waited.
fn clones_changed(e: anyhow::Error) -> anyhow::Error {
    anyhow!("The installation's clones changed while the plan waited ({e:#}).\nRun the command again.")
}

/// Under the lock: the fetch made now must be the one checked before the
/// prompt. Nothing re-probes the clones once it runs (and a checkout
/// overwrites), so a clone edited, committed in or moved while the question
/// waited, or by a concurrent `inst refetch`, refuses here, as `recheck`
/// refuses a changed record.
fn same_fetch(checked: &crate::fetch::Plan, now: &crate::fetch::Plan) -> Res<()> {
    // Everything the fetch does; not a note's words (a remote that failed to answer in other
    // words the second time changed no clone), nor whether its ask went silent (the second
    // plan may find the window already up, and either way the clone is left as it is).
    let shape = |p: &crate::fetch::Plan| {
        let git: Vec<String> = (p.git.iter())
            .map(|g| {
                let what = (&g.repo, &g.dir, &g.url, &g.branch, &g.commit, &g.action, &g.checkouts, &g.forced, &g.retarget);
                format!("{what:?} noted: {}", g.note.is_some())
            })
            .collect();
        format!("{git:?} {:?} {:?} {:?} {:?} {:?} {:?}", p.skipped, p.downloads, p.external, p.links, p.libraries, p.root)
    };
    if shape(checked) != shape(now) {
        bail!("The installation's clones changed while the plan waited.\nRun the command again.");
    }
    Ok(())
}

/// What fetching the `composed` list takes for `added`: those repos (each
/// package's own, and its libraries'), never the rest of the installation,
/// and only links into a base clone a package gives more thorns. Every
/// refusal it can make (a library directory that is another repository's, a
/// repo the fetch would leave alone) comes here, before anyone is asked
/// about the plan. A base clone that is only linked into is never fetched or
/// pinned, so a detached HEAD or local changes there are no refusal.
fn fetch_plan_for(
    inst: &crate::installation::Installation,
    composed: &str,
    base: &thornlist::Thornlist,
    added: &[AddedPackage],
) -> Res<crate::fetch::Plan> {
    let repos: BTreeSet<String> = added.iter().flat_map(all_repos).collect();
    let repos: BTreeSet<&str> = repos.iter().map(String::as_str).collect();
    let list = thornlist::parse(composed)?;
    check_library_dirs(&inst.root, list.root(), added)?;
    // More thorns of a base package: links into the base's clone, as it is.
    let linked: BTreeSet<&str> =
        added.iter().filter_map(|a| base_clone_of(a, base)).map(|c| c.repo.as_str()).collect();
    let (progress, renderer) = crate::manifest::setup_prodash();
    let mut classify = progress.add_child("classify repos");
    // Only the clones the packages touch: no other is looked at, nor its remote asked.
    let only: BTreeSet<&str> = repos.union(&linked).copied().collect();
    let fetch_plan = crate::fetch::plan_only(&list, &inst.root, &mut classify, Some(&only));
    drop(classify);
    renderer.shutdown_and_wait();
    let mut fetch_plan = fetch_plan?;
    let fetched = |r: &str| repos.contains(r) && !linked.contains(r);
    if let Some(skipped) = fetch_plan.skipped.iter().find(|s| fetched(s.repo.as_str())) {
        bail!("Repo {} in the installation has {}; it was left alone", skipped.repo, skipped.reason.describe());
    }
    narrow_linking(&mut fetch_plan, &repos, &linked);
    Ok(fetch_plan)
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
                    "{}'s library {} would be cloned at '{}', which holds a clone of {}, not {}.\nMove it aside and \
                     rerun the add.",
                    a.package,
                    l.package,
                    dir.display(),
                    r.url,
                    l.url
                ),
                None => bail!(
                    "{}'s library {} would be cloned at '{}', which exists and has no fetch record.\nMove it aside \
                     (or delete it if it is an unneeded checkout of {}), then rerun the add.",
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
            "Cannot place the libraries of {}: its thorns come from several clones ({})",
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
                "{}'s library {} stays where the installation's thornlist puts it (as {}).",
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

/// The flags of `cactup add`.
#[derive(Clone, Debug, Default)]
pub struct AddFlags {
    /// `--replace`: a package whose thorns collide with a base repo's takes
    /// their place.
    pub replace: bool,
    /// `-f`: overwrite hand edits of the live thornlist.
    pub force: bool,
    /// `--upgrade`: a present package the plan touches may move.
    pub upgrade: bool,
    /// `--upgrade-all`: every present package moves to its newest release
    /// that fits.
    pub upgrade_all: bool,
    /// `--no-deps`: nothing beyond the packages named.
    pub no_deps: bool,
    /// `--ignore-claim`: claims to leave out, this command only.
    pub ignore_claim: Vec<String>,
    /// `--yes`: no question asked.
    pub yes: bool,
    /// `--dry-run`: show the plan, change nothing.
    pub dry_run: bool,
}

/// The flags of `cactup remove`.
#[derive(Clone, Debug, Default)]
pub struct RemoveFlags {
    pub force: bool,
    /// `--no-deps`: remove exactly what was named, asking nobody what
    /// depends on it, and leave what it alone needed.
    pub no_deps: bool,
    pub yes: bool,
    pub dry_run: bool,
}

/// What `add` and `remove` start from: the installation's record and both
/// thornlists, read without the lock (the plan is made, shown and confirmed
/// first), then checked again under it.
struct Start {
    inst: Installation,
    meta: crate::installation::InstallationMeta,
    pristine: String,
    live_path: std::path::PathBuf,
    /// The live list as read.
    live: String,
    /// The live list as cactup composed it.
    before: String,
}

/// Which command a [`Start`] is for: its refusals' words.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Doing {
    Adding,
    Removing,
}

impl Start {
    fn read(ctx: &Ctx, force: bool, doing: Doing) -> Res<Start> {
        let inst = Installation::resolve(ctx)?;
        let meta = inst.meta()?;
        let pristine = std::fs::read_to_string(inst.source_thornlist_to_read()).context(match doing {
            Doing::Adding => "This installation has no pristine thornlist to add to",
            Doing::Removing => "This installation has no pristine thornlist",
        })?;
        let live_path = inst.live_thornlist()?;
        let live = std::fs::read_to_string(inst.live_thornlist_to_read()?).unwrap_or_default();
        // The live list must be what cactup composed, or the command would
        // silently discard edits made to it by hand.
        let before = compose(&pristine, &meta.added)?;
        let edits = super::refetch::component_divergence(&live, &before);
        if !edits.is_empty() && !force {
            match doing {
                Doing::Adding => bail!(
                    "'{}' has hand edits, which adding would overwrite:\n  {}\nMove them into a thornlist of your own, or pass -f",
                    live_path.display(),
                    edits.join("\n  ")
                ),
                Doing::Removing => bail!(
                    "'{}' has hand edits, which removing would overwrite:\n  {}\nPass -f to discard them",
                    live_path.display(),
                    edits.join("\n  ")
                ),
            }
        }
        Ok(Start { inst, meta, pristine, live_path, live, before })
    }

    /// Under the lock: the record and both thornlists as they were when the
    /// plan was made, or a refusal (another command, or a hand edit made
    /// while the question waited, changed them meanwhile).
    fn recheck(&self, locked: &crate::installation::LockedInstallation) -> Res<crate::installation::InstallationMeta> {
        let meta = locked.meta()?;
        let pristine = std::fs::read_to_string(self.inst.source_thornlist_to_read()).unwrap_or_default();
        let live = std::fs::read_to_string(self.inst.live_thornlist_to_read()?).unwrap_or_default();
        if meta.added != self.meta.added
            || meta.added_for != self.meta.added_for
            || pristine != self.pristine
            || live != self.live
        {
            bail!("The installation changed while the plan was being made.\nRun the command again.");
        }
        Ok(meta)
    }
}

/// Cactarium's error, or the refusal of a command that cannot go on
/// without it.
fn needs_the_cactarium(e: anyhow::Error, way: &str) -> anyhow::Error {
    if gix::interrupt::is_triggered() {
        return e;
    }
    anyhow!("{e:#}\n{way}")
}

/// One line for each present package whose release Cactarium cannot
/// give, for a refusal.
fn unread_lines(unread: &[&super::resolution::Unread]) -> String {
    unread.iter().map(|u| format!("\n  {}", u.line())).collect()
}

/// The plan for adding `specs` to an installation (its base list `base`,
/// its record `meta`, its thorns as read from the tree `local`), and the
/// installation as the plan saw it.
fn plan_add(
    client: &crate::cactarium::Client,
    install_root: &std::path::Path,
    base: &thornlist::Thornlist,
    meta: &crate::installation::InstallationMeta,
    local: Option<&super::resolution::LocalFacts>,
    specs: &[String],
    flags: &AddFlags,
) -> Res<(crate::deps::Plan, Vec<crate::deps::Present>, std::collections::BTreeMap<String, String>)> {
    use crate::deps::{self, ClaimPattern, NamedSpec, Options, Request, Rules, Step};
    use super::resolution::{PresentOptions, Registry};
    let named: Vec<NamedSpec> = specs.iter().map(|s| NamedSpec::parse(s)).collect::<Res<_>>()?;
    let ignore: Vec<ClaimPattern> = (flags.ignore_claim.iter())
        .map(|s| s.parse::<ClaimPattern>().with_context(|| format!("--ignore-claim {s}")))
        .collect::<Res<_>>()?;
    let names: Vec<String> = (named.iter().map(|s| s.id.to_string()))
        .chain(meta.added.iter().map(|a| a.package.clone()))
        .chain(base.components().iter().map(|c| c.repo.clone()))
        .collect();
    let mut reg = Registry::new(client, names.iter().map(String::as_str));
    // `/resolve` first, for Cactarium's own word on a name that is no
    // package (a stack, a sim, a typo).
    let asked: Vec<(String, String)> = (specs.iter().zip(&named))
        .map(|(spec, n)| {
            let slug: Slug = spec.parse().expect("parsed above");
            let req = match (&slug.at, slug.sha()) {
                (Some(at), _) => format!("{}@{at}", n.id),
                (None, Some(sha)) => format!("{}#{sha}", n.id),
                (None, None) => n.id.to_string(),
            };
            (n.id.to_string(), req)
        })
        .collect();
    reg.check_named(&asked)?;
    // Without --no-deps an older Cactarium (no lookup) cannot map the base;
    // with it, the base is taken as repos of no known package, as before
    // dependencies were resolved.
    let options = PresentOptions {
        full: flags.upgrade || flags.upgrade_all,
        without_lookup: flags.no_deps,
        no_base_reads: flags.no_deps,
    };
    let set = match super::resolution::present(&mut reg, install_root, base, &meta.added, local, &options) {
        Ok(set) => set,
        Err(e) if e.downcast_ref::<crate::cactarium::OldCactarium>().is_some() => {
            bail!("{e:#}.\nPass --no-deps to add only the packages named, without their dependencies.")
        }
        Err(e) => return Err(e),
    };
    // Claim overrides kept in the installation join these with `cactup claims` (P4).
    let rules = Rules { ignore, overrides: Vec::new() };
    let options =
        Options { upgrade: flags.upgrade, upgrade_all: flags.upgrade_all, no_deps: flags.no_deps, replace: flags.replace };
    let request = Request { specs: &named, present: &set.present, rules: &rules, options };
    let plan = deps::resolve(&request, &mut reg);
    // A base package the resolver read at its commit is checked after all.
    let mut set = set;
    set.forget_read(&reg);
    let finished = reg.finish();
    let plan = plan.map_err(|e| super::resolution::refusal_text(e, &finished))?;
    // An added package whose release cannot be read hides what it depends
    // on: no present package moves beside it, unless --no-deps says the
    // dependencies are the user's business. One the plan itself moves,
    // re-adds or replaces hides nothing any more.
    let renewed: Vec<&deps::PkgId> = (plan.steps.iter())
        .filter_map(|s| match s {
            Step::Upgrade { to, .. } => Some(&to.id),
            Step::Readd { release, .. } => Some(&release.id),
            Step::Replace { replaced: deps::Replaced::Added { id }, .. } => Some(id),
            _ => None,
        })
        .collect();
    let moved = moves_of(&plan);
    // Base packages whose dependencies are unknown are normal (a base that
    // follows branches): said once, and only when the plan moves something
    // they might need where it is.
    let moved_ids: Vec<&str> = moved.iter().map(|m| m.split(' ').next().unwrap_or(m)).collect();
    let unchecked = still_unchecked(&set.unchecked, &moved_ids);
    if unchecked > 0 && !moved.is_empty() {
        println!("{}", unchecked_line(unchecked, &format!("moving {}", moved.join(", "))).yellow());
    }
    let unread: Vec<&super::resolution::Unread> = set.unread.iter().filter(|u| !renewed.contains(&&u.id)).collect();
    if !unread.is_empty() {
        let moves = flags.upgrade || flags.upgrade_all || !moved.is_empty();
        if moves && !flags.no_deps {
            bail!(
                "Cannot move installed packages beside these, whose dependencies are unknown:{}\nAdd each again at \
                 a version served by Cactarium, or pass --no-deps.",
                unread_lines(&unread)
            );
        }
        let labels: Vec<&str> = unread.iter().map(|u| u.label.as_str()).collect();
        println!(
            "{}",
            format!("Not checked: what {} depend on (Cactarium cannot read them).", labels.join(", ")).yellow()
        );
    }
    Ok((plan, set.present, set.heads))
}

/// The `[[added]]` entries giving a base repo's package more thorns, brought
/// in rather than named, of which `specs` name the package whole or one of
/// those thorns.
fn named_extensions(
    specs: &[String],
    present: &[crate::deps::Present],
    meta: &crate::installation::InstallationMeta,
) -> Vec<String> {
    let believed = super::resolution::added_for(meta);
    let mut out = Vec::new();
    for spec in specs {
        let Ok(named) = crate::deps::NamedSpec::parse(spec) else { continue };
        let Some(a) = meta.added.iter().find(|a| a.package == named.id.as_str()) else { continue };
        let base = present.iter().any(|p| p.release.id == named.id && matches!(p.origin, crate::deps::Origin::Base { .. }));
        let thorn_of = |t: &AddedThorn| t.checkout.rsplit('/').next().unwrap_or(&t.checkout).to_owned();
        let names_one = named.thorns.as_ref().is_none_or(|ts| {
            ts.iter().any(|n| a.thorns.iter().any(|t| thorn_of(t).eq_ignore_ascii_case(n)))
        });
        let requested = crate::deps::is_requested(believed.get(&named.id), &a.commit);
        if base && names_one && !requested && !out.contains(&a.package) {
            out.push(a.package.clone());
        }
    }
    out
}

/// Records `promoted` as named, and says so.
fn keep_as_named(meta: &mut crate::installation::InstallationMeta, promoted: &[String]) {
    for name in promoted {
        let Some(a) = meta.added.iter().find(|a| &a.package == name) else { continue };
        let entry = meta.added_for.entry(name.clone()).or_insert_with(|| crate::installation::AddedFor {
            requested: true,
            by: Vec::new(),
            for_commit: Some(a.commit.clone()),
            choices: Vec::new(),
        });
        entry.requested = true;
        let thorns: Vec<&str> = a.thorns.iter().map(|t| t.checkout.as_str()).collect();
        println!("Kept {} of {} as named.", thorns.join(", "), name.bold());
    }
}

/// What a plan moves, each once: an upgrade of a base package and the
/// replacement of its repo are one move.
fn moves_of(plan: &crate::deps::Plan) -> Vec<String> {
    use crate::deps::{Replaced, Step};
    plan.steps
        .iter()
        .filter_map(|s| match s {
            Step::Upgrade { to, .. } => Some(to.id.to_string()),
            Step::Replace { replaced: Replaced::Base { repo }, by, .. } => Some(format!("{by} (in place of base repo {repo})")),
            Step::Replace { replaced: Replaced::Added { id }, .. } => Some(id.to_string()),
            _ => None,
        })
        .fold(Vec::new(), |mut out: Vec<String>, m| {
            let id = m.split(' ').next().unwrap_or(&m).to_owned();
            match out.iter().position(|o| o.split(' ').next() == Some(id.as_str())) {
                Some(i) if m.len() > out[i].len() => out[i] = m,
                Some(_) => {}
                None => out.push(m),
            }
            out
        })
}

/// How many of the base packages whose dependencies are unknown
/// (`owner/name#commit`) stay so: one the plan moves to a release is known.
fn still_unchecked(unchecked: &[String], moved: &[&str]) -> usize {
    unchecked.iter().filter(|u| !moved.contains(&u.split('#').next().unwrap_or(u))).count()
}

/// `remove`'s line about base packages whose dependencies are unknown.
fn unchecked_short(gone: &[&str]) -> String {
    format!("Not checked: whether base packages at unreleased commits need {}.", gone.join(", "))
}

/// The one line about base packages whose dependencies are unknown (at
/// commits no version froze, or unreadable now): `n` of them, and what the
/// plan does that they might mind.
fn unchecked_line(n: usize, doing: &str) -> String {
    let packages = if n == 1 { "base package" } else { "base packages" };
    format!("Cannot check whether {doing} breaks {n} {packages} whose dependencies are unknown here.")
}

/// A yanked library the plan places: a dependency named exactly that
/// version (a range never takes a yanked one).
fn library_yank_warning(label: &str, reason: Option<&str>, by: &[&str]) -> String {
    let why = reason.map(|r| format!(": {r}")).unwrap_or_default();
    let asks = if by.len() == 1 { "asks" } else { "ask" };
    format!("The library {label} is yanked{why}. Using it because {} {asks} for exactly that version.", by.join(" and "))
}

/// `label is yanked: reason. Using it …`, for a release the plan takes
/// although it was yanked (asked for exactly, or a library a dependency
/// names exactly).
fn yank_warning(label: &str, reason: Option<&str>) -> String {
    let why = reason.map(|r| format!(": {r}")).unwrap_or_default();
    format!("{label} is yanked{why}. Using it because it was asked for exactly that version.")
}

pub fn dispatch(ctx: &Ctx, specs: &[String], flags: &AddFlags) -> Res<()> {
    use crate::deps::{Origin, Step};
    // What was asked must read before anything else is looked at.
    for spec in specs {
        crate::deps::NamedSpec::parse(spec)?;
    }
    for pattern in &flags.ignore_claim {
        pattern.parse::<crate::deps::ClaimPattern>().with_context(|| format!("--ignore-claim {pattern}"))?;
    }
    let start = Start::read(ctx, flags.force, Doing::Adding)?;
    let base = thornlist::parse(&start.pristine)?;
    let live = thornlist::parse(&start.before)?;
    let local = super::resolution::local_facts(&live, &start.inst.cactus_root()?)?;
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let (plan, present, heads) = plan_add(&client, &start.inst.root, &base, &start.meta, Some(&local), specs, flags)?;

    let command = format!("cactup add {}", specs.join(" "));
    let how = super::resolution::Confirm {
        yes: flags.yes,
        dry_run: flags.dry_run,
        interactive: crate::cactarium::interactive(),
    };
    // Thorns of a base repo's package that a dependency brought in, now
    // named: they stay when what brought them in goes.
    let promoted = named_extensions(specs, &present, &start.meta);
    if plan.is_empty() {
        for (_, line) in plan.lines(&command).iter().filter(|(k, _)| *k != crate::deps::LineKind::Header) {
            println!("{}", line.trim_start());
        }
        if promoted.is_empty() || flags.dry_run {
            println!("Nothing to add.");
            return Ok(());
        }
        let locked = start.inst.locked()?;
        let mut meta = start.recheck(&locked)?;
        keep_as_named(&mut meta, &promoted);
        locked.set_meta(&meta)?;
        return Ok(());
    }
    // Everything that can refuse the plan, before anyone is asked about it.
    let mut notes: Vec<String> = Vec::new();
    let records = records_of(&plan, &present, &start.meta, &base)?;
    let added = place(&base, &start.meta, records, &plan, &mut notes, &heads)?;
    // Added packages displaced by another (a flesh in place of a flesh
    // package): their records go.
    let displaced = displaced_by(&plan);
    let gone: Vec<AddedPackage> =
        start.meta.added.iter().filter(|a| displaced.iter().any(|(id, _)| *id == a.package)).cloned().collect();
    // The list as it will be, and what fetching it takes: the record cannot
    // change without the recheck below refusing, so neither can these.
    let mut entries = start.meta.added.clone();
    entries.retain(|a| !displaced.iter().any(|(id, _)| *id == a.package));
    for package in &added {
        entries.retain(|a| a.package != package.package);
        entries.push(package.clone());
    }
    let composed = compose(&start.pristine, &entries)?;
    let fetch_plan = fetch_plan_for(&start.inst, &composed, &base, &added)?;
    if !super::resolution::show_and_confirm(&plan.lines(&command), plan.beyond_named(), how)? {
        return Ok(());
    }
    for step in &plan.steps {
        if let Step::Add { release, .. } | Step::Readd { release, .. } | Step::Upgrade { to: release, .. } = step
            && release.yanked
        {
            println!("{}", yank_warning(&release.label(), release.yank_reason.as_deref()).yellow());
        }
    }
    for l in plan.libraries.iter().filter(|l| l.release.yanked && l.placements.iter().any(|p| p.new)) {
        let by: Vec<&str> = l.placements.iter().filter(|p| p.new).map(|p| p.by_label.as_str()).collect();
        println!("{}", library_yank_warning(&l.release.label(), l.release.yank_reason.as_deref(), &by).yellow());
    }
    confirm_untrusted(&plan, ctx.globals.risky_allow_untrusted_artifacts, how.interactive)?;

    #[cfg(test)]
    AFTER_PROMPT.with(|h| h.get().map(|h| h(&start.inst.root)));
    let locked = start.inst.locked()?;
    let _source_locks = super::copy::lock_sources(ctx, &start.inst)?;
    let mut meta = start.recheck(&locked)?;
    let now = fetch_plan_for(&start.inst, &composed, &base, &added).map_err(clones_changed)?;
    same_fetch(&fetch_plan, &now)?;
    let believed = super::resolution::added_for(&meta);
    let old_commits: std::collections::BTreeMap<String, String> =
        meta.added.iter().map(|a| (a.package.clone(), a.commit.clone())).collect();
    meta.added.retain(|a| !displaced.iter().any(|(id, _)| *id == a.package));
    meta.added_for.retain(|name, _| !displaced.iter().any(|(id, _)| id == name));
    for package in &added {
        if let Some(i) = meta.added.iter().position(|a| a.package == package.package) {
            meta.added.remove(i);
        }
        meta.added.push(package.clone());
    }
    // Why each package is there, for the commit it ends up at. A package with
    // no `[[added]]` entry before has no entry to merge with, whatever an
    // older binary's `remove` left in the map. More thorns of a base repo's
    // package, given for a dependency and never named, are brought in: they
    // go when nothing needs them any more.
    let mut rebuilt: Vec<String> = Vec::new();
    for record in &plan.records {
        let id = record.id.to_string();
        let Some(package) = meta.added.iter().find(|a| a.package == id) else { continue };
        let old_commit = old_commits.get(&id);
        let old = old_commit.and(believed.get(&record.id));
        let extends_base = present.iter().any(|p| {
            p.release.id == record.id
                && matches!(&p.origin, Origin::Base { repo } if *repo == package.repo)
                && p.release.commit == package.commit
                && package.replaces.is_empty()
        });
        let mut record = record.clone();
        if extends_base && old_commit.is_none() && record.requested.is_none() {
            record.requested = Some(false);
        }
        let merged = record.merge(old, old_commit.map(String::as_str));
        meta.added_for.insert(id.clone(), super::resolution::record_of(&merged));
        // Made anew only when written for another commit than before (or for
        // a package that had none); more thorns at the same commit edit it.
        if old_commit != Some(&package.commit) || old.is_none() {
            rebuilt.push(id);
        }
    }
    keep_as_named(&mut meta, &promoted);
    debug_assert!(meta.added == entries, "the record changed after the plan was checked");
    let inst = &start.inst;
    let repos: BTreeSet<String> = added.iter().flat_map(all_repos).collect();
    // After every refusal, just before the fetch: a placement the add drops
    // (its `into` changed, or the base took the library over) loses its link
    // and exclude line, since a stale link would have the new list's
    // sections resolved through it. Until the new list and record are
    // written, any failure (the fetch, a `?` below) undoes that, and takes
    // away the placements this add newly made.
    let mut undo = crate::fetch::library::PutBack::drop_placements(&inst.root, &start.before, &composed)?;
    // A flesh in or out moves the links at the root; they move back unless
    // the new list is written.
    let root_links = RootLinks::save(&inst.root, &[&start.before, &composed])?;
    let report = crate::fetch::execute(&fetch_plan, &inst.root)?;
    undo.made(&fetch_plan, &report);
    if !report.failures.is_empty() {
        // What did fetch is recorded all the same, so a later prune knows
        // cactup made it.
        crate::fetch::FetchState::record(&inst.root, &report.records())?;
        let why: Vec<String> = report.failures.iter().map(|f| format!("{}: {}", f.what, f.error)).collect();
        bail!("Failed to fetch {}:\n  {}", repos.iter().cloned().collect::<Vec<_>>().join(", "), why.join("\n  "));
    }
    crate::fetch::FetchState::record(&inst.root, &report.records())?;

    let live_path = &start.live_path;
    crate::cow::Cow::at(&inst.root)?.before_replace(live_path)?;
    std::fs::write(live_path, &composed).with_context(|| format!("Failed to write '{}'", live_path.display()))?;
    // The list is written: what it drops stays dropped, whatever follows.
    let unplacing = undo.commit();
    root_links.commit();
    locked.set_meta_rebuilding(&meta, &rebuilt)?;
    drop(locked);

    for package in &added {
        let flesh = package.is_flesh().then_some("the flesh");
        let checkouts: Vec<&str> =
            flesh.into_iter().chain(package.thorns.iter().map(|t| t.checkout.as_str())).collect();
        let instead: Vec<&str> =
            displaced.iter().filter(|(_, by)| *by == package.package).map(|(id, _)| id.as_str()).collect();
        let repos: Vec<String> = package.replaces.iter().map(|r| format!("base repo {r}")).collect();
        let instead = if instead.is_empty() { repos.join(", ") } else { instead.join(", ") };
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
            if instead.is_empty() { String::new() } else { format!(", in place of {instead}") }
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
        println!("Removed the old library link at {link}.");
    }
    for note in &unplacing.notes {
        println!("{note}");
    }
    let replaced: Vec<&str> = gone.iter().map(|a| a.package.as_str()).collect();
    for line in left_lines(&left_on_disk(&gone, &base), Some(&replaced)) {
        println!("{line}");
    }
    // A package re-added as it was (a flesh, say) leaves nothing new to compile.
    let compile = added.iter().any(|a| !a.thorns.is_empty() || !a.libraries.is_empty());
    if plan.rebuild || compile {
        println!("{}", build_hint(plan.rebuild.then_some("the new flesh"), false));
    }
    Ok(())
}

/// What says that `left` (repos left on disk, unlisted) stay for a prune:
/// those of the packages `replaced` by others (`add`), or of the packages
/// removed (`remove`, `None`).
fn left_lines(left: &[String], replaced: Option<&[&str]>) -> Vec<String> {
    let (repos, them) = match left {
        [] => return Vec::new(),
        [one] => (format!("repo {one}"), "it"),
        many => (format!("repos {}", many.join(", ")), "them"),
    };
    let stay = if left.len() == 1 { "stays" } else { "stay" };
    let first = match replaced {
        Some(replaced) => format!("The {repos} of the replaced {} {stay} on disk.", replaced.join(", ")),
        None if left.len() == 1 => format!("Its {repos} stays on disk."),
        None => format!("Their {repos} stay on disk."),
    };
    vec![first, format!("Remove {them} with `cactup inst refetch --prune`.")]
}

/// Every snapshot nobody vouches for that the plan takes (an untrusted
/// package's branch tip, or a commit no published version names): warned
/// about, and taken only on a yes at the terminal or with the flag.
fn confirm_untrusted(plan: &crate::deps::Plan, allow: bool, interactive: bool) -> Res<()> {
    use crate::cactarium::Unreviewed;
    for u in &plan.untrusted {
        let (label, what) = match &u.tip {
            Some(tip) => (format!("{}@{tip}", u.id), Unreviewed::Tip),
            None => (format!("{}#{}", u.id, &u.commit[..12.min(u.commit.len())]), Unreviewed::Commit),
        };
        crate::cactarium::confirm_untrusted_on(&label, "package", what, allow, interactive)?;
    }
    Ok(())
}

/// The added packages dropped by a plan for another (a flesh package
/// displaced by another flesh), each with the package taking its place.
fn displaced_by(plan: &crate::deps::Plan) -> Vec<(String, String)> {
    use crate::deps::{Replaced, Step};
    (plan.steps.iter())
        .filter_map(|s| match s {
            Step::Replace { replaced: Replaced::Added { id }, by, .. } => Some((id.to_string(), by.to_string())),
            _ => None,
        })
        .collect()
}

/// The last line of `add` and `remove`: what to build now. A changed flesh
/// needs every config rebuilt.
fn build_hint(flesh: Option<&str>, removing: bool) -> String {
    let build = "cactup build <config>".bold();
    match (flesh, removing) {
        (Some(flesh), _) => format!("Rebuild every config for {flesh}, e.g. `{build}`."),
        (None, false) => format!("Build a config to compile them, e.g. `{build}`."),
        (None, true) => format!("Rebuild configs that compiled their thorns, e.g. `{build}`."),
    }
}

/// The links at the Cactus root (the flesh's `Makefile`, `lib`, `src`, …)
/// as they were before a fetch re-points them, put back unless the command
/// goes through: a fetch that fails or is interrupted part way through its
/// links leaves some pointing at the new flesh and some at the old one.
struct RootLinks {
    /// Each link, and what it pointed at (`None`: nothing was there).
    saved: Vec<(std::path::PathBuf, Option<std::path::PathBuf>)>,
    armed: bool,
}

impl RootLinks {
    /// The links (or nothing) at the root checkouts of the `lists`; a real
    /// file or directory there is no link moved by a fetch.
    fn save(install_root: &std::path::Path, lists: &[&str]) -> Res<RootLinks> {
        let mut saved: Vec<(std::path::PathBuf, Option<std::path::PathBuf>)> = Vec::new();
        for text in lists {
            let list = thornlist::parse(text)?;
            let root = list.root().trim_end_matches('/');
            for c in list.components().iter().filter(|c| c.target.trim_end_matches('/') == root) {
                let path = install_root.join(&c.target).join(&c.checkout);
                if saved.iter().any(|(p, _)| *p == path) {
                    continue;
                }
                match std::fs::symlink_metadata(&path) {
                    Ok(m) if m.file_type().is_symlink() => {
                        let to = std::fs::read_link(&path)
                            .with_context(|| format!("Failed to read the link '{}'", path.display()))?;
                        saved.push((path, Some(to)));
                    }
                    Ok(_) => {}
                    Err(_) => saved.push((path, None)),
                }
            }
        }
        Ok(RootLinks { saved, armed: true })
    }

    /// The command went through: the links stay as they are now.
    fn commit(mut self) {
        self.armed = false;
    }

    /// Puts every link back that it can, and says why for each failure.
    fn put_back(&self) -> Vec<String> {
        let mut failed = Vec::new();
        for (path, was) in &self.saved {
            let now = std::fs::read_link(path).ok();
            if now == *was {
                continue;
            }
            let put = (|| -> Res<()> {
                if now.is_some() {
                    std::fs::remove_file(path).with_context(|| format!("Failed to remove '{}'", path.display()))?;
                }
                if let Some(to) = was {
                    std::os::unix::fs::symlink(to, path)
                        .with_context(|| format!("Failed to link '{}' to '{}'", path.display(), to.display()))?;
                }
                Ok(())
            })();
            if let Err(e) = put {
                failed.push(format!("{e:#}"));
            }
        }
        failed
    }
}

impl Drop for RootLinks {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let failed = self.put_back();
        for why in &failed {
            eprintln!("{why}");
        }
        if !failed.is_empty() {
            eprintln!("Some links at the Cactus root were not put back.\nRun `cactup inst refetch` to repair them.");
        }
    }
}

/// The `[[added]]` records a plan writes, in plan order, before they are
/// placed into the base's clones: one per package brought in, re-added or
/// moved, and per present package given more thorns; each `Replace` names
/// the base repo its package takes the place of.
fn records_of(
    plan: &crate::deps::Plan,
    present: &[crate::deps::Present],
    meta: &crate::installation::InstallationMeta,
    base: &thornlist::Thornlist,
) -> Res<Vec<AddedPackage>> {
    use crate::deps::{Origin, Replaced, Step};
    let mut out: Vec<AddedPackage> = Vec::new();
    for step in &plan.steps {
        match step {
            Step::Add { release, thorns, .. }
            | Step::Readd { release, thorns }
            | Step::Upgrade { to: release, thorns, .. } => {
                let mut package = record_of_release(release, thorns, plan)?;
                // A package added before keeps the base repos it replaced.
                if let Some(old) = meta.added.iter().find(|a| a.package == package.package) {
                    for r in &old.replaces {
                        if !package.replaces.contains(r) {
                            package.replaces.push(r.clone());
                        }
                    }
                    // Re-added at the release it is at: its libraries stay
                    // where they are (the plan places only a changed
                    // release's).
                    if matches!(step, Step::Readd { .. }) {
                        package.libraries.clone_from(&old.libraries);
                    }
                    if old.is_flesh() && !release.flesh {
                        bail!(
                            "{} is not a flesh.\n{} is the installation's flesh.\nRemove it first with `cactup remove {}`.",
                            release.label(),
                            old.package,
                            old.package
                        );
                    }
                }
                if release.flesh {
                    let why = match step {
                        Step::Add { why, .. } | Step::Upgrade { why, .. } => Some(why),
                        _ => None,
                    };
                    flesh_record(&mut package, release, why, base, meta)?;
                }
                out.push(package);
            }
            Step::AddThorns { id, thorns, release, .. } => {
                let p = present
                    .iter()
                    .find(|p| &p.release.id == id)
                    .ok_or_else(|| anyhow!("The plan gives thorns to {id}, which the installation does not have"))?;
                let extended = meta.added.iter().any(|a| a.package == id.as_str());
                match &p.origin {
                    // An added package, or a base repo's package an
                    // `[[added]]` entry already gives more thorns: that entry
                    // grows.
                    _ if extended => {
                        let mut package = (meta.added.iter())
                            .find(|a| a.package == id.as_str())
                            .cloned()
                            .ok_or_else(|| anyhow!("{id} has no record in this installation"))?;
                        // The release as the plan read it (a stand-in it read has
                        // more thorns than the one the installation was taken with).
                        let more = record_of_release(release, thorns, plan)?;
                        for t in more.thorns {
                            if !package.thorns.iter().any(|have| have.checkout == t.checkout) {
                                package.thorns.push(t);
                            }
                        }
                        // The libraries the new thorns need.
                        for l in more.libraries {
                            if !package.libraries.iter().any(|have| have.package == l.package && have.into == l.into) {
                                package.libraries.push(l);
                            }
                        }
                        // More thorns of a base repo's package: the entry is
                        // at the release the base has now.
                        if matches!(p.origin, Origin::Base { .. }) {
                            package.version = more.version;
                            package.commit = more.commit;
                            package.tag = more.tag;
                        }
                        out.push(package);
                    }
                    Origin::Added => bail!("{id} has no record in this installation"),
                    // More thorns of a base repo's package, at the base's
                    // commit: they go into the base's own clone.
                    Origin::Base { .. } => out.push(record_of_release(release, thorns, plan)?),
                }
            }
            Step::Replace { replaced: Replaced::Base { repo }, by, .. } => {
                let package = (out.iter_mut())
                    .rev()
                    .find(|a| a.package == by.as_str())
                    .ok_or_else(|| anyhow!("The plan replaces base repo {repo} with {by}, which it does not add"))?;
                if !package.replaces.contains(repo) {
                    package.replaces.push(repo.clone());
                }
            }
            // A flesh in place of an added flesh package: that package goes
            // (the caller drops its record), and the flesh's record takes the
            // base's flesh repos over, as every flesh package does.
            Step::Replace { replaced: Replaced::Added { id }, by, flesh, .. } => {
                let taker = out.iter().rev().find(|a| a.package == by.as_str());
                if !flesh || !taker.is_some_and(AddedPackage::is_flesh) {
                    bail!("The plan replaces {id} with {by}, which this cactup cannot do")
                }
            }
            Step::Keep { .. } => {}
        }
    }
    Ok(out)
}

/// Makes `package` (of `release`, a flesh, brought in for `why`) a flesh
/// package: its items at the root are those of the base's flesh, and it takes
/// the place of the base's flesh repos. A base with no flesh leaves the items
/// recorded before, when it is added again; else it is refused.
fn flesh_record(
    package: &mut AddedPackage,
    release: &crate::deps::Release,
    why: Option<&crate::deps::Why>,
    base: &thornlist::Thornlist,
    meta: &crate::installation::InstallationMeta,
) -> Res<()> {
    package.flesh = base_flesh_items(base);
    if package.flesh.is_empty()
        && let Some(old) = meta.added.iter().find(|a| a.package == package.package)
    {
        package.flesh.clone_from(&old.flesh);
    }
    if package.flesh.is_empty() {
        let needed = match why {
            Some(crate::deps::Why::Needed { by, .. }) => format!("\n{by} needs it."),
            _ => String::new(),
        };
        bail!(
            "{} is a Cactus flesh.{needed}\nThe installation's thornlist has no flesh to replace.",
            release.label()
        );
    }
    for repo in base_flesh_repos(base) {
        if !package.replaces.iter().any(|r| r == repo) {
            package.replaces.push(repo.to_owned());
        }
    }
    Ok(())
}

/// `release`'s record with `thorns` enabled and the libraries the plan places
/// for it, after the checks every name and source given by Cactarium
/// passes before it is written anywhere.
fn record_of_release(release: &crate::deps::Release, thorns: &[String], plan: &crate::deps::Plan) -> Res<AddedPackage> {
    use crate::deps::At;
    let slug = release.label();
    let (version, mut tag) = match &release.at {
        At::Version(v) => (v.to_string(), release.tag.clone()),
        At::Tip { name, .. } => (name.clone(), None),
        At::Commit(c) | At::Unknown(c) => (c.clone(), None),
    };
    if is_tip(&version) {
        // The label recorded with a tip: the default branch it follows, named
        // so a refetch knows which branch to move along.
        let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
        let line = crate::progress::Line::over(
            progress.add_child(slug.clone()),
            &slug,
            crate::progress::Layout::for_names([slug.as_str()]),
        );
        line.phase("asking for the default branch");
        let branch = crate::fetch::git::ask_remote_default_branch(&release.url, crate::fetch::git::AskWindow::now());
        drop(line);
        if let Some(renderer) = renderer {
            renderer.shutdown_and_wait();
        }
        tag = Some(branch?);
    }
    check_registry_source(&slug, &release.url, &release.commit, tag.as_deref())?;
    for t in &release.thorns {
        check_registry_names(&slug, &t.arrangement, &t.thorn, &t.repo_path)?;
    }
    // The package's own clone is named from its URL: a plain component too.
    let clone = repo_name(&release.url);
    if clone.is_empty()
        || clone == "."
        || clone == ".."
        || clone.starts_with('-')
        || !clone.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    {
        bail!("Cactarium gives {slug} the repository {}, whose name is not a usable directory name", release.url);
    }
    let mut checkouts: Vec<AddedThorn> = Vec::new();
    for name in thorns {
        let t = release.thorn(name).ok_or_else(|| anyhow!("{slug} has no thorn {name}"))?;
        if t.repo_path.trim().is_empty() {
            bail!(
                "Cactarium gave no repo-path for {}/{} in {slug}.\nReport this to Cactarium.",
                t.arrangement,
                t.thorn
            );
        }
        checkouts.push(AddedThorn { checkout: format!("{}/{}", t.arrangement, t.thorn), repo_path: t.repo_path.clone() });
    }
    // The libraries this release places, each inside its own clone or inside
    // another library's (named by that library's clone).
    let mine: Vec<(&crate::deps::Library, &crate::deps::Placement)> = (plan.libraries.iter())
        .flat_map(|l| l.placements.iter().map(move |p| (l, p)))
        .filter(|(_, p)| p.new && p.by == release.id)
        .collect();
    let mut libraries = Vec::new();
    for (l, p) in &mine {
        let r = &l.release;
        let label = r.label();
        let (version, tag) = match &r.at {
            At::Version(v) => (v.to_string(), r.tag.clone()),
            At::Tip { commit, .. } | At::Commit(commit) | At::Unknown(commit) => (commit.clone(), None),
        };
        check_registry_source(&label, &r.url, &r.commit, tag.as_deref())?;
        crate::fetch::library::check_into(&p.into)
            .with_context(|| format!("{} places {} at \"{}\"", p.by_label, r.id, p.into.escape_debug()))?;
        let inside = match &p.inside {
            None => String::new(),
            Some(holder) => {
                let into = (mine.iter())
                    .find(|(hl, hp)| &hl.release.id == holder && hp.inside != Some(r.id.clone()))
                    .map(|(_, hp)| hp.into.as_str())
                    .ok_or_else(|| anyhow!("{label} goes inside {holder}, which {slug} does not place"))?;
                into.rsplit('/').next().unwrap_or(into).to_owned()
            }
        };
        let library = AddedLibrary {
            package: r.id.to_string(),
            version,
            tag,
            commit: r.commit.clone(),
            url: r.url.clone(),
            inside,
            into: p.into.clone(),
        };
        if !libraries.contains(&library) {
            libraries.push(library);
        }
    }
    Ok(AddedPackage {
        package: release.id.to_string(),
        version,
        tag,
        commit: release.commit.clone(),
        url: release.url.clone(),
        repo: clone,
        thorns: checkouts,
        replaces: Vec::new(),
        contact: release.contact.clone(),
        libraries,
        flesh: Vec::new(),
    })
}

/// The commit HEAD is at in clone `repo` of the installation, when it can be
/// read.
pub(crate) fn clone_head(install_root: &std::path::Path, root: &str, repo: &str) -> Option<String> {
    let dir = install_root.join(root).join("repos").join(repo);
    crate::fetch::git::head_of(&dir).ok().map(|(_, id)| id.to_string())
}

/// The records placed into the installation: each goes into a clone the base
/// already has when the base clones its repository (at the base's commit
/// there), a package taking a base thorn's place takes it at the same
/// `Arrangement/Thorn`, and a clone's name means one source. Libraries go
/// inside their package's clone, or stay where the base puts them.
fn place(
    base: &thornlist::Thornlist,
    meta: &crate::installation::InstallationMeta,
    records: Vec<AddedPackage>,
    plan: &crate::deps::Plan,
    notes: &mut Vec<String>,
    on_disk: &std::collections::BTreeMap<String, String>,
) -> Res<Vec<AddedPackage>> {
    use crate::deps::{Replaced, Step};
    // The base checkouts the live list keeps (compose drops the claimed
    // checkouts of replaced repos).
    let replaced: BTreeSet<&str> = meta.added.iter().flat_map(|a| a.replaces.iter().map(String::as_str)).collect();
    let claimed: BTreeSet<&str> = meta.added.iter().flat_map(|a| a.thorns.iter().map(|t| t.checkout.as_str())).collect();
    let kept: Vec<&thornlist::Component> = (base.components().iter())
        .filter(|c| !(replaced.contains(c.repo.as_str()) && claimed.contains(c.checkout.as_str())))
        .collect();
    let thorn = |checkout: &str| checkout.rsplit('/').next().unwrap_or(checkout).to_ascii_lowercase();
    let mut out = Vec::new();
    for mut package in records {
        let slug = describe(&package.package, &package.version);
        let mut url = package.url.clone();
        let mut repo = package.repo.clone();
        let reused = reuse_base_clone(base, &mut url, &mut repo, &mut package.thorns);
        package.url = url;
        package.repo = repo.clone();
        // More thorns of the base's own clone at the commit it is at on disk
        // follow the base's source (compose), so they share it whatever the
        // base's other checkouts say.
        let at_disk = on_disk.get(&repo).is_some_and(|c| *c == package.commit);
        if reused && !(package.replaces.is_empty() && at_disk) {
            share_base_clone(base, &repo, &slug, &mut package)?;
        }
        // A thorn taken over from a base repo is taken at the base's own
        // checkout: compose drops exactly the checkouts the package claims.
        for step in &plan.steps {
            let Step::Replace { replaced: Replaced::Base { repo: base_repo }, by, thorns, .. } = step else {
                continue;
            };
            if by.as_str() != package.package {
                continue;
            }
            let named: BTreeSet<String> = thorns.iter().map(|t| t.to_ascii_lowercase()).collect();
            for c in kept.iter().filter(|c| &c.repo == base_repo && crate::sim::trim::is_arrangement_thorn(base, c)) {
                if !named.is_empty() && !named.contains(&thorn(&c.checkout)) {
                    continue;
                }
                let theirs = package.thorns.iter().find(|t| thorn(&t.checkout) == thorn(&c.checkout));
                if let Some(t) = theirs
                    && t.checkout != c.checkout
                {
                    bail!(
                        "{slug} would replace {} of base repo {base_repo}, but has it as {}; a replacement needs the \
                         same Arrangement/Thorn",
                        c.checkout,
                        t.checkout
                    );
                }
            }
        }
        // A flesh package from another repository in the clone of the base's
        // flesh: the base's flesh comes back there on `remove`, and the fetch
        // would have to move one clone between two repositories.
        if package.is_flesh() && !reused && base_flesh_repos(base).contains(package.repo.as_str()) {
            bail!(
                "{slug} and the base's flesh have the same clone name, {}.\nA flesh package needs a clone name \
                 of its own.",
                package.repo
            );
        }
        // A clone's name is one source: a base repo of the same name whose
        // checkouts stay would be a second.
        if !reused {
            let mine: BTreeSet<&str> = package.thorns.iter().map(|t| t.checkout.as_str()).collect();
            let stays: Vec<&str> = (kept.iter())
                .filter(|c| c.repo == package.repo)
                .filter(|c| !(package.replaces.contains(&c.repo) && mine.contains(c.checkout.as_str())))
                .map(|c| c.checkout.as_str())
                .collect();
            if !stays.is_empty() {
                bail!(
                    "{slug} would be cloned as repos/{}, which the base already uses for {}",
                    package.repo,
                    stays.join(", ")
                );
            }
        }
        let libraries = std::mem::take(&mut package.libraries);
        package.libraries = libraries_inside(base, &package, libraries, notes)?;
        out.push(package);
    }
    Ok(out)
}

/// The clones of `removed` packages that stay on disk unlisted, for `inst
/// refetch --prune` to take: not one a package replaced (it goes back to
/// the base's pin), nor one the base checks out itself. A flesh package's
/// own clone stays (the base's flesh is another).
fn left_on_disk(removed: &[AddedPackage], base: &thornlist::Thornlist) -> Vec<String> {
    let base_repos: BTreeSet<&str> = base.components().iter().map(|c| c.repo.as_str()).collect();
    (removed.iter().filter(|r| r.replaces.is_empty() || r.is_flesh()))
        .flat_map(all_repos)
        .filter(|repo| !base_repos.contains(repo.as_str()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// What `remove` takes away beyond `targets`, and whether it may: asks
/// Cactarium for the current claims of every present package. An
/// `[[added]]` entry that only gives a base repo's package more thorns is one
/// package with that repo: named, it goes unless a remaining package needs
/// those thorns; never named and no longer needed, it goes as an orphan.
fn plan_remove_with_deps(
    ctx: &Ctx,
    start: &Start,
    base: &thornlist::Thornlist,
    targets: &[crate::deps::PkgId],
) -> Res<crate::deps::RemovePlan> {
    use crate::deps::{self, Extension, Origin, PkgId, Rules};
    let named: Vec<String> = targets.iter().map(ToString::to_string).collect();
    // Without Cactarium nothing can say what depends on what.
    let way = format!(
        "Pass --no-deps to remove {} without Cactarium.",
        named.join(", ")
    );
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let names: Vec<&str> = start.meta.added.iter().map(|a| a.package.as_str()).collect();
    let mut reg = super::resolution::Registry::new(&client, names);
    let options = super::resolution::PresentOptions::default();
    let added_for = super::resolution::added_for(&start.meta);
    let extensions_of = |set: &super::resolution::PresentSet| -> Vec<Extension> {
        (start.meta.added.iter())
            .filter_map(|a| {
                let id = PkgId::parse(&a.package).ok()?;
                let base = set.present.iter().any(|p| p.release.id == id && matches!(p.origin, Origin::Base { .. }));
                let thorns: Vec<String> =
                    a.thorns.iter().map(|t| t.checkout.rsplit('/').next().unwrap_or(&t.checkout).to_owned()).collect();
                // As the base has the package now (a refetch may have moved it).
                let at = set.present.iter().find(|p| p.release.id == id).map(|p| p.release.label())?;
                base.then(|| Extension {
                    id,
                    label: format!("{at} ({})", thorns.join(", ")),
                    thorns,
                    commit: a.commit.clone(),
                })
            })
            .collect()
    };
    let set = super::resolution::present(&mut reg, &start.inst.root, base, &start.meta.added, None, &options);
    // A base package at a commit no version froze that cactup gave more
    // thorns: what those thorns claim decides what may go, so it is read
    // there, exactly.
    let set = set.and_then(|mut set| {
        let extended: Vec<PkgId> = (set.present.iter())
            .filter(|p| p.stand_in && start.meta.added.iter().any(|a| a.package == p.release.id.as_str()))
            .map(|p| p.release.id.clone())
            .collect();
        let unread = super::resolution::read_stand_ins(&mut reg, &mut set, &extended)?;
        // What may go: the targets, and every added package the user did not name. Not only
        // what the sweep takes with no base back: a base that comes back can take a dependency
        // over and leave more to sweep, whose bases come back in turn. Only their bases are read.
        let added = start.meta.added.iter().filter_map(|a| PkgId::parse(&a.package).ok());
        let bringing = deps::may_bring_back(targets, &added_for, added);
        let returning =
            super::resolution::returning_base(&mut reg, &start.inst.root, base, &start.meta.added, &bringing)?;
        Ok((set, unread, returning))
    });
    reg.finish();
    let (set, unread_extensions, (returning, unread_returning)) = set.map_err(|e| needs_the_cactarium(e, &way))?;
    if !unread_extensions.is_empty() {
        let lines: Vec<String> = unread_extensions.iter().map(|(id, u)| format!("\n  {id}: {}", u.why)).collect();
        bail!(
            "Cannot tell what still needs {}.\nThe needs of thorns added to these base packages are unknown:{}\n\
             Pass --no-deps to remove only the packages named.",
            named.join(", "),
            lines.concat()
        );
    }
    // A remaining added package whose release cannot be read hides what it
    // needs: nothing can be said to be unneeded beside it.
    let unread: Vec<&super::resolution::Unread> = set.unread.iter().filter(|u| !targets.contains(&u.id)).collect();
    if !unread.is_empty() {
        bail!(
            "Cannot tell what still needs {}.\nThe dependencies of these installed packages are unknown:{}\n\
             Pass --no-deps to remove only the packages named.",
            named.join(", "),
            unread_lines(&unread)
        );
    }
    let extensions = extensions_of(&set);
    // Claim overrides kept in the installation join with `cactup claims` (P4).
    let mut plan = deps::plan_remove(&set.present, &added_for, targets, &Rules::default(), false, &extensions, &returning)
        .map_err(|r| {
            // What comes back with the targets that could not be read might have held them.
            let lines: Vec<String> = (unread_returning.iter())
                .filter(|(replacer, _)| targets.contains(replacer))
                .map(|(_, u)| format!("\n  {u}"))
                .collect();
            match (&r, lines.is_empty()) {
                (deps::Refusal::HasDependents { .. }, false) => anyhow!(
                    "{r}\nBase repos coming back cannot be read; they might meet these needs:{}",
                    lines.concat()
                ),
                _ => anyhow!("{r}"),
            }
        })?;
    // What comes back with what goes (the targets and the orphans) that could not be read: what
    // it needs is unknown, and any orphan might be it, so the plan says so beside each, and the
    // confirmation covers it.
    let going: BTreeSet<&PkgId> = plan.removed.iter().chain(plan.orphans.iter().map(|o| &o.id)).collect();
    let unread_back: Vec<&String> =
        unread_returning.iter().filter(|(replacer, _)| going.contains(replacer)).map(|(_, why)| why).collect();
    if !unread_back.is_empty() {
        let lines: Vec<String> = unread_back.iter().map(|u| format!("\n  {u}")).collect();
        println!("{}", format!("Not checked (unreadable): what these returning base repos need:{}", lines.concat()).yellow());
        for o in &mut plan.orphans {
            o.caveat = Some("unless a returning base repo needs it".to_owned());
        }
    }
    // Base packages whose dependencies are unknown are normal (a base that
    // follows branches), and nothing tells which of them could claim what
    // goes (their claims are what is unknown): one short line, no count.
    if !set.unchecked.is_empty() {
        let gone: Vec<&str> = plan.removed.iter().chain(plan.orphans.iter().map(|o| &o.id)).map(PkgId::as_str).collect();
        println!("{}", unchecked_short(&gone).yellow());
    }
    Ok(plan)
}

/// `cactup remove owner/name…` (§8.11): undo adds. Unless `--no-deps`, it
/// first asks Cactarium what depends on what (the current claims of
/// every present package): it refuses while another package depends on one
/// named, and takes along the packages only the removed ones needed, after
/// showing them. The records go, the live list is recomposed without them,
/// and only the repos they touched are fetched again — their own (left on
/// disk, now unlisted; `inst refetch --prune` removes them) and any base
/// repo one replaced, which goes back to the base's pin.
pub fn remove(ctx: &Ctx, packages: &[String], flags: &RemoveFlags) -> Res<()> {
    use crate::deps::{self, PkgId, Rules};
    let mut targets: Vec<PkgId> = Vec::new();
    for package in packages {
        let slug: Slug = package.parse().map_err(|e| anyhow!("\"{package}\" is not a package (owner/name): {e}"))?;
        if slug.at.is_some() || slug.sha.is_some() || slug.thorn.is_some() {
            bail!("Name the whole package as owner/name");
        }
        let id = PkgId::parse(&format!("{}/{}", slug.owner, slug.name))?;
        if !targets.contains(&id) {
            targets.push(id);
        }
    }
    let start = Start::read(ctx, flags.force, Doing::Removing)?;
    for id in &targets {
        if !start.meta.added.iter().any(|a| a.package == id.as_str()) {
            let added: Vec<&str> = start.meta.added.iter().map(|a| a.package.as_str()).collect();
            bail!(
                "{id} was not added to this installation (added: {})",
                if added.is_empty() { "nothing".to_owned() } else { added.join(", ") }
            );
        }
    }
    let base = thornlist::parse(&start.pristine)?;
    let plan = if flags.no_deps {
        deps::plan_remove(&[], &Default::default(), &targets, &Rules::default(), true, &[], &[])
            .map_err(|r| anyhow!("{r}"))?
    } else {
        plan_remove_with_deps(ctx, &start, &base, &targets)?
    };
    let command = format!("cactup remove {}", packages.join(" "));
    let how = super::resolution::Confirm {
        yes: flags.yes,
        dry_run: flags.dry_run,
        interactive: crate::cactarium::interactive(),
    };
    let gone: Vec<String> =
        plan.removed.iter().chain(plan.orphans.iter().map(|o| &o.id)).map(ToString::to_string).collect();
    let inst = &start.inst;
    // The list as it will be, and restoring what the removed packages
    // replaced, before anyone is asked: the recheck below refuses if the
    // record changes meanwhile.
    let removed: Vec<AddedPackage> = start.meta.added.iter().filter(|a| gone.contains(&a.package)).cloned().collect();
    let entries: Vec<AddedPackage> = start.meta.added.iter().filter(|a| !gone.contains(&a.package)).cloned().collect();
    let composed = compose(&start.pristine, &entries)?;

    // The replaced base repos go back to what the base pins: the base's
    // flesh too (whatever it is called now), for a flesh package, its links
    // at the root pointed back into it, and its clone fetched again if it
    // was pruned.
    let flesh = removed.iter().any(AddedPackage::is_flesh);
    let mut touched: BTreeSet<&str> =
        removed.iter().flat_map(|r| r.replaces.iter().map(String::as_str)).collect();
    if flesh {
        touched.extend(base_flesh_repos(&base));
    }
    let restore = restore_plan(inst, &composed, &touched, &gone, &removed)?;
    let lines = removal_lines(&plan, &command, flesh);
    if !super::resolution::show_and_confirm(&lines, plan.beyond_named(), how)? {
        return Ok(());
    }
    let locked = inst.locked()?;
    let _source_locks = super::copy::lock_sources(ctx, inst)?;
    let mut meta = start.recheck(&locked)?;
    if let Some(checked) = &restore {
        let now = restore_plan(inst, &composed, &touched, &gone, &removed).map_err(clones_changed)?;
        same_fetch(checked, now.as_ref().expect("the same repos to restore"))?;
    }
    meta.added.retain(|a| !gone.contains(&a.package));
    meta.added_for.retain(|name, _| !gone.contains(name));
    // After every refusal: their libraries' links and exclude lines go with
    // them (only what placing made; whatever else is at those paths is left,
    // with a note). Until the new list and record are written, any failure
    // puts them back.
    let mut undo = crate::fetch::library::PutBack::drop_placements(&inst.root, &start.before, &composed)?;
    let root_links = RootLinks::save(&inst.root, &[&start.before, &composed])?;
    let restored: Vec<String> = restore.as_ref().map(restored_lines).unwrap_or_default();
    if let Some(fetch_plan) = restore {
        let report = crate::fetch::execute(&fetch_plan, &inst.root)?;
        undo.made(&fetch_plan, &report);
        if !report.failures.is_empty() {
            let why: Vec<String> = report.failures.iter().map(|f| format!("{}: {}", f.what, f.error)).collect();
            let replaced: Vec<&str> = touched.iter().copied().collect();
            bail!("Failed to restore {}:\n  {}", replaced.join(", "), why.join("\n  "));
        }
        crate::fetch::FetchState::record(&inst.root, &report.records())?;
    }
    // A base with no flesh (a refetch took it away) gets no flesh back: the
    // root's links into the removed flesh's clone go, so the root is the
    // base's. They are links only; the clone keeps the files.
    let fleshless = flesh && base_flesh_repos(&base).is_empty();
    let unlinked = if fleshless { unlink_flesh(&inst.cactus_root()?, &removed)? } else { Vec::new() };
    let live_path = &start.live_path;
    crate::cow::Cow::at(&inst.root)?.before_replace(live_path)?;
    std::fs::write(live_path, &composed).with_context(|| format!("Failed to write '{}'", live_path.display()))?;
    // The list is written: what it drops stays dropped, whatever follows.
    let unplacing = undo.commit();
    root_links.commit();
    locked.set_meta(&meta)?;
    drop(locked);

    for package in &removed {
        if base.components().iter().any(|c| c.repo == package.repo) && package.replaces.is_empty() {
            let thorns: Vec<&str> = package.thorns.iter().map(|t| t.checkout.as_str()).collect();
            println!(
                "Removed {} of {} from the thornlist.\nIts repo belongs to the base and stays.",
                thorns.join(", "),
                package.package.bold()
            );
        } else {
            println!("Removed {} from the thornlist.", package.package.bold());
        }
    }
    for link in unplacing.links() {
        println!("Removed its library link at {link}.");
    }
    for note in &unplacing.notes {
        println!("{note}");
    }
    if !unlinked.is_empty() {
        println!("The base has no flesh.");
        println!("Removed the links at the Cactus root into repos/{}.", unlinked.join(", repos/"));
    }
    for line in left_lines(&left_on_disk(&removed, &base), None) {
        println!("{line}");
    }
    for line in &restored {
        println!("{line}");
    }
    println!("{}", build_hint((flesh && !fleshless).then_some("the base's flesh"), true));
    Ok(())
}

/// What `remove` shows of its plan: a flesh package going changes the flesh,
/// so every config needs a rebuild.
fn removal_lines(plan: &crate::deps::RemovePlan, command: &str, flesh: bool) -> Vec<(crate::deps::LineKind, String)> {
    let mut lines = plan.lines(command);
    if flesh {
        lines.push((crate::deps::LineKind::Rebuild, crate::deps::FLESH_REBUILD.to_owned()));
    }
    lines
}

/// Removes the links at the Cactus root (`cactus`) into the clones of the
/// flesh packages among `removed`, each at one of its items: the clones
/// whose links went.
fn unlink_flesh(cactus: &std::path::Path, removed: &[AddedPackage]) -> Res<Vec<String>> {
    let repos = cactus.join("repos");
    let real = std::fs::canonicalize(&repos).ok();
    let mut unlinked: Vec<String> = Vec::new();
    for a in removed.iter().filter(|a| a.is_flesh()) {
        for item in &a.flesh {
            let path = cactus.join(item);
            let linked = std::fs::symlink_metadata(&path).is_ok_and(|m| m.file_type().is_symlink());
            if !linked || thornlist::repo_behind(&path, &repos, real.as_deref()).as_deref() != Some(a.repo.as_str()) {
                continue;
            }
            std::fs::remove_file(&path).with_context(|| format!("Failed to remove the link '{}'", path.display()))?;
            if !unlinked.contains(&a.repo) {
                unlinked.push(a.repo.clone());
            }
        }
    }
    Ok(unlinked)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A restored base repo is said as it went back: to the base's pin, or
    /// to the branch a base that follows one has.
    #[test]
    fn the_fetch_under_the_lock_is_compared_without_its_notes_words() {
        let plan = |branch: &str, note: Option<&str>| crate::fetch::Plan {
            git: vec![crate::fetch::GitRepoPlan {
                repo: "carpet".into(),
                dir: std::path::PathBuf::new(),
                url: String::new(),
                branch: Some(branch.into()),
                commit: None,
                action: crate::fetch::GitAction::Align,
                checkouts: Vec::new(),
                forced: None,
                retarget: None,
                note: note.map(str::to_owned),
                unasked: None,
            }],
            skipped: Vec::new(),
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            libraries: Vec::new(),
            root: "Cactus".into(),
        };
        let unknown = |why: &str| format!("its remote's default branch is unknown ({why}); going back to v1");
        let (a, b) = (unknown("timed out"), unknown("connection refused"));
        assert!(same_fetch(&plan("v1", Some(&a)), &plan("v1", Some(&b))).is_ok());
        assert!(same_fetch(&plan("v1", Some(&a)), &plan("master", None)).is_err());
        assert!(same_fetch(&plan("v1", Some(&a)), &plan("v1", None)).is_err());
    }

    #[test]
    fn a_restore_says_where_it_went() {
        let item = |repo: &str, commit: Option<gix::ObjectId>, branch: Option<&str>| crate::fetch::GitRepoPlan {
            repo: repo.into(),
            dir: std::path::PathBuf::new(),
            url: String::new(),
            branch: branch.map(str::to_owned),
            commit,
            action: crate::fetch::GitAction::Align,
            checkouts: Vec::new(),
            forced: None,
            retarget: None,
            note: None,
            unasked: None,
        };
        let pin = gix::ObjectId::empty_tree(gix::hash::Kind::Sha1);
        let plan = crate::fetch::Plan {
            git: vec![item("carpetx", None, Some("master")), item("cactusbase", Some(pin), Some("v2026.5.0"))],
            skipped: Vec::new(),
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            libraries: Vec::new(),
            root: "Cactus".into(),
        };
        assert_eq!(
            restored_lines(&plan),
            ["Restored carpetx to the base's branch master.", "Restored cactusbase to the base's commit."]
        );
    }

    fn package(repo: &str, checkouts: &[(&str, &str)], replaces: &[&str]) -> AddedPackage {
        AddedPackage {
            libraries: Vec::new(),
            flesh: Vec::new(),
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

    /// `!REPO_PATH` is Cactarium's value, verbatim, for every layout,
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
                flesh: Vec::new(),
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
    /// whatever Cactarium derived; a repository that the base does not have
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
        let mut tip = package("spacetimex", &[("SpacetimeX/Z4c", "$2")], &[]);
        tip.version = "head".into();
        tip.tag = Some("main".into());
        let out = compose(BASE, &[tip]).unwrap();
        let added = &out[out.find(BLOCK_HEADER).unwrap()..];
        assert!(!added.contains("!REPO_COMMIT"), "{added}");
        let list = thornlist::parse(&out).unwrap();
        let z4c = list.components().iter().find(|c| c.checkout == "SpacetimeX/Z4c").unwrap();
        assert_eq!((z4c.commit.as_deref(), z4c.branch.as_deref()), (None, Some("main")));
        assert!(is_tip("master") && !is_tip("1.0.0"));
    }

    /// More thorns in a clone the base has (the same repository, replacing
    /// nothing) follow the base's source for it, whatever commit they were
    /// recorded at: a refetch that moves the base moves them, and one clone
    /// keeps one source.
    #[test]
    fn more_thorns_of_a_base_clone_follow_the_base() {
        let pinned = BASE.replace(
            "!URL = https://x/carpetx.git\n",
            &format!("!URL = https://x/carpetx.git\n!REPO_BRANCH = main\n!REPO_COMMIT = {}\n", "b".repeat(40)),
        );
        // Recorded under another spelling of the URL (a refetch may bring
        // one): the base's is written, so one clone keeps one source.
        let more = AddedPackage { url: "https://x/carpetx".into(), ..package("carpetx", &[("CarpetX/Arith", "$2")], &[]) };
        let list = thornlist::parse(&compose(&pinned, std::slice::from_ref(&more)).unwrap()).unwrap();
        let arith = list.components().iter().find(|c| c.checkout == "CarpetX/Arith").unwrap();
        assert_eq!((arith.commit.as_deref(), arith.branch.as_deref()), (Some(&*"b".repeat(40)), Some("main")));
        assert_eq!(arith.url.as_deref(), Some("https://x/carpetx.git"));
        // Replacing the repo, it is its own source.
        let fork = package("carpetx", &[("CarpetX/CarpetX", "$2")], &["carpetx"]);
        let list = thornlist::parse(&compose(&pinned, &[fork]).unwrap()).unwrap();
        let carpetx = list.components().iter().find(|c| c.checkout == "CarpetX/CarpetX").unwrap();
        assert_eq!(carpetx.commit.as_deref(), Some(&*"5".repeat(40)));
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

    /// What `cactup add specs` would record on an installation with an empty
    /// base, against Cactarium at `base` (nothing confirmed at a
    /// terminal, nothing untrusted taken), with `flags`.
    fn wanted_with(base: &str, specs: &[&str], flags: &AddFlags) -> Res<Vec<AddedPackage>> {
        let client = crate::cactarium::Client::at(base);
        let tmp = tempfile::tempdir()?;
        let list = thornlist::parse("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n")?;
        let meta = crate::installation::InstallationMeta::default();
        let specs: Vec<String> = specs.iter().map(|s| (*s).to_owned()).collect();
        let (plan, present, _) = plan_add(&client, tmp.path(), &list, &meta, None, &specs, flags)?;
        confirm_untrusted(&plan, false, false)?;
        let records = records_of(&plan, &present, &meta, &list)?;
        place(&list, &meta, records, &plan, &mut Vec::new(), &Default::default())
    }

    fn wanted(base: &str, specs: &[&str]) -> Res<Vec<AddedPackage>> {
        wanted_with(base, specs, &AddFlags::default())
    }

    /// `"version":…,"commit":…` and the rest of a version's fields.
    fn info(version: &str, commit: &str, tag: Option<&str>) -> String {
        let tag = tag.map_or("null".to_owned(), |t| format!("\"{t}\""));
        format!(
            r#""version":"{version}","track":null,"tag":{tag},"commit":"{commit}","tree":null,"yanked":false,
            "yank-reason":null,"state":"ok","curated":false,"published":0"#
        )
    }

    /// `/resolve`'s answer for a package.
    fn resolved(slug: &str, url: &str, version: &str, commit: &str) -> String {
        format!(r#"{{"slug":"{slug}","kind":"package","url":"{url}","contact":null,{}}}"#, info(version, commit, None))
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
        let c = "4715".repeat(10);
        let resolved = resolved("max/carpetx@head", &url, "head", &c);
        let facts = serde_json::to_string(&cactup_ecosystem_common::ccl::ThornFacts::default()).unwrap();
        let tip = format!(
            r#"{{{},"url":"{url}","claims":[],
            "thorns":[{{"arrangement":"CarpetX","thorn":"Arith","repo-path":"$2","facts":{facts}}},
                      {{"arrangement":"CarpetX","thorn":"Loop","repo-path":"$2","facts":{facts}}}]}}"#,
            info("head", &c, None)
        );
        let (base, _) = crate::fetch::download::test_server::serve_with(3, move |path| {
            let body = match path {
                p if p.contains("/resolve") => resolved.clone(),
                p if p.ends_with("/versions") => r#"{"slug":"max/carpetx","versions":[]}"#.to_owned(),
                _ => tip.clone(),
            };
            ("200 OK", "api-generation: 1\r\nx-cactarium-untrusted: false\r\n".into(), body.into_bytes())
        });
        let wanted = wanted(&base, &["max/carpetx@head"]).unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(wanted.len(), 1);
        let thorns: Vec<&str> = wanted[0].thorns.iter().map(|t| t.checkout.as_str()).collect();
        assert_eq!(thorns, ["CarpetX/Arith", "CarpetX/Loop"]);
        assert_eq!(wanted[0].version, "head");
        assert_eq!(wanted[0].tag.as_deref(), Some(branch.as_str()), "the default branch, by name");
    }

    /// A Cactarium serving `max/carpetx` read at commit `commit` (no
    /// published version there, so it answers with the full commit as the
    /// version), answering `untrusted` in the header.
    fn serve_commit(commit: &str, untrusted: bool) -> String {
        let facts = serde_json::to_string(&cactup_ecosystem_common::ccl::ThornFacts::default()).unwrap();
        let resolved = resolved(&format!("max/carpetx#{commit}"), "https://x/carpetx.git", commit, commit);
        let read = format!(
            r#"{{{},"url":"https://x/carpetx.git","claims":[],
            "thorns":[{{"arrangement":"CarpetX","thorn":"Arith","repo-path":"$2","facts":{facts}}}]}}"#,
            info(commit, commit, None)
        );
        let headers = format!("api-generation: 1\r\nx-cactarium-untrusted: {untrusted}\r\n");
        let (base, _) = crate::fetch::download::test_server::serve_with(3, move |path| {
            let body = match path {
                p if p.contains("/resolve") => resolved.clone(),
                p if p.ends_with("/versions") => r#"{"slug":"max/carpetx","versions":[]}"#.to_owned(),
                _ => read.clone(),
            };
            ("200 OK", headers.clone(), body.into_bytes())
        });
        base
    }

    /// `owner/pkg#<sha>` (full or abbreviated), or `owner/pkg@<sha>`
    /// Cactarium reads as a commit, that no published version was frozen
    /// at: the package read there, pinned to the full commit id with no
    /// branch label, and the untrusted confirmation applied as for a tip.
    #[test]
    fn a_package_at_a_commit_is_pinned_there() {
        let commit = "0123456789abcdef0123456789abcdef01234567";
        let add = |spec: &str, untrusted: bool| wanted(&serve_commit(commit, untrusted), &[spec]);

        let specs = [format!("max/carpetx#{commit}"), "max/carpetx#0123456".to_owned(), "max/carpetx@0123456789ab".to_owned()];
        for spec in specs {
            let wanted = add(&spec, false).unwrap_or_else(|e| panic!("{spec}: {e:#}"));
            let w = &wanted[0];
            assert_eq!((w.version.as_str(), w.commit.as_str(), w.tag.as_deref()), (commit, commit, None), "{spec}");
        }
        let w = add("max/carpetx#0123456", false).unwrap().remove(0);
        let base = "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n";
        let text = compose(base, &[w]).unwrap();
        assert!(text.contains(&format!("!REPO_COMMIT = {commit}")) && !text.contains("!REPO_BRANCH"), "{text}");
        assert_eq!(describe("max/carpetx", commit), "max/carpetx at commit 0123456789ab");

        let err = format!("{:#}", add("max/carpetx#0123456", true).map(drop).unwrap_err());
        assert!(err.contains("untrusted commit") && err.contains("risky-allow-untrusted-artifacts"), "{err}");
        // An answer at a commit the request does not name is refused.
        let err = format!("{:#}", add("max/carpetx#fffffff", false).map(drop).unwrap_err());
        assert!(err.contains("answered with"), "{err}");

        // A `#<sha>` answered with a version at an unrelated commit, too.
        let other = format!(
            r#"{{{},"url":"https://x/carpetx.git","claims":[],"thorns":[]}}"#,
            info("1.2.0", &"f".repeat(40), Some("v1.2.0"))
        );
        let answer = resolved("max/carpetx@1.2.0", "https://x/carpetx.git", "1.2.0", &"f".repeat(40));
        let (base, _) = crate::fetch::download::test_server::serve_with(3, move |path| {
            let body = match path {
                p if p.contains("/resolve") => answer.clone(),
                p if p.ends_with("/versions") => r#"{"slug":"max/carpetx","versions":[]}"#.to_owned(),
                _ => other.clone(),
            };
            ("200 OK", "api-generation: 1\r\n".into(), body.into_bytes())
        });
        let err = format!("{:#}", wanted(&base, &["max/carpetx#0123456"]).map(drop).unwrap_err());
        assert!(err.contains("answered with max/carpetx@1.2.0"), "{err}");
    }

    /// `add` asks `/resolve` for a package (`kind=package`), so a Cactarium
    /// that knows the parameter says a sim entry is a sim rather than asking
    /// for one of its sims; that message is shown as is.
    #[test]
    fn add_resolves_a_package_as_a_package() {
        let refused = r#"{"error":{"code":"bad-request","message":"et/cottonmouth-sims is a sim, not a package"}}"#;
        let (base, heads) = crate::fetch::download::test_server::serve_with(1, move |_| {
            ("400 Bad Request", "api-generation: 1\r\n".into(), refused.as_bytes().to_vec())
        });
        let err = format!("{:#}", wanted(&base, &["et/cottonmouth-sims"]).map(drop).unwrap_err());
        assert_eq!(err, "et/cottonmouth-sims is a sim, not a package");
        let head = &heads.join().unwrap()[0];
        assert!(head.starts_with("GET /api/v1/resolve?req=et/cottonmouth-sims&kind=package "), "{head}");
    }

    /// A slug's third segment may hold a sim name (`owner/name/qc0-z4c`), but
    /// for a package it is a thorn: anything else is refused before asking
    /// Cactarium (nothing listens at this address).
    #[test]
    fn a_package_spec_names_a_thorn_not_a_sim() {
        let err = format!("{:#}", wanted("http://127.0.0.1:9", &["et/carpetx/Carpet-X"]).map(drop).unwrap_err());
        assert!(err.contains("is not a package") && err.contains("not a thorn name"), "{err}");
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
        let wanted = |version: &str, tag: &str, commit: &str| AddedPackage {
            libraries: Vec::new(),
            flesh: Vec::new(),
            package: "max/carpetx".into(),
            version: version.into(),
            tag: Some(tag.into()),
            commit: commit.into(),
            url: "https://x/carpetx".into(),
            repo: "carpetx".into(),
            thorns: vec![AddedThorn { checkout: "CarpetX/Arith".into(), repo_path: "$2".into() }],
            replaces: Vec::new(),
            contact: None,
        };
        // Same commit, other label: takes the base's label, and plans.
        let mut w = wanted("1.0.0", "v1.0.0", &c);
        let (mut url, mut repo, mut thorns) = (w.url.clone(), String::new(), w.thorns.clone());
        assert!(reuse_base_clone(&base, &mut url, &mut repo, &mut thorns));
        w.url = url;
        w.thorns = thorns;
        share_base_clone(&base, &repo, "max/carpetx@1.0.0", &mut w).unwrap();
        assert_eq!(w.tag.as_deref(), Some("master"));
        let package = AddedPackage { repo: repo.clone(), replaces: vec![repo.clone()], ..w };
        let list = thornlist::parse(&compose(&base_text, &[package]).unwrap()).unwrap();
        let tmp = tempfile::tempdir().unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        crate::fetch::plan(&list, tmp.path(), &mut progress).unwrap_or_else(|e| panic!("{e:#}"));

        // Another commit: refused.
        let mut other = wanted("1.1.0", "v1.1.0", &"d".repeat(40));
        let err = format!("{:#}", share_base_clone(&base, &repo, "max/carpetx@1.1.0", &mut other).unwrap_err());
        assert!(err.contains("which the base keeps at"), "{err}");
        // A tip over a pinned base clone: refused.
        let mut tip = wanted("head", "master", "e");
        assert!(share_base_clone(&base, &repo, "max/carpetx@head", &mut tip).is_err());
    }

    /// A package of another repository whose clone name a new base happens to
    /// use (a user's fork, say, after a refetch) keeps its own source: never
    /// re-pointed at the base's, and the clash is the fetch planner's loud
    /// refusal.
    #[test]
    fn a_package_from_another_repository_keeps_its_source() {
        let mut fork = package("carpetx", &[("CarpetX/Arith", "$2")], &[]);
        fork.url = "https://elsewhere/someone/carpetx.git".into();
        let text = compose(BASE, std::slice::from_ref(&fork)).unwrap();
        let list = thornlist::parse(&text).unwrap();
        let arith = list.components().iter().find(|c| c.checkout == "CarpetX/Arith").unwrap();
        assert_eq!(arith.url.as_deref(), Some("https://elsewhere/someone/carpetx.git"));
        assert_eq!(arith.commit.as_deref(), Some(&*"5".repeat(40)));
        let tmp = tempfile::tempdir().unwrap();
        let mut progress = prodash::tree::Root::new().add_child("plan");
        assert!(crate::fetch::plan(&list, tmp.path(), &mut progress).is_err(), "two sources for one clone");
        assert!(base_clone_of(&fork, &thornlist::parse(BASE).unwrap()).is_none());
    }

    /// A base that no longer has the clone thorns were added to: said, as
    /// they fall back to their own source.
    #[test]
    fn thorns_added_to_a_base_clone_the_new_base_drops_are_reported() {
        let old = thornlist::parse(BASE).unwrap();
        let new = thornlist::parse(&BASE.replace("https://x/carpetx.git", "https://x/other.git").replace("carpetx", "other")).unwrap();
        let more = package("carpetx", &[("CarpetX/Arith", "$2")], &[]);
        let warnings = extensions_left_behind(&old, &new, std::slice::from_ref(&more));
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("max/carpetx added thorns CarpetX/Arith to base repo carpetx"), "{warnings:?}");
        assert!(extensions_left_behind(&old, &old, &[more]).is_empty());
    }

    /// The base's clone is found as Cactarium compares repositories: a
    /// forge that ignores path case is one repository however spelled.
    #[test]
    fn a_base_clone_is_found_however_its_url_is_spelled() {
        let base = thornlist::parse(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = https://github.com/EinsteinToolkit/CarpetX.git\n!REPO_PATH = $2\n!CHECKOUT = CarpetX/CarpetX\n",
        )
        .unwrap();
        let (mut url, mut repo) = ("https://github.com/einsteintoolkit/carpetx".to_owned(), "carpetx".to_owned());
        let mut thorns = vec![AddedThorn { checkout: "CarpetX/Arith".into(), repo_path: "$2".into() }];
        assert!(reuse_base_clone(&base, &mut url, &mut repo, &mut thorns));
        assert_eq!(repo, "CarpetX");
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
            flesh: Vec::new(),
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
        let wanted = wanted(&base, &["et/kadaththorn"]).unwrap_or_else(|e| panic!("{e:#}"));
        let libs = &wanted[0].libraries;
        assert_eq!(libs.len(), 1);
        let l = &libs[0];
        assert_eq!(
            (l.package.as_str(), l.version.as_str(), l.commit.as_str(), l.inside.as_str(), l.into.as_str(), l.tag.as_deref()),
            ("et/fuka", "2026.5.1", &*"b".repeat(40), "KadathThorn", "src/fuka", Some("ET_2026_05_v1"))
        );
        assert_eq!(l.clone_name(), "fuka");
        let thorns: Vec<&str> = wanted[0].thorns.iter().map(|t| t.checkout.as_str()).collect();
        assert_eq!(thorns, ["Fuka/KadathThorn"], "the library adds no thorns");
    }

    /// The registry's `into` is checked again here: a path out of the
    /// dependent is refused before anything else is asked.
    #[test]
    fn a_registry_into_that_leaves_the_repo_is_refused() {
        let base = serve_kadath("et/fuka", "../../escape", true, 4);
        let err = wanted(&base, &["et/kadaththorn"]).map(drop).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("places et/fuka at \"../../escape\"") && err.contains("not a plain folder name"), "{err}");
    }

    /// `into` on a dependency whose target is no library is refused as that.
    #[test]
    fn into_on_a_package_with_thorns_is_refused() {
        let base = serve_kadath("et/fuka", "src/fuka", false, 4);
        let err = wanted(&base, &["et/kadaththorn"]).map(drop).unwrap_err();
        assert!(format!("{err:#}").contains("et/fuka is not a library"), "{err:#}");
    }

    /// `cactup add et/fuka`: a library on its own has nowhere to go; said
    /// as that, not as an empty package.
    #[test]
    fn adding_a_library_alone_says_what_it_is() {
        let base = serve_kadath("et/fuka", "src/fuka", true, 2);
        let err = wanted(&base, &["et/fuka"]).map(drop).unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("et/fuka is a library, which has no thorns"), "{err}");
        assert!(err.contains("Add a package that builds against it"), "{err}");
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
        assert!(err.contains("has no fetch record") && err.contains("Move it aside"), "{err}");
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

    /// `remove` of exactly what is named, asking Cactarium nothing.
    const NO_DEPS: RemoveFlags = RemoveFlags { force: false, no_deps: true, yes: false, dry_run: false };

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
                        copied_from: None,
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
            std::fs::create_dir_all(it.inst.live_thornlist().unwrap().parent().unwrap()).unwrap();
            std::fs::write(it.inst.live_thornlist().unwrap(), &composed).unwrap();
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

        let err = format!("{:#}", remove(&it.ctx, &["et/kadaththorn".to_owned()], &NO_DEPS).unwrap_err());
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
        remove(&it.ctx, &["et/kadaththorn".to_owned()], &NO_DEPS).unwrap_or_else(|e| panic!("{e:#}"));
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
        let (base, _) = crate::fetch::download::test_server::serve_with(8, move |path| {
            // Version 2026.5.0, which the installation has, is no longer served.
            if path.contains("/versions/") {
                return ("404 Not Found", "api-generation: 1\r\n".into(), br#"{"error":{"code":"not-found","message":"no"}}"#.to_vec());
            }
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
        dispatch(&it.ctx, &["et/kadaththorn".to_owned()], &AddFlags::default()).unwrap_or_else(|e| panic!("{e:#}"));
        assert!(std::fs::symlink_metadata(it.kadath_dir().join("src/fuka")).is_err(), "the old link is gone");
        assert_eq!(std::fs::read_link(it.kadath_dir().join("ext/fuka")).unwrap(), std::path::Path::new("../../fuka"));
        let exclude = it.exclude();
        assert!(!exclude.contains("/src/fuka") && exclude.contains("/ext/fuka"), "{exclude}");
        assert_eq!(it.inst.meta().unwrap().added[0].libraries[0].into, "ext/fuka");
    }

    /// `cactup add et/kadaththorn` again at the version it is at (whole, or
    /// one of its thorns): a re-add, which keeps its library placed where it
    /// is, link and record alike.
    #[test]
    fn a_readd_keeps_the_package_s_libraries() {
        for spec in ["et/kadaththorn", "et/kadaththorn/KadathThorn"] {
            let it = Installed::new("src/fuka", "", &[]);
            let facts = serde_json::to_string(&cactup_ecosystem_common::ccl::ThornFacts::default()).unwrap();
            let (kurl, furl) = (it.kadath_url.clone(), it.fuka_url.clone());
            let kadath = format!(
                r#"{{{},"url":"{kurl}","thorns":[{{"arrangement":"Fuka","thorn":"KadathThorn","repo-path":"../$2","facts":{facts}}}],
                "claims":[{{"kind":"dependency","target":"et/fuka","range":"^2026.5.0","into":"src/fuka","note":null}}]}}"#,
                info("2026.5.0", &it.k, None)
            );
            let fuka = format!(r#"{{{},"url":"{furl}","library":true,"thorns":[],"claims":[]}}"#, info("2026.5.0", &it.f, None));
            let resolve_kadath = resolved("et/kadaththorn@2026.5.0", &kurl, "2026.5.0", &it.k);
            let (base, _) = crate::fetch::download::test_server::serve_with(20, move |path| {
                let body = match path {
                    p if p.contains("/resolve") => resolve_kadath.clone(),
                    p if p.ends_with("/packages/et/kadaththorn/versions") => {
                        format!(r#"{{"slug":"et/kadaththorn","versions":[{kadath}]}}"#)
                    }
                    p if p.contains("/packages/et/kadaththorn/versions/") => kadath.clone(),
                    p if p.ends_with("/packages/et/fuka/versions") => format!(r#"{{"slug":"et/fuka","versions":[{fuka}]}}"#),
                    _ => fuka.clone(),
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
            dispatch(&it.ctx, &[spec.to_owned()], &AddFlags::default()).unwrap_or_else(|e| panic!("{spec}: {e:#}"));
            let link = it.kadath_dir().join("src/fuka");
            assert!(std::fs::symlink_metadata(&link).is_ok_and(|m| m.file_type().is_symlink()), "{spec}: the link stays");
            assert!(it.exclude().contains("/src/fuka"), "{spec}");
            assert_eq!(it.inst.meta().unwrap().added[0].libraries.len(), 1, "{spec}");
        }
    }

    #[test]
    fn a_yank_warning_says_why() {
        assert_eq!(
            yank_warning("et/x@1.0.0", Some("breaks Loop")),
            "et/x@1.0.0 is yanked: breaks Loop. Using it because it was asked for exactly that version."
        );
        assert!(yank_warning("et/x@1.0.0", None).starts_with("et/x@1.0.0 is yanked. Using"));
        assert_eq!(
            library_yank_warning("et/fuka@1.0.0", None, &["et/kadath@2.0.0"]),
            "The library et/fuka@1.0.0 is yanked. Using it because et/kadath@2.0.0 asks for exactly that version."
        );
        assert!(library_yank_warning("l@1", None, &["a@1", "b@1"]).contains("a@1 and b@1 ask for"));
        assert_eq!(
            unchecked_line(1, "moving et/admbase (in place of base repo admbase)"),
            "Cannot check whether moving et/admbase (in place of base repo admbase) breaks 1 base package whose \
             dependencies are unknown here."
        );
        let release: crate::deps::Release = {
            let v = format!(
                r#"{{{},"url":"https://x/admbase.git","thorns":[],"claims":[]}}"#,
                info("2.0.0", &"a".repeat(40), None)
            );
            let v: cactup_ecosystem_common::api::PackageVersion = serde_json::from_str(&v).unwrap();
            let id = crate::deps::PkgId::parse("et/admbase").unwrap();
            crate::deps::Release::from_served(&id, &v, None).unwrap()
        };
        let plan = crate::deps::Plan {
            steps: vec![
                crate::deps::Step::Upgrade {
                    from: release.clone(),
                    to: release.clone(),
                    thorns: Vec::new(),
                    why: crate::deps::Why::Named,
                    flag: None,
                },
                crate::deps::Step::Replace {
                    replaced: crate::deps::Replaced::Base { repo: "admbase".into() },
                    by: release.id.clone(),
                    thorns: Vec::new(),
                    flesh: false,
                    explicit: true,
                },
            ],
            ..Default::default()
        };
        assert_eq!(moves_of(&plan), ["et/admbase (in place of base repo admbase)"]);
        assert!(unchecked_line(3, "removing max/a").ends_with("breaks 3 base packages whose dependencies are unknown here."));
        assert_eq!(unchecked_short(&["max/a", "et/b"]), "Not checked: whether base packages at unreleased commits need max/a, et/b.");
        // A moved base package is no longer one whose dependencies are unknown.
        let unchecked = ["et/admbase#abababababab".to_owned(), "et/cactusbase#cacacacacaca".to_owned()];
        let moved = moves_of(&plan);
        let moved: Vec<&str> = moved.iter().map(|m| m.split(' ').next().unwrap()).collect();
        assert_eq!(still_unchecked(&unchecked, &moved), 1);
    }

    /// `cactup add` again at a version that moves FUKA to `ext/fuka` and also
    /// wants a second library, whose repository cannot be fetched: the add
    /// fails, and leaves exactly what was there before: the old link and
    /// exclude line back, the new link (placed before the failure was
    /// known) gone, the record unchanged.
    #[test]
    fn an_add_whose_fetch_fails_leaves_the_prior_placement() {
        let it = Installed::new("src/fuka", "", &[]);
        let facts = serde_json::to_string(&cactup_ecosystem_common::ccl::ThornFacts::default()).unwrap();
        let info = |version: &str, commit: &str| {
            format!(
                r#""version":"{version}","track":null,"tag":null,"commit":"{commit}","tree":null,"yanked":false,
                "yank-reason":null,"state":"ok","curated":false,"published":0"#
            )
        };
        let (kurl, furl) = (it.kadath_url.clone(), it.fuka_url.clone());
        let gone = it.tmp.path().join("up/missing/eigen").display().to_string();
        let ec = "c".repeat(40);
        let resolve_kadath = format!(
            r#"{{"slug":"et/kadaththorn@2026.6.0","kind":"package","url":"{kurl}","contact":null,{}}}"#,
            info("2026.6.0", &it.k)
        );
        let kadath_versions = format!(
            r#"{{"slug":"et/kadaththorn","versions":[{{{},"url":"{kurl}",
            "thorns":[{{"arrangement":"Fuka","thorn":"KadathThorn","repo-path":"../$2","facts":{facts}}}],
            "claims":[{{"kind":"dependency","target":"et/fuka","range":"^2026.5.0","into":"ext/fuka","note":null}},
                      {{"kind":"dependency","target":"et/eigen","range":"^3.4.0","into":"vendor/eigen","note":null}}]}}]}}"#,
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
        let resolve_eigen = format!(
            r#"{{"slug":"et/eigen@3.4.0","kind":"package","url":"{gone}","contact":null,{}}}"#,
            info("3.4.0", &ec)
        );
        let eigen_versions = format!(
            r#"{{"slug":"et/eigen","versions":[{{{},"url":"{gone}","library":true,"thorns":[],"claims":[]}}]}}"#,
            info("3.4.0", &ec)
        );
        let (base, _) = crate::fetch::download::test_server::serve_with(10, move |path| {
            if path.contains("/versions/") {
                return ("404 Not Found", "api-generation: 1\r\n".into(), br#"{"error":{"code":"not-found","message":"no"}}"#.to_vec());
            }
            let body = match path {
                p if p.contains("/resolve") && p.contains("kadaththorn") => resolve_kadath.clone(),
                p if p.contains("/resolve") && p.contains("eigen") => resolve_eigen.clone(),
                p if p.contains("/resolve") => resolve_fuka.clone(),
                p if p.contains("/packages/et/kadaththorn/") => kadath_versions.clone(),
                p if p.contains("/packages/et/eigen/") => eigen_versions.clone(),
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
        let exclude_before = it.exclude();
        let err = format!("{:#}", dispatch(&it.ctx, &["et/kadaththorn".to_owned()], &AddFlags::default()).unwrap_err());
        assert!(err.contains("Failed to fetch") && err.contains("eigen"), "{err}");
        assert_eq!(std::fs::read_link(it.kadath_dir().join("src/fuka")).unwrap(), std::path::Path::new("../../fuka"));
        assert!(std::fs::symlink_metadata(it.kadath_dir().join("ext/fuka")).is_err(), "the new link went");
        assert_eq!(it.exclude(), exclude_before);
        let meta = it.inst.meta().unwrap();
        assert_eq!(meta.added[0].libraries.len(), 1);
        assert_eq!(meta.added[0].libraries[0].into, "src/fuka");
    }

    /// A refetch whose prune fails after the fetch went through keeps what
    /// it did: the new list is written first, so the placement it drops
    /// stays dropped (here the base took FUKA over).
    #[test]
    fn a_refetch_whose_prune_fails_keeps_the_new_list() {
        use std::os::unix::fs::PermissionsExt;
        let it = Installed::new("src/fuka", "", &[]);
        // An interrupted prune's leftover that cannot be deleted.
        let stuck = it.inst.root.join(".cactup/pruning/old.1/locked");
        std::fs::create_dir_all(&stuck).unwrap();
        std::fs::write(stuck.join("f"), "x").unwrap();
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o555)).unwrap();
        if std::fs::remove_file(stuck.join("f")).is_ok() {
            return; // root: permissions do not bind
        }
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
            "--prune",
            file.to_str().unwrap(),
        ])
        .unwrap()
        .command
        {
            crate::args::Commands::Inst(crate::args::InstallationCommand::Refetch(args)) => args,
            other => panic!("{other:?}"),
        };
        let result = super::super::refetch::dispatch(&it.ctx, args);
        std::fs::set_permissions(&stuck, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(result.is_err(), "the prune fails");
        let live = std::fs::read_to_string(it.inst.live_thornlist().unwrap()).unwrap();
        assert!(live.contains("$ARR/Fuka/KadathThorn/src/fuka/../../../../repos/KadathThorn/src/fuka"), "{live}");
        assert!(!it.exclude().contains("/src/fuka"), "the dropped placement stays dropped: {}", it.exclude());
        assert!(it.kadath_dir().join("src/fuka/Cmake/CMakeLists.txt").is_file());
    }

    /// Putting the root's links back tries every one: a link that cannot be
    /// made (its directory gone) is said, and the next is put back all the same.
    #[test]
    fn every_root_link_is_put_back_that_can_be() {
        let tmp = tempfile::tempdir().unwrap();
        let (gone, kept) = (tmp.path().join("gone/Makefile"), tmp.path().join("lib"));
        std::os::unix::fs::symlink("repos/cactus-mp/lib", &kept).unwrap();
        let links = RootLinks {
            saved: vec![(gone, Some("repos/flesh/Makefile".into())), (kept.clone(), Some("repos/flesh/lib".into()))],
            armed: false,
        };
        let failed = links.put_back();
        assert_eq!(failed.len(), 1, "{failed:?}");
        assert!(failed[0].contains("gone/Makefile"), "{failed:?}");
        assert_eq!(std::fs::read_link(&kept).unwrap(), std::path::Path::new("repos/flesh/lib"));
    }

    /// What `remove` and `add` say about a flesh package: the plan's rebuild
    /// line and the last line, and the clone a flesh package leaves on disk
    /// (one replacing a base repo still has a clone of its own), in the
    /// singular for one repo.
    #[test]
    fn what_a_flesh_change_says() {
        let plan = crate::deps::RemovePlan {
            removed: vec![crate::deps::PkgId::parse("max/mp").unwrap()],
            removed_labels: vec!["max/mp@1.0.0".into()],
            orphans: Vec::new(),
            took: Default::default(),
        };
        let lines = removal_lines(&plan, "cactup remove max/mp", true);
        assert_eq!(lines.last().unwrap(), &(crate::deps::LineKind::Rebuild, crate::deps::FLESH_REBUILD.to_owned()));
        assert!(!removal_lines(&plan, "cactup remove max/mp", false).iter().any(|(k, _)| *k == crate::deps::LineKind::Rebuild));
        assert!(build_hint(Some("the base's flesh"), true).contains("Rebuild every config for the base's flesh, e.g."));

        let base = thornlist::parse(BASE).unwrap();
        let mut mp = package("cactus-mp", &[], &["flesh"]);
        assert!(left_on_disk(std::slice::from_ref(&mp), &base).is_empty(), "a replacing package goes back");
        mp.flesh = vec!["Makefile".into()];
        assert_eq!(left_on_disk(std::slice::from_ref(&mp), &base), ["cactus-mp"]);
        assert_eq!(
            left_lines(&["cactus-mp".into()], Some(&["max/mp"])),
            ["The repo cactus-mp of the replaced max/mp stays on disk.", "Remove it with `cactup inst refetch --prune`."]
        );
        assert_eq!(left_lines(&["a".into()], None)[0], "Its repo a stays on disk.");
        assert_eq!(
            left_lines(&["a".into(), "b".into()], None),
            ["Their repos a, b stay on disk.", "Remove them with `cactup inst refetch --prune`."]
        );
    }
}

#[cfg(test)]
mod deps_tests;
