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
use crate::installation::{AddedPackage, AddedThorn, Installation};
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
    let emitted = cactup_ecosystem_common::crl_emit::emit(root, &components, &[], None);
    let sections = emitted.find("!TARGET").map(|i| &emitted[i..]).unwrap_or("");
    out.push_str(sections);
    thornlist::parse(&out).context("The thornlist with the added packages does not parse")?;
    Ok(out)
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

/// Whether an added package's version is a branch tip (`head`, `master`):
/// its default branch, followed rather than pinned.
pub(crate) fn is_tip(version: &str) -> bool {
    matches!(version, "head" | "master")
}

/// The names the Cactarium gives a thorn become paths on disk and words in a
/// thornlist (which GetComponents also hands to a shell, single-quoted), so
/// they are checked before anything is written: an arrangement and a thorn
/// are single plain path components; a `!REPO_PATH` is plain path
/// characters, `$1`/`$2`, and whatever clone name it implies is a single
/// plain component too.
fn check_registry_names(slug: &str, arrangement: &str, thorn: &str, repo_path: &str) -> Res<()> {
    let plain = |s: &str| {
        !s.is_empty() && s != "." && s != ".." && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
    };
    if !plain(arrangement) || !plain(thorn) {
        bail!("The Cactarium names a thorn of {slug} \"{arrangement}/{thorn}\", which is not a plain Arrangement/Thorn");
    }
    let path_ok = repo_path.bytes().all(|b| b.is_ascii_alphanumeric() || b"._-/$".contains(&b));
    let checkout = format!("{arrangement}/{thorn}");
    let clone = clone_of("package", &crate::installation::AddedThorn { checkout, repo_path: repo_path.to_owned() });
    if !path_ok || !plain(&clone) {
        bail!("The Cactarium gives {arrangement}/{thorn} of {slug} the repo-path \"{repo_path}\", which cactup will not write");
    }
    Ok(())
}

/// Every clone a package is fetched into.
fn package_repos(a: &AddedPackage) -> BTreeSet<String> {
    a.thorns.iter().map(|t| clone_of(&a.repo, t)).collect()
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
}

