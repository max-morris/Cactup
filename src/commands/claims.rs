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
use crate::installation::{AddedPackage, ClaimOverride, Installation, InstallationMeta};
use cactup_ecosystem_common::version::TrackedReq;
use crate::{thornlist, Res};

pub fn dispatch(ctx: &Ctx, command: ClaimsCommand) -> Res<()> {
    match command {
        ClaimsCommand::List => list(ctx),
        ClaimsCommand::Check => check(ctx),
        ClaimsCommand::Override { remove: Some(key), .. } => remove(ctx, &key),
        ClaimsCommand::Override { action: Some(action), .. } => {
            let (o, note) = match action {
                OverrideAction::Drop { claim, note } => {
                    if note.as_deref().is_some_and(|n| n.contains(['\n', '\r'])) {
                        bail!("A note is one line.");
                    }
                    (Override::drop_claim(&claim)?, note)
                }
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
    // The key must read back as this override, or the installation could not be used again.
    if !o.reads_back() {
        bail!("'{key}' does not read back as this override.");
    }
    validate(ctx, &inst, o)?;
    let locked = inst.locked()?;
    let mut meta = locked.meta()?;
    if let Some(old) = meta.claim_overrides.get_mut(&key) {
        match note {
            Some(note) if old.note.as_ref() != Some(&note) => {
                old.note = Some(note);
                locked.set_meta(&meta)?;
                println!("Changed the note of the override '{key}'.");
            }
            _ => println!("The override '{key}' is already set."),
        }
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

/// The lines `claims list` prints for the overrides (each with its note and what is wrong with it),
/// and the first key that does not read. `repos`: the repos of the base list, when it is known.
pub(crate) fn override_lines(meta: &InstallationMeta, repos: Option<&BTreeSet<String>>) -> (Vec<String>, Option<String>) {
    if meta.claim_overrides.is_empty() {
        return (vec!["No claim overrides.".to_owned()], None);
    }
    let mut lines = vec!["Claim overrides:".to_owned()];
    let mut unreadable = None;
    for (key, record) in &meta.claim_overrides {
        let mut marks: Vec<String> = record.note.iter().cloned().collect();
        match deps::read_record(key, record) {
            Err(_) => {
                marks.push("does not read".to_owned());
                unreadable.get_or_insert(key.clone());
            }
            Ok(Override::Satisfies { by, .. }) if by.is_repo() => {
                let name = by.as_str().trim_start_matches("local:");
                if repos.is_some_and(|r| !r.contains(name)) {
                    marks.push(format!("the installation has no repo {name}"));
                }
            }
            Ok(_) => {}
        }
        lines.push(match marks.is_empty() {
            true => format!("  {key}"),
            false => format!("  {key}  ({})", marks.join("; ")),
        });
    }
    (lines, unreadable)
}

/// The overrides, then the any-of choices recorded for the packages added.
fn list(ctx: &Ctx) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let meta = inst.meta()?;
    // The repos of the base list, to tell a `local:` override that names none.
    let repos: Option<BTreeSet<String>> = std::fs::read_to_string(inst.source_thornlist_to_read())
        .ok()
        .and_then(|text| thornlist::parse(&text).ok())
        .map(|base| base.components().iter().map(|c| c.repo.clone()).collect());
    let (lines, unreadable) = override_lines(&meta, repos.as_ref());
    lines.iter().for_each(|l| println!("{l}"));
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
    match unreadable {
        Some(key) => Err(deps::unreadable(&key)),
        None => Ok(()),
    }
}

/// What the packages and claims an override names must be: served by Cactarium (or, for
/// `local:<repo>`, a repo of the installation no package stands for), and the claim there in
/// some version.
fn validate(ctx: &Ctx, inst: &Installation, o: &Override) -> Res<()> {
    let meta = inst.meta()?;
    let client = Client::for_command(ctx)?;
    let names: Vec<&str> = meta.added.iter().map(|a| a.package.as_str()).collect();
    // Version lists only: no read at a commit, so no reason to wait as long as resolving does.
    let mut reg = Registry::new(&client, names).bounded(std::time::Duration::from_secs(60));
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
            // The version installed, when the package is: the others may differ.
            let installed: Option<&AddedPackage> = meta.added.iter().find(|a| a.package == package.as_str());
            let declares = |r: &Release| r.claims.iter().any(|c| pattern.matches(package, c));
            let already = match installed {
                Some(a) => releases.iter().find(|r| r.commit == a.commit).is_some_and(declares),
                None => !releases.is_empty() && releases.iter().all(declares),
            };
            if already {
                bail!(
                    "{package} already depends on {target}.\nAdd thorns to it with \
                     `cactup claims override needs {package}:dependency:{target} THORN`."
                );
            }
            let target_releases = served(reg, &target)?;
            if let Some(range) = &claim.range {
                meets_range(&target_releases, &target, range)?;
            }
            has_thorns(&target_releases, &target, &claim.needs)?;
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
        bail!("{id} is a repo of the installation. It has no claims.");
    }
    reg.load(std::slice::from_ref(id))?;
    match reg.releases(id) {
        Some(releases) => Ok(releases.to_vec()),
        None => Err(anyhow!("{id} is unknown to Cactarium.")),
    }
}

/// A version of `package` meets `range`, on tracks it has.
fn meets_range(releases: &[Release], package: &PkgId, range: &str) -> Res<()> {
    let q: TrackedReq = range.parse()?;
    for alt in &q.alts {
        let on_track = |r: &Release| r.at.version().is_some_and(|v| v.track == alt.track);
        if !releases.iter().any(on_track) {
            let track = alt.track.as_ref().map_or("the default track".to_owned(), |t| format!("the track {t}"));
            bail!("{package} has no version on {track}.");
        }
    }
    if !releases.iter().any(|r| r.at.version().is_some_and(|v| q.matches(v))) {
        bail!("No version of {package} meets {range}.");
    }
    Ok(())
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
    /// Added packages whose release Cactarium cannot give, one line each with why: what they
    /// depend on is unknown.
    pub unread: Vec<String>,
    /// The base repos have no fetch record, so none is mapped to a package.
    pub unmapped: bool,
}

impl Checked {
    /// What was left unchecked, one line each; empty when everything was checked.
    pub fn not_checked(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.unchecked > 0 {
            let what = if self.unchecked == 1 { "base package" } else { "base packages" };
            out.push(format!("Not checked: the claims of {} {what} at unreleased commits.", self.unchecked));
        }
        if !self.unread.is_empty() {
            let lines: String = self.unread.iter().map(|l| format!("\n  {l}")).collect();
            out.push(format!("Not checked: the dependencies of these packages are unknown:{lines}"));
        }
        if self.unmapped {
            out.push("Not checked: no base repo is mapped to a package (no fetch record). Run `cactup inst refetch`.".to_owned());
        }
        out
    }
}

/// The problems among the packages of `inst`, as `claims check` reports them (steps 6 and 7 of the
/// resolver over everything installed, with the installation's overrides applied). Changes
/// nothing; fails when Cactarium cannot be asked.
pub(crate) fn problems(ctx: &Ctx, inst: &Installation, meta: &InstallationMeta) -> Res<Checked> {
    problems_with(&Client::for_command(ctx)?, inst, meta, &PresentOptions::default())
}

/// [`problems`], asking `client`, reading the installation with `options`.
fn problems_with(client: &Client, inst: &Installation, meta: &InstallationMeta, options: &PresentOptions) -> Res<Checked> {
    let pristine = std::fs::read_to_string(inst.source_thornlist_to_read())
        .map_err(|e| anyhow!("This installation has no pristine thornlist: {e}"))?;
    let base = thornlist::parse(&pristine)?;
    let live = thornlist::parse(&super::add::compose(&pristine, &meta.added)?)?;
    let local = resolution::local_facts(&live, &inst.cactus_root()?)?;
    let rules = Rules::of(&meta.claim_overrides, Vec::new())?;
    let names: Vec<&str> = (meta.added.iter().map(|a| a.package.as_str()))
        .chain(base.components().iter().map(|c| c.repo.as_str()))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut reg = Registry::new(client, names);
    let unmapped = !base.components().is_empty()
        && crate::fetch::FetchState::read(&inst.root)?.is_none_or(|f| f.repos.is_empty());
    let answer = (|| -> Res<Checked> {
        let set = resolution::present(&mut reg, &inst.root, &base, &meta.added, Some(&local), options)?;
        let lines = deps::check(&set.present, &rules, &mut reg)?;
        let unread = set.unread.iter().map(resolution::Unread::line).collect();
        Ok(Checked { lines, unchecked: set.unchecked.len(), unread, unmapped })
    })();
    let finished = reg.finish();
    let mut found = answer?;
    // An older Cactarium cannot list providers: the line says so rather than that none exists.
    if finished.no_providers {
        for line in &mut found.lines {
            *line = line.replace(deps::NO_PROVIDER, "Cactarium cannot list providers.");
        }
    }
    Ok(found)
}

/// How long each request of the warnings after a stack install may take.
const WARN_REQUEST: std::time::Duration = std::time::Duration::from_secs(15);
/// How long those warnings may take in all.
const WARN_DEADLINE: std::time::Duration = std::time::Duration::from_secs(20);

/// Stops the requests of the warnings ([`warn_problems`]) at their deadline
/// or at Ctrl-C.
static WARN_STOP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
/// Set when a Ctrl-C arrived while the warnings ran, and they took it.
static WARN_TOOK_INTERRUPT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Whether the Ctrl-C now set arrived during [`warn_problems`], which ended
/// on it: the command it followed had finished, so it is no failure.
pub(crate) fn interrupt_taken_by_warnings() -> bool {
    WARN_TOOK_INTERRUPT.load(std::sync::atomic::Ordering::SeqCst)
}

/// The check `claims check` makes, run after a stack install or a refetch to a stack, once the
/// command's own output is complete: each problem printed as a warning, ending in its way past,
/// and returned. It never fails the command (a release is frozen and installed as it is).
///
/// Bounded: each request may take [`WARN_REQUEST`], and the whole check [`WARN_DEADLINE`]; base
/// packages at commits no version froze are not read (they go on the "Not checked" line). A
/// Ctrl-C while it runs ends the check only. It is the one exception to "a command that comes
/// back Ok after Ctrl-C fails": the command had finished. Narrowly: the interrupt flag stays set
/// (nothing after this may run as if there were none), and only a Ctrl-C that arrived after the
/// check began is taken ([`interrupt_taken_by_warnings`], which `main` consults).
// Plan §4.3, misfit 7.
pub(crate) fn warn_problems(ctx: &Ctx, inst: &Installation) -> Vec<String> {
    use std::sync::atomic::Ordering;
    if gix::interrupt::is_triggered() {
        return Vec::new();
    }
    let said = match Client::for_command(ctx).and_then(|c| Ok((c, inst.meta()?))) {
        Ok((client, meta)) => {
            let client = client.stopped_by(&WARN_STOP).with_timeout(WARN_REQUEST);
            warn_lines(bounded_check(client, Installation::new(inst.alias.clone(), inst.root.clone()), meta))
        }
        Err(e) => vec![did_not_check(&e)],
    };
    for line in &said {
        println!("{}", line.yellow());
    }
    if gix::interrupt::is_triggered() {
        WARN_TOOK_INTERRUPT.store(true, Ordering::SeqCst);
    }
    said
}

/// How the check after a stack install ended.
enum Bounded {
    Done(Res<Checked>),
    TimedOut,
    Interrupted,
}

/// The check, on a thread of its own, waited for until [`WARN_DEADLINE`] or Ctrl-C, whichever
/// comes first; either stops its requests ([`WARN_STOP`]) and leaves the thread to unwind.
fn bounded_check(client: Client, inst: Installation, meta: InstallationMeta) -> Bounded {
    use std::sync::atomic::Ordering;
    WARN_STOP.store(false, Ordering::SeqCst);
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let options = PresentOptions { no_unreleased_reads: true, ..PresentOptions::default() };
        let _ = send.send(problems_with(&client, &inst, &meta, &options));
    });
    let deadline = std::time::Instant::now() + warn_deadline();
    let ended = loop {
        if gix::interrupt::is_triggered() {
            break Bounded::Interrupted;
        }
        if std::time::Instant::now() >= deadline {
            break Bounded::TimedOut;
        }
        match receive.recv_timeout(std::time::Duration::from_millis(50)) {
            // A Ctrl-C between this loop's look at the flag and the answer: the check failed
            // because it was stopped, and says so as one the loop saw first would.
            Ok(Err(_)) if gix::interrupt::is_triggered() => break Bounded::Interrupted,
            Ok(found) => break Bounded::Done(found),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                break Bounded::Done(Err(anyhow!("The check died")));
            }
        }
    };
    WARN_STOP.store(true, Ordering::SeqCst);
    // Unwound by the stop: its renderer is shut down before anything else is printed.
    let _ = receive.recv_timeout(std::time::Duration::from_secs(2));
    ended
}

