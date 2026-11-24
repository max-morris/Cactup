//! `cactup stack init` (D5): a `cactarium-stack.toml` for the installation, extending the stack
//! release it is on, and `--publish --version`, which writes nothing and publishes the definition
//! found there as that release.
//!
//! What installs is pinned exactly: base repos moved since the release, packages that replace a
//! base repo as overrides (at their exact version, or at the commit fetched for a tip or a commit
//! no version froze), and every other added package as a pinned section of a generated thornlist.
//! The dependencies the installed packages declare follow as information: what could be swapped
//! in with a good chance of the stack still working (decision 10). Nothing is invented.
// Plan §4.7, misfit 7, decision 10, the P7 row of §7.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use cactup_ecosystem_common::manifest::{Manifest, STACK_FILE};
use cactup_ecosystem_common::name::Name;
use cactup_ecosystem_common::slug::Slug;
use cactup_ecosystem_common::version::{TrackedReq, TrackedVersion};
use colored::Colorize;

use super::package::{q, qa};
use super::resolution::{self, PresentOptions, Registry};
use super::Ctx;
use crate::args::StackInitArgs;
use crate::cactarium::Client;
use crate::deps::At;
use crate::installation::{AddedPackage, Installation};
use crate::sim::export;
use crate::thornlist;
use crate::Res;

/// The generated thornlist's file name, beside the definition.
pub const THORNLIST_FILE: &str = "cactarium-stack.th";

