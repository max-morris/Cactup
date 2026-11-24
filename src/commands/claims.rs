//! `cactup claims` (D4): the claim overrides of an installation, the any-of choices recorded in
//! it, and a check of the installed packages against each other's claims.
// Plan §4.4, §2.4, the P4 row of §7.

use std::collections::BTreeSet;

use anyhow::{anyhow, bail};
use colored::Colorize;

use super::resolution::{self, PresentOptions, Registry};
use super::Ctx;
use crate::args::{ClaimsCommand, OverrideAction};
use crate::cactarium::Client;
use crate::deps::{self, ClaimKind, ClaimPattern, Facts, Override, PkgId, Release, Rules};
use crate::installation::{ClaimOverride, Installation, InstallationMeta};
use crate::{thornlist, Res};

pub fn dispatch(ctx: &Ctx, command: ClaimsCommand) -> Res<()> {
    match command {
        ClaimsCommand::List => list(ctx),
        ClaimsCommand::Check => check(ctx),
        ClaimsCommand::Override { remove: Some(key), .. } => remove(ctx, &key),
        ClaimsCommand::Override { action: Some(action), .. } => {
            let (o, note) = match action {
                OverrideAction::Drop { claim, note } => (Override::drop_claim(&claim)?, note),
                OverrideAction::Add { package, dependency, needs, needed_by } => {
                    (Override::add_dependency(&package, &dependency, &needs, &needed_by)?, None)
                }
                OverrideAction::Needs { claim, thorns } => (Override::add_needs(&claim, &thorns)?, None),
                OverrideAction::Satisfies { by, target } => (Override::satisfies(&by, &target)?, None),
            };
            set(ctx, &o, note)
        }
        ClaimsCommand::Override { .. } => bail!("Name an override to make, or one to remove with --remove."),
    }
}

/// Records `o`, once the packages and claims it names are found to exist.
fn set(ctx: &Ctx, o: &Override, note: Option<String>) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let key = o.key();
    validate(ctx, &inst, o)?;
    let locked = inst.locked()?;
    let mut meta = locked.meta()?;
    // An override already set keeps its note.
    if meta.claim_overrides.contains_key(&key) {
        println!("The override '{key}' is already set.");
        return Ok(());
    }
    meta.claim_overrides.insert(key.clone(), ClaimOverride { kind: o.kind_name().to_owned(), note });
    locked.set_meta(&meta)?;
    println!("Added the override '{key}'.");
    println!("Remove it with `cactup claims override --remove '{key}'`.");
    Ok(())
}

/// Forgets the override named by `key`, as `list` shows it or as it is written in canonical form.
fn remove(ctx: &Ctx, key: &str) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let locked = inst.locked()?;
    let mut meta = locked.meta()?;
    let canonical = Override::from_key(key).map(|o| o.key()).ok();
    let found = (meta.claim_overrides.contains_key(key).then(|| key.to_owned()))
        .or_else(|| canonical.filter(|k| meta.claim_overrides.contains_key(k)));
    let Some(found) = found else {
        bail!("No override '{key}'.\nList them with `cactup claims list`.");
    };
    meta.claim_overrides.remove(&found);
    locked.set_meta(&meta)?;
    println!("Removed the override '{found}'.");
    Ok(())
}

/// The overrides, then the any-of choices recorded for the packages added.
fn list(ctx: &Ctx) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let meta = inst.meta()?;
    // Read as every resolution reads them: an unreadable record is an error here too.
    deps::overrides_of(&meta.claim_overrides)?;
    if meta.claim_overrides.is_empty() {
        println!("No claim overrides.");
    } else {
        println!("Claim overrides:");
        for (key, record) in &meta.claim_overrides {
            match &record.note {
                Some(note) => println!("  {key}  ({note})"),
                None => println!("  {key}"),
            }
        }
    }
    let mut choices = Vec::new();
    for (package, entry) in meta.added_for_present() {
        // Believed only at the commit it was written for.
        let Some(entry) = entry.filter(|e| e.for_commit.as_deref() == Some(package.commit.as_str())) else { continue };
        for c in &entry.choices {
            choices.push(format!("  {}: any of {}, with {}", c.dependent, c.alternatives.join(", "), package.package));
        }
    }
    if choices.is_empty() {
        println!("No any-of choices.");
    } else {
        choices.sort();
        println!("Any-of choices:");
        choices.iter().for_each(|c| println!("{c}"));
    }
    Ok(())
}

/// What the packages and claims an override names must be: served by Cactarium (or, for
/// `local:<repo>`, a repo of the installation no package stands for), and the claim there in
/// some version.
fn validate(ctx: &Ctx, inst: &Installation, o: &Override) -> Res<()> {
    let meta = inst.meta()?;
    let client = Client::new(&ctx.db.read()?);
    let names: Vec<&str> = meta.added.iter().map(|a| a.package.as_str()).collect();
    let mut reg = Registry::new(&client, names);
    let answer = validate_with(&mut reg, inst, &meta, o);
    reg.finish();
    answer
}