/// [`WARN_DEADLINE`], shortened by `CACTUP_TEST_WARN_DEADLINE_MS` in tests.
fn warn_deadline() -> std::time::Duration {
    #[cfg(test)]
    if let Some(ms) = std::env::var("CACTUP_TEST_WARN_DEADLINE_MS").ok().and_then(|v| v.parse().ok()) {
        return std::time::Duration::from_millis(ms);
    }
    WARN_DEADLINE
}

/// The lines the warnings say for how the check ended.
fn warn_lines(ended: Bounded) -> Vec<String> {
    let mut said = Vec::new();
    match ended {
        Bounded::Done(Ok(found)) => {
            if !found.lines.is_empty() {
                said.push("Claim problems:".to_owned());
                said.extend(found.lines.iter().map(|l| format!("  {l}")));
            }
            said.extend(found.not_checked());
        }
        Bounded::Done(Err(e)) => said.push(did_not_check(&e)),
        Bounded::TimedOut => said.push("Did not check the claims: Cactarium did not answer in time.".to_owned()),
        Bounded::Interrupted => said.push("Did not check the claims: interrupted.".to_owned()),
    }
    if !matches!(said.first(), Some(l) if l.starts_with("Did not check")) {
        return said;
    }
    said.push("Check them with `cactup claims check`.".to_owned());
    said
}

/// One line for a check that could not run: the first line of why.
fn did_not_check(e: &anyhow::Error) -> String {
    let why = format!("{e:#}");
    format!("Did not check the claims: {}", why.lines().next().unwrap_or_default().trim_end_matches('.'))
        + "."
}

/// `claims check`: prints the problems, one line each, and fails when there are any.
fn check(ctx: &Ctx) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let meta = inst.meta()?;
    let found = problems(ctx, &inst, &meta)?;
    for line in &found.lines {
        println!("{line}");
    }
    let unchecked = found.not_checked();
    for line in &unchecked {
        println!("{}", line.yellow());
    }
    match found.lines.len() {
        0 if unchecked.is_empty() => {
            println!("No problems.");
            Ok(())
        }
        0 => {
            println!("No problems in what was checked.");
            Ok(())
        }
        1 => bail!("1 problem."),
        n => bail!("{n} problems."),
    }
}