/// `cactup stack init [DIR]`, or with `--publish` the release of the definition there.
pub fn init(ctx: &Ctx, args: &StackInitArgs) -> Res<()> {
    let dir = args.dir.clone().unwrap_or_else(|| PathBuf::from("."));
    if args.publish {
        let version = args.version.as_deref().ok_or_else(|| anyhow!("Name the release with --version."))?;
        return publish(ctx, &dir, args.owner.as_deref(), version);
    }
    let inst = Installation::resolve(ctx)?;
    let entry = (ctx.db.read()?.installations.get(&inst.alias).cloned())
        .ok_or_else(|| anyhow!("No installation named \"{}\"", inst.alias))?;
    let on = release_of(&entry)?;
    let file = dir.join(STACK_FILE);
    if file.exists() && !args.force {
        bail!("'{}' exists.\nPass -f to overwrite it.", file.display());
    }
    let name = stack_name(args.name.as_deref(), &inst.alias)?;
    // Anonymous: what a shared stack rests on must be reviewed.
    let client = Client::new(&ctx.db.read()?);
    let owner = super::package::owner(&client, args.owner.as_deref())?;
    let meta = inst.meta()?;
    let cactus = inst.cactus_root()?;
    let pristine = std::fs::read_to_string(inst.source_thornlist_to_read())
        .map_err(|e| anyhow!("This installation has no pristine thornlist: {e}"))?;
    let base = thornlist::parse(&pristine)?;
    let live = thornlist::parse(&super::add::compose(&pristine, &meta.added)?)?;

    // Refused as `sim export` refuses: nobody else could fetch uncommitted or unpushed work.
    let repos_dir = cactus.join("repos");
    let states = export::installed_states(&repos_dir, &live)?;
    let pins = export::repo_pins(&live, |repo| states.get(repo).cloned(), |_| None);
    let fetched = crate::fetch::FetchState::read(&inst.root)?;
    let published = export::check_repos(&repos_dir, &pins, fetched.as_ref())?;
    let fixes = export::Fixes {
        when: "in the installation",
        commit: "commit them",
        unhashed: String::new(),
        unpinned: String::new(),
        unlisted: None,
        unknown_commits: false,
    };
    let problems = export::state_problems(&pins, &published, &fixes);
    if !problems.is_empty() {
        bail!("These repos cannot go into a stack:\n  {}", problems.join("\n  "));
    }
    let commit_of = |repo: &str| states.get(repo).map(|s| crate::fetch::committed(s).to_owned());

    // The release, and what the installed packages claim.
    let label = on.to_string();
    let names = (meta.added.iter().map(|a| a.package.as_str()))
        .chain(base.components().iter().map(|c| c.repo.as_str()))
        .chain([label.as_str()]);
    let mut reg = Registry::new(&client, names);
    let asked = (|| -> Res<_> {
        let (o, n) = (on.owner.to_string(), on.name.to_string());
        let version = on.at.clone().unwrap_or_default();
        let release = reg.one(&label, "reading the release", move |c| c.stack_release(&o, &n, &version))?;
        let set = resolution::present(&mut reg, &inst.root, &base, &meta.added, None, &PresentOptions::default())?;
        Ok((release.stack, set))
    })();
    reg.finish();
    let (release, set) = asked?;

    let mut out = format!(
        "# cactarium-stack.toml for installation {}, written by `cactup stack init`.\n\
         # Review it before publishing.\nformat = 1\n\n[stack]\nname = {}\nowners = [{}]\nextends = [{}]\n",
        inst.alias,
        q(&name),
        q(&owner),
        q(&on.to_string())
    );
    let mut todos: Vec<String> = Vec::new();

    // Base repos moved since the release.
    let replaced: BTreeSet<&str> = meta.added.iter().flat_map(|a| a.replaces.iter().map(String::as_str)).collect();
    let mut pin_lines: Vec<String> = Vec::new();
    for p in &release.repos {
        if replaced.contains(p.name.as_str()) {
            continue;
        }
        let (Some(now), Some(was)) = (commit_of(&p.name), &p.commit) else { continue };
        if &now != was {
            pin_lines.push(pin_text(&p.name, Some(&p.url), p.git_ref.as_deref(), &now));
        }
    }

    // Added packages: an override where one replaces a base repo, else a pinned section.
    let mut overrides: Vec<String> = Vec::new();
    let mut sectioned: Vec<AddedPackage> = Vec::new();
    for a in &meta.added {
        let unreleased = super::add::is_tip(&a.version) || super::add::is_commit(&a.version);
        let fetched = commit_of(&a.repo).unwrap_or_else(|| a.commit.clone());
        let Some((first, rest)) = a.replaces.split_first() else {
            sectioned.push(match unreleased {
                true => AddedPackage { version: fetched.clone(), commit: fetched, ..a.clone() },
                false => a.clone(),
            });
            continue;
        };
        if unreleased {
            let mut o = format!("\n[[stack.override]]\nrepo = {}\nurl = {}\n", q(first), q(&a.url));
            if let Some(branch) = &a.tag {
                o.push_str(&format!("ref = {}\n", q(branch)));
            }
            overrides.push(o);
            pin_lines.push(pin_text(first, Some(&a.url), a.tag.as_deref(), &fetched));
        } else {
            let v: TrackedVersion =
                a.version.parse().map_err(|e| anyhow!("installation.toml adds {} at {}: {e}", a.package, a.version))?;
            let package = format!("{}@{}", a.package, TrackedReq::exactly(&v));
            overrides.push(format!("\n[[stack.override]]\nrepo = {}\npackage = {}\n", q(first), q(&package)));
        }
        if !rest.is_empty() {
            todos.push(format!(
                "# TODO: {} also replaces the base repos {}. An override redirects one repo.\n",
                a.package,
                rest.join(", ")
            ));
        }
    }
    // Composed onto the release's own text, so a package that adds thorns to a base clone keeps
    // exactly the source the release gives that clone.
    let sections = match sectioned.is_empty() {
        true => None,
        false => {
            let composed = super::add::compose(&release.thornlist.text, &sectioned)?;
            let block = composed.split_once(super::add::BLOCK_HEADER).map(|(_, b)| b.trim_start_matches('\n')).unwrap_or("");
            let root = thornlist::parse(&release.thornlist.text)?.root().to_owned();
            Some(format!(
                "# Packages added to installation {} with `cactup add`, written by `cactup stack init`.\n\
                 !CRL_VERSION = 1.0\n\n!DEFINE ROOT = {root}\n\n{block}",
                inst.alias
            ))
        }
    };
    if sections.is_some() {
        out.push_str(&format!("thornlist = {}\n", q(THORNLIST_FILE)));
    }
    for p in &pin_lines {
        out.push_str(p);
    }
    for o in &overrides {
        out.push_str(o);
    }
    for t in &todos {
        out.push('\n');
        out.push_str(t);
    }

    // What the installed packages declare they depend on: information, never a pin.
    let installed: BTreeSet<String> =
        set.present.iter().filter(|p| !p.release.id.is_repo()).map(|p| p.release.id.to_string()).collect();
    let mut declared: Vec<(String, Vec<String>, String)> = Vec::new();
    for p in &set.present {
        let id = p.release.id.to_string();
        let by = match &p.release.at {
            At::Version(v) => format!("{id}@{v}"),
            _ => format!("{id} at commit {}", p.release.commit.get(..12).unwrap_or(&p.release.commit)),
        };
        for c in p.release.claims.iter().filter(|c| c.kind == "dependency" && c.into.is_none()) {
            let options: Vec<Target> = match c.alternatives.is_empty() {
                true => vec![(c.target.as_str(), c.range.as_deref(), c.sha.as_deref(), c.needs.as_slice())],
                false => (c.alternatives.iter())
                    .map(|a| (a.target.as_str(), a.range.as_deref(), a.sha.as_deref(), a.needs.as_slice()))
                    .collect(),
            };
            for (target, range, sha, needs) in options {
                if target == id || !installed.contains(target) {
                    continue;
                }
                let target = match (range, sha) {
                    (Some(r), _) => format!("{target}@{r}"),
                    (None, Some(s)) => format!("{target}#{s}"),
                    (None, None) => target.to_owned(),
                };
                declared.push((target, needs.to_vec(), by.clone()));
            }
        }
    }
    declared.sort();
    declared.dedup();
    if !declared.is_empty() {
        out.push_str("\n# What the installed packages declare they depend on: information, not pins.\n");
    }
    for (target, needs, by) in &declared {
        out.push_str(&format!("\n# Declared by {by}.\n[[dependency]]\ntarget = {}\n", q(target)));
        if !needs.is_empty() {
            out.push_str(&format!("needs = {}\n", qa(needs.iter().map(String::as_str))));
        }
    }

    // Refused unless both read back as what they mean to be.
    Manifest::from_repo_file(STACK_FILE, &out)
        .map_err(|e| anyhow!("The cactarium-stack.toml written does not parse ({e}); nothing was written"))?;
    let th = dir.join(THORNLIST_FILE);
    if let Some(text) = &sections {
        thornlist::parse(text)
            .map_err(|e| anyhow!("The {THORNLIST_FILE} written does not parse ({e:#}); nothing was written"))?;
        if th.exists() && !args.force {
            bail!("'{}' exists.\nPass -f to overwrite it.", th.display());
        }
        std::fs::write(&th, text).with_context(|| format!("Failed to write '{}'", th.display()))?;
    }
    std::fs::write(&file, &out).with_context(|| format!("Failed to write '{}'", file.display()))?;
    match &sections {
        Some(_) => println!("Wrote '{}' and '{}'.", file.display(), th.display()),
        None => println!("Wrote '{}'.", file.display()),
    }
    let todos = out.lines().filter(|l| l.starts_with("# TODO:")).count();
    match todos {
        0 => {}
        1 => println!("{}", "Resolve the TODO in it.".yellow()),
        n => println!("{}", format!("Resolve the {n} TODOs in it.").yellow()),
    }
    let at = match &args.dir {
        Some(d) => format!(" {}", d.display()),
        None => String::new(),
    };
    println!("Publish a release with `cactup stack init{at} --publish --version X.Y.Z`.");
    Ok(())
}