fn validate_with(reg: &mut Registry, inst: &Installation, meta: &InstallationMeta, o: &Override) -> Res<()> {
    match o {
        Override::Drop(pattern) => {
            let releases = served(reg, &pattern.package)?;
            if !releases.iter().flat_map(|r| &r.claims).any(|c| pattern.matches(&pattern.package, c)) {
                let (kind, target) = pattern.kind.as_ref().expect("a drop names its claim");
                bail!("{} makes no {} claim on {target} in any version.", pattern.package, kind.as_str());
            }
        }
        Override::Needs { package, target, thorns } => {
            let releases = served(reg, package)?;
            let pattern = ClaimPattern::new(package.clone(), ClaimKind::Dependency, target.clone());
            if !releases.iter().flat_map(|r| &r.claims).any(|c| pattern.matches(package, c)) {
                bail!("{package} has no dependency on {target} in any version.");
            }
            has_thorns(&served(reg, target)?, target, thorns)?;
        }
        Override::Add { package, claim } => {
            let releases = served(reg, package)?;
            let target = PkgId::parse(&claim.target)?;
            let pattern = ClaimPattern::new(package.clone(), ClaimKind::Dependency, target.clone());
            if releases.iter().flat_map(|r| &r.claims).any(|c| pattern.matches(package, c)) {
                bail!(
                    "{package} already depends on {target}.\nAdd thorns to it with \
                     `cactup claims override needs {package}:dependency:{target} THORN`."
                );
            }
            has_thorns(&served(reg, &target)?, &target, &claim.needs)?;
            has_thorns(&releases, package, &claim.needed_by)?;
        }
        Override::Satisfies { by, target } => {
            served(reg, target)?;
            match by.is_repo() {
                false => {
                    served(reg, by)?;
                }
                true => unmapped_repo(reg, inst, meta, by)?,
            }
        }
    }
    Ok(())
}

/// The versions Cactarium serves of `id`.
fn served(reg: &mut Registry, id: &PkgId) -> Res<Vec<Release>> {
    if id.is_repo() {
        bail!("{id} is a repo of the installation, with no claims.");
    }
    reg.load(std::slice::from_ref(id))?;
    match reg.releases(id) {
        Some(releases) => Ok(releases.to_vec()),
        None => Err(anyhow!("{id} is unknown to Cactarium.")),
    }
}

/// Every thorn of `names` is in some version of `package`.
fn has_thorns(releases: &[Release], package: &PkgId, names: &[String]) -> Res<()> {
    for name in names {
        if !releases.iter().any(|r| r.thorn(name).is_some()) {
            bail!("{package} has no thorn {name} in any version.");
        }
    }
    Ok(())
}

/// `local:<repo>` stands for a base repo that no registered package is known to be.
fn unmapped_repo(reg: &mut Registry, inst: &Installation, meta: &InstallationMeta, id: &PkgId) -> Res<()> {
    let pristine = std::fs::read_to_string(inst.source_thornlist_to_read())
        .map_err(|e| anyhow!("This installation has no pristine thornlist: {e}"))?;
    let base = thornlist::parse(&pristine)?;
    let name = id.as_str().trim_start_matches("local:");
    if !base.components().iter().any(|c| c.repo == name) {
        bail!("The installation has no repo {name}.");
    }
    let options = PresentOptions { no_base_reads: true, ..PresentOptions::default() };
    let set = resolution::present(reg, &inst.root, &base, &meta.added, None, &options)?;
    if !set.present.iter().any(|p| &p.release.id == id) {
        let package = (set.present.iter())
            .find(|p| matches!(&p.origin, deps::Origin::Base { repo } if repo == name))
            .map(|p| p.release.id.to_string());
        match package {
            Some(package) => bail!("The repo {name} is the package {package}.\nName that instead of {id}."),
            None => bail!("The repo {name} is not in the installation."),
        }
    }
    Ok(())
}

/// What [`problems`] found.
pub(crate) struct Checked {
    /// One line for each problem, ending in the way past.
    pub lines: Vec<String>,
    /// Base packages whose claims are unknown: at a commit no version froze that Cactarium could
    /// not read.
    pub unchecked: usize,
}

/// The problems among the packages of `inst`, as `claims check` reports them (steps 6 and 7 of the
/// resolver over everything installed, with the installation's overrides applied). Changes
/// nothing; fails when Cactarium cannot be asked.
pub(crate) fn problems(ctx: &Ctx, inst: &Installation, meta: &InstallationMeta) -> Res<Checked> {
    let pristine = std::fs::read_to_string(inst.source_thornlist_to_read())
        .map_err(|e| anyhow!("This installation has no pristine thornlist: {e}"))?;
    let base = thornlist::parse(&pristine)?;
    let live = thornlist::parse(&super::add::compose(&pristine, &meta.added)?)?;
    let local = resolution::local_facts(&live, &inst.cactus_root()?)?;
    let rules = Rules::of(&meta.claim_overrides, Vec::new())?;
    let client = Client::new(&ctx.db.read()?);
    let names: Vec<&str> = (meta.added.iter().map(|a| a.package.as_str()))
        .chain(base.components().iter().map(|c| c.repo.as_str()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut reg = Registry::new(&client, names);
    let answer = (|| -> Res<Checked> {
        let set = resolution::present(&mut reg, &inst.root, &base, &meta.added, Some(&local), &PresentOptions::default())?;
        let lines = deps::check(&set.present, &rules, &mut reg)?;
        Ok(Checked { lines, unchecked: set.unchecked.len() })
    })();
    reg.finish();
    answer
}

/// `claims check`: prints the problems, one line each, and fails when there are any.
fn check(ctx: &Ctx) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let meta = inst.meta()?;
    let found = problems(ctx, &inst, &meta)?;
    for line in &found.lines {
        println!("{line}");
    }
    if found.unchecked > 0 {
        let what = if found.unchecked == 1 { "base package" } else { "base packages" };
        let line = format!("Not checked: the claims of {} {what} at unreleased commits.", found.unchecked);
        println!("{}", line.yellow());
    }
    match found.lines.len() {
        0 => {
            println!("No problems.");
            Ok(())
        }
        1 => bail!("1 problem."),
        n => bail!("{n} problems."),
    }
}
