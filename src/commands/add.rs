//! `cactup add owner/pkg[/Thorn][@range]` (spec §8.11): put a Cactarium
//! package's thorns into the active installation, at the commit the registry
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
                let thorn = t.checkout.rsplit('/').next().unwrap_or(&t.checkout);
                // `$2` (the thorn's name) when the thorn's directory is named
                // for it, so a package's thorns share one section.
                let repo_path = if t.repo_path == thorn { "$2".to_owned() } else { t.repo_path.clone() };
                Component {
                    ty: ComponentType::Git,
                    target: format!("{root}/arrangements"),
                    checkout: t.checkout.clone(),
                    name: Some(a.repo.clone()),
                    url: Some(a.url.clone()),
                    auth_url: None,
                    anon_user: None,
                    anon_pass: None,
                    repo_path: Some(repo_path),
                    branch: a.tag.clone(),
                    commit: Some(a.commit.clone()),
                    fetch_only: false,
                    repo: a.repo.clone(),
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

/// A repo's name as a thornlist derives it from its URL (no `!NAME`): the
/// last path segment, `.git` stripped.
fn repo_name(url: &str) -> String {
    let stripped = url.trim_end_matches('/').trim_end_matches(".git");
    stripped.rsplit(['/', ':']).next().unwrap_or(stripped).to_owned()
}

pub fn dispatch(ctx: &Ctx, package: &str, replace: bool, force: bool) -> Res<()> {
    let slug: Slug = package.parse().map_err(|e| anyhow!("\"{package}\" is not a package (owner/name[/Thorn][@version]): {e}"))?;
    let entry = format!("{}/{}", slug.owner, slug.name);
    let req = match &slug.at {
        Some(at) => format!("{entry}@{at}"),
        None => entry.clone(),
    };
    let client = crate::cactarium::Client::new(&ctx.db.read()?);
    let resolved = client.resolve(&req)?;
    if resolved.kind != "package" {
        bail!("{} is a {}, not a package; install a stack with `cactup install`", resolved.slug, resolved.kind);
    }
    let commit = resolved.version.commit.clone().ok_or_else(|| anyhow!("The Cactarium gave {} no commit", resolved.slug))?;
    let url = resolved.url.clone().ok_or_else(|| anyhow!("The Cactarium gave {} no repository", resolved.slug))?;
    let versions = client.package_versions(slug.owner.as_str(), slug.name.as_str())?;
    let version = versions
        .versions
        .iter()
        .find(|v| v.version.version == resolved.version.version)
        .ok_or_else(|| anyhow!("The Cactarium lists no thorns for {}", resolved.slug))?;
    let thorns: Vec<AddedThorn> = version
        .thorns
        .iter()
        .filter(|t| slug.thorn.as_deref().is_none_or(|want| want.eq_ignore_ascii_case(&t.thorn)))
        .map(|t| AddedThorn { checkout: format!("{}/{}", t.arrangement, t.thorn), repo_path: t.repo_path.clone() })
        .collect();
    if thorns.is_empty() {
        let all: Vec<&str> = version.thorns.iter().map(|t| t.thorn.as_str()).collect();
        bail!(
            "{} has no thorn {} (its thorns: {})",
            resolved.slug,
            slug.thorn.as_deref().unwrap_or("at all"),
            all.join(", ")
        );
    }

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

    // What in the base the package collides with: the same repository, or a
    // thorn it also checks out.
    let repo = repo_name(&url);
    let base = thornlist::parse(&pristine)?;
    let want: BTreeSet<&str> = thorns.iter().map(|t| t.checkout.as_str()).collect();
    let normalized = crate::fetch::git::normalize_url(&url);
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
            resolved.slug
        );
    }

    // Record it: a second add of the same package updates it, adding a thorn
    // to the ones already enabled when only a thorn was named.
    let mut package = AddedPackage {
        package: entry.clone(),
        version: resolved.version.version.clone(),
        tag: resolved.version.tag.clone(),
        commit,
        url,
        repo: repo.clone(),
        thorns,
        replaces: conflicts.into_iter().collect(),
    };
    if let Some(i) = meta.added.iter().position(|a| a.package == entry) {
        let old = meta.added.remove(i);
        if slug.thorn.is_some() {
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
    let composed = compose(&pristine, &meta.added)?;

    // Fetch that one repo — never the rest of the installation.
    let list = thornlist::parse(&composed)?;
    let (progress, renderer) = crate::manifest::setup_prodash();
    let mut classify = progress.add_child("classify repos");
    let mut plan = crate::fetch::plan(&list, &inst.root, &mut classify)?;
    drop(classify);
    renderer.shutdown_and_wait();
    if let Some(skipped) = plan.skipped.iter().find(|s| s.repo == repo) {
        bail!("Repo {repo} in the installation has {}; it was left alone", skipped.reason.describe());
    }
    plan.git.retain(|g| g.repo == repo);
    plan.links.retain(|c| c.repo == repo);
    plan.downloads.clear();
    plan.external.clear();
    plan.skipped.clear();
    let report = crate::fetch::execute(&plan, &inst.root)?;
    if !report.failures.is_empty() {
        let why: Vec<String> = report.failures.iter().map(|f| format!("{}: {}", f.what, f.error)).collect();
        bail!("Failed to fetch {}:\n  {}", resolved.slug, why.join("\n  "));
    }
    crate::fetch::FetchState::record(&inst.root, &report.records())?;

    std::fs::write(&live_path, &composed).with_context(|| format!("Failed to write '{}'", live_path.display()))?;
    locked.set_meta(&meta)?;
    drop(locked);

    let checkouts: Vec<&str> = package.thorns.iter().map(|t| t.checkout.as_str()).collect();
    println!(
        "Added {} ({}) at {}{}.",
        resolved.slug.bold(),
        checkouts.join(", "),
        &package.commit[..12.min(package.commit.len())],
        if package.replaces.is_empty() { String::new() } else { format!(", in place of {}", package.replaces.join(", ")) }
    );
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
        println!("Its repo {} stays on disk; `cactup inst refetch --prune` removes it.", removed.repo);
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
        let out = compose(BASE, &[package("spacetimex", &[("SpacetimeX/Z4c", "Z4c"), ("SpacetimeX/ADM", "ADM")], &[])]).unwrap();
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
        let fork = package("carpetx-fork", &[("CarpetX/CarpetX", "CarpetX"), ("CarpetX/Loop", "Loop")], &["carpetx"]);
        let out = compose(BASE, std::slice::from_ref(&fork)).unwrap();
        let list = thornlist::parse(&out).unwrap();
        let loop_from: Vec<&str> =
            list.components().iter().filter(|c| c.checkout == "CarpetX/Loop").map(|c| c.repo.as_str()).collect();
        assert_eq!(loop_from, ["carpetx-fork"]);
        assert!(list.components().iter().any(|c| c.checkout == "CactusBase/IOUtil"), "the rest of the base stays");

        let unrecorded = AddedPackage { replaces: vec![], ..fork };
        assert!(compose(BASE, &[unrecorded]).is_err(), "two repos claiming one checkout must not compose");
    }

    #[test]
    fn repo_names_follow_the_url() {
        assert_eq!(repo_name("file:///repos/carpetx.git"), "carpetx");
        assert_eq!(repo_name("git@github.com:max/SpacetimeX.git"), "SpacetimeX");
        assert_eq!(repo_name("https://x/y/z/"), "z");
    }
}