/// Resolve every spec, merging the ones of one package: several thorns of
/// it (`owner/pkg/A owner/pkg/B`) are one add of those thorns, and naming
/// the package whole anywhere adds all of them. Two specs that resolve to
/// different versions of one package are an error, not a silent winner.
fn resolve_all(client: &crate::cactarium::Client, specs: &[String], allow_untrusted: bool) -> Res<Vec<Wanted>> {
    let mut wanted: Vec<Wanted> = Vec::new();
    let mut listings: std::collections::HashMap<String, cactup_ecosystem_common::api::PackageVersions> =
        std::collections::HashMap::new();
    for spec in specs {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let slug: Slug =
            spec.parse().map_err(|e| anyhow!("\"{spec}\" is not a package (owner/name[/Thorn][@version]): {e}"))?;
        let entry = slug.entry();
        let req = match &slug.at {
            Some(at) => format!("{entry}@{at}"),
            None => entry.clone(),
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
        let version = if let Some(tip) = slug.at.as_deref().filter(|_| slug.tip().is_some()) {
            let (version, untrusted) = client.package_tip(slug.owner.as_str(), slug.name.as_str(), tip)?;
            if untrusted {
                crate::cactarium::confirm_untrusted(&resolved.slug, "package", allow_untrusted)?;
            }
            let scratch = tempfile::tempdir().context("Failed to create a scratch directory")?;
            gix::init_bare(scratch.path()).context("Failed to create a scratch repository")?;
            let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
            let line = crate::progress::Line::over(
                progress.add_child(resolved.slug.clone()),
                &resolved.slug,
                crate::progress::Layout::for_names([resolved.slug.as_str()]),
            );
            line.phase("asking its repository for its default branch");
            let branch = crate::fetch::git::remote_default_branch(scratch.path(), &url);
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
        let thorns: Vec<AddedThorn> = version
            .thorns
            .iter()
            .filter(|t| slug.thorn.as_deref().is_none_or(|want| want.eq_ignore_ascii_case(&t.thorn)))
            .map(|t| AddedThorn { checkout: format!("{}/{}", t.arrangement, t.thorn), repo_path: t.repo_path.clone() })
            .collect();
        for t in &version.thorns {
            check_registry_names(&resolved.slug, &t.arrangement, &t.thorn, &t.repo_path)?;
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
        match wanted.iter_mut().find(|w| w.entry == entry) {
            Some(w) if w.version != resolved.version.version => bail!(
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
                version: resolved.version.version.clone(),
                tag: label,
                commit,
                url,
                thorns,
                contact: resolved.contact.clone(),
                only_named,
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

pub fn dispatch(ctx: &Ctx, specs: &[String], replace: bool, force: bool) -> Res<()> {
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let wanted = resolve_all(&client, specs, ctx.globals.risky_allow_untrusted_artifacts)?;

    let inst = Installation::resolve(ctx)?;
    let locked = inst.locked()?;
    let mut meta = locked.meta()?;
    let pristine = std::fs::read_to_string(inst.source_thornlist_to_read())
        .context("This installation has no pristine thornlist to add to")?;
    let live_path = inst.live_thornlist();
    let live = std::fs::read_to_string(inst.live_thornlist_to_read()).unwrap_or_default();
    // The live list must be what cactup composed, or the add would silently
    // discard edits made to it by hand.
    let edits = super::refetch::component_divergence(&live, &compose(&pristine, &meta.added)?);
    if !edits.is_empty() && !force {
        bail!(
            "'{}' has hand edits, which adding would overwrite:\n  {}\nMove them into a thornlist of your own, or pass -f",
            live_path.display(),
            edits.join("\n  ")
        );
    }

    let base = thornlist::parse(&pristine)?;
    let mut added: Vec<AddedPackage> = Vec::new();
    for w in wanted {
        // What in the base the package collides with: the same repository,
        // or a thorn it also checks out.
        let mut repo = repo_name(&w.url);
        let mut thorns = w.thorns;
        let mut url = w.url;
        let reused = reuse_base_clone(&base, &mut url, &mut repo, &mut thorns);
        let w = Wanted { thorns, url, ..w };
        if reused {
            // One clone is at one commit: the base's other checkouts of it
            // stay at the base's pin, so the package must be at the same one.
            let claimed: BTreeSet<&str> = w.thorns.iter().map(|t| t.checkout.as_str()).collect();
            let stay: Vec<&thornlist::Component> =
                base.components().iter().filter(|c| c.repo == repo && !claimed.contains(c.checkout.as_str())).collect();
            let at = if is_tip(&w.version) { None } else { Some(w.commit.as_str()) };
            if let Some(other) = stay.iter().find(|c| c.commit.as_deref() != at) {
                let names: Vec<&str> = stay.iter().map(|c| c.checkout.as_str()).collect();
                bail!(
                    "{} would share the clone {repo} with the base's {}, which stays at {}; one clone holds one \
                     commit. Add a version at that commit, or replace those thorns too",
                    w.slug,
                    names.join(", "),
                    other.commit.as_deref().map_or_else(|| "its branch".to_owned(), |c| c[..12.min(c.len())].to_owned())
                );
            }
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
        };
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

    // Fetch those repos — never the rest of the installation.
    let repos: BTreeSet<String> = added.iter().flat_map(package_repos).collect();
    let repos: BTreeSet<&str> = repos.iter().map(String::as_str).collect();
    let list = thornlist::parse(&composed)?;
    let (progress, renderer) = crate::manifest::setup_prodash();
    let mut classify = progress.add_child("classify repos");
    let mut plan = crate::fetch::plan(&list, &inst.root, &mut classify)?;
    drop(classify);
    renderer.shutdown_and_wait();
    if let Some(skipped) = plan.skipped.iter().find(|s| repos.contains(s.repo.as_str())) {
        bail!("Repo {} in the installation has {}; it was left alone", skipped.repo, skipped.reason.describe());
    }
    plan.git.retain(|g| repos.contains(g.repo.as_str()));
    plan.links.retain(|c| repos.contains(c.repo.as_str()));
    plan.downloads.clear();
    plan.external.clear();
    plan.skipped.clear();
    let report = crate::fetch::execute(&plan, &inst.root)?;
    if !report.failures.is_empty() {
        let why: Vec<String> = report.failures.iter().map(|f| format!("{}: {}", f.what, f.error)).collect();
        bail!("Failed to fetch {}:\n  {}", repos.iter().copied().collect::<Vec<_>>().join(", "), why.join("\n  "));
    }
    crate::fetch::FetchState::record(&inst.root, &report.records())?;

    std::fs::write(&live_path, &composed).with_context(|| format!("Failed to write '{}'", live_path.display()))?;
    locked.set_meta(&meta)?;
    drop(locked);

    for package in &added {
        let checkouts: Vec<&str> = package.thorns.iter().map(|t| t.checkout.as_str()).collect();
        let short = &package.commit[..12.min(package.commit.len())];
        let at = if is_tip(&package.version) {
            format!("following its default branch, now at {short}")
        } else {
            format!("at {short}")
        };
        println!(
            "Added {} ({}) {at}{}.",
            format!("{}@{}", package.package, package.version).bold(),
            checkouts.join(", "),
            if package.replaces.is_empty() { String::new() } else { format!(", in place of {}", package.replaces.join(", ")) }
        );
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
    if slug.at.is_some() || slug.thorn.is_some() {
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
    let edits = super::refetch::component_divergence(&live, &compose(&pristine, &meta.added)?);
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
        plan.git.retain(|g| touched.contains(g.repo.as_str()));
        plan.links.retain(|c| touched.contains(c.repo.as_str()));
        plan.downloads.clear();
        plan.external.clear();
        plan.skipped.clear();
        let report = crate::fetch::execute(&plan, &inst.root)?;
        if !report.failures.is_empty() {
            let why: Vec<String> = report.failures.iter().map(|f| format!("{}: {}", f.what, f.error)).collect();
            bail!("Failed to restore {}:\n  {}", removed.replaces.join(", "), why.join("\n  "));
        }
        crate::fetch::FetchState::record(&inst.root, &report.records())?;
    }
    std::fs::write(&live_path, &composed).with_context(|| format!("Failed to write '{}'", live_path.display()))?;
    locked.set_meta(&meta)?;
    drop(locked);

    println!("Removed {} from the thornlist.", removed.package.bold());
    if touched.is_empty() {
        let repos: Vec<String> = package_repos(&removed).into_iter().collect();
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
    fn registry_names_must_be_plain() {
        check_registry_names("et/x", "ExternalLibraries", "HDF5", "../$1-$2").unwrap();
        check_registry_names("et/x", "CarpetX", "Arith", "$2").unwrap();
        for (arr, thorn, path) in
            [("..", "T", "$2"), ("A/B", "T", "$2"), ("A", "T'", "$2"), ("A", "T", "../.."), ("A", "T", "$2;rm"), ("A", "T", "a b")]
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
        let wanted = resolve_all(&crate::cactarium::Client::at(&base), &["max/carpetx@head".to_owned()], false)
            .unwrap_or_else(|e| panic!("{e:#}"));
        assert_eq!(wanted.len(), 1);
        let thorns: Vec<&str> = wanted[0].thorns.iter().map(|t| t.checkout.as_str()).collect();
        assert_eq!(thorns, ["CarpetX/Arith", "CarpetX/Loop"]);
        assert_eq!(wanted[0].tag.as_deref(), Some(branch.as_str()), "the default branch, by name");
    }

    #[test]
    fn repo_names_follow_the_url() {
        assert_eq!(repo_name("file:///repos/carpetx.git"), "carpetx");
        assert_eq!(repo_name("git@github.com:max/SpacetimeX.git"), "SpacetimeX");
        assert_eq!(repo_name("https://x/y/z/"), "z");
    }
}