/// A package a dependency may be met by: its id, range, commit and needs.
type Target<'a> = (&'a str, Option<&'a str>, Option<&'a str>, &'a [String]);

/// One `[[stack.pin]]`.
fn pin_text(name: &str, url: Option<&str>, git_ref: Option<&str>, commit: &str) -> String {
    let mut s = format!("\n[[stack.pin]]\nname = {}\n", q(name));
    if let Some(u) = url.filter(|u| !u.is_empty()) {
        s.push_str(&format!("url = {}\n", q(u)));
    }
    if let Some(r) = git_ref {
        s.push_str(&format!("ref = {}\n", q(r)));
    }
    s.push_str(&format!("commit = {}\n", q(commit)));
    s
}

/// The stack release the installation is on (where a refetch took it, else what was
/// installed): refused when it is on none, or on a branch tip, which no stack can extend.
fn release_of(entry: &crate::database::CactusInstallation) -> Res<Slug> {
    let alias = &entry.alias;
    let slug = match (&entry.current_release, &entry.current_thornlist) {
        (Some(current), _) if current.contains('/') => Some(current.clone()),
        (Some(_), _) | (None, Some(_)) => None,
        (None, None) => entry.stack.clone(),
    };
    let Some(slug) = slug else {
        bail!(
            "Installation {alias} is not on a Cactarium stack release.\n\
             Move it onto one with `cactup inst refetch --release owner/stack@version`."
        );
    };
    let parsed: Slug = slug.parse().map_err(|e| anyhow!("Installation {alias} records stack {slug}: {e}"))?;
    if parsed.thorn.is_some() || parsed.exact().is_err() {
        bail!(
            "Installation {alias} is on {slug}, which is no release.\n\
             Move it onto a release with `cactup inst refetch --release {}@<version>`.",
            parsed.entry()
        );
    }
    Ok(parsed)
}

fn stack_name(given: Option<&str>, alias: &str) -> Res<String> {
    let name = given.map_or_else(|| alias.to_ascii_lowercase(), str::to_owned);
    if let Err(e) = name.parse::<Name>() {
        match given {
            Some(_) => bail!("\"{name}\" is not a stack name: {e}"),
            None => bail!("\"{name}\" is not a stack name: {e}\nName the stack with --name."),
        }
    }
    Ok(name)
}

/// `cactup stack init [DIR] --publish --version V`: the definition in `dir` (and its own
/// thornlist) published as release `version` of `owner/<its name>`. Writes nothing.
fn publish(ctx: &Ctx, dir: &Path, owner: Option<&str>, version: &str) -> Res<()> {
    let file = dir.join(STACK_FILE);
    let text = std::fs::read_to_string(&file)
        .map_err(|_| anyhow!("No {STACK_FILE} in '{}'.\nWrite one with `cactup stack init`.", dir.display()))?;
    let manifest = Manifest::from_repo_file(STACK_FILE, &text).map_err(|e| anyhow!("'{}': {e}", file.display()))?;
    let stack = manifest.stack.expect("a stack file holds a [stack]");
    let owner = match owner {
        Some(o) => o.to_owned(),
        None => stack.owners.first().map(ToString::to_string).ok_or_else(|| anyhow!("[stack] names no owners."))?,
    };
    let thornlist = stack.thornlist.as_ref().map(|t| dir.join(t));
    super::import::publish_stack(ctx, &file, &format!("{owner}/{}", stack.name), version, thornlist.as_deref())
}
