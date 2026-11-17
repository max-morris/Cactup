//! What `cactup add` and `cactup remove` ask the Cactarium, and what they
//! show before they change anything: the registry side of [`crate::deps`]
//! (its [`Facts`], over HTTP, in parallel, with progress), the installation
//! as the resolver sees it (which package each base repo is, by one lookup
//! of every pin), and the plan, printed and confirmed.
// Plan §2.2 (interrupts and progress), §2.3 (showing and confirming), §3 (CCL
// facts), §4.1, §4.2.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{anyhow, bail, Context};
use cactup_ecosystem_common::api::{PinInfo, PinQuery, ThornInfo};
use cactup_ecosystem_common::ccl::ThornFacts;
use cactup_ecosystem_common::slug::is_commit_id;
use colored::Colorize;

use crate::cactarium::Client;
use crate::deps::{
    self, AddedFor, At, ChoiceKey, Facts, InstalledLibrary, LineKind, Origin, PkgId, Present, Provider, Release,
    Snapshot,
};
use crate::installation::{AddedPackage, InstallationMeta};
use crate::progress::{Headline, Layout, Line};
use crate::thornlist::{Component, ComponentType, Thornlist};
use crate::Res;

/// The headline over every request to the Cactarium.
const ASK: &str = "ask the Cactarium";
/// The line a long search shows on.
const SEARCH: &str = "look for a plan";
/// How many search steps go by between two updates of the search line.
const TICK_EVERY: usize = 1024;
/// A whole-request limit for each request: blocking reqwest cannot be
/// interrupted mid-request, so this bounds how long Ctrl-C can wait.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// The thorns read from the installed tree: checkout → its `.ccl` facts
/// (`None`: unreadable). What [`crate::sim::trim::thorn_facts`] gives, keyed
/// by checkout.
pub(crate) type LocalFacts = BTreeMap<String, Option<ThornFacts>>;

/// The Cactarium, as [`deps::resolve`] reads it: every package's versions
/// loaded once, in parallel waves, each request on a line under a headline;
/// snapshots and provider searches one at a time, each on its line too. One
/// renderer for the whole conversation, shut down by [`Registry::finish`] (or
/// on drop), so nothing may print to the terminal while one is alive.
pub(crate) struct Registry {
    client: Client,
    loaded: BTreeMap<PkgId, Option<Vec<Release>>>,
    contacts: BTreeMap<PkgId, String>,
    progress: std::sync::Arc<prodash::tree::Root>,
    renderer: Option<prodash::render::line::JoinHandle>,
    layout: Layout,
    search: Option<Line>,
    last_tick: usize,
}

impl Registry {
    /// A conversation with the Cactarium `client` talks to; `names` sizes
    /// the progress lines (package ids and repo names known so far).
    pub(crate) fn new<'a>(client: &Client, names: impl IntoIterator<Item = &'a str>) -> Registry {
        let client = Client::at(client.base()).with_timeout(REQUEST_TIMEOUT);
        // Only bars: no line here ever calls `info`/`fail`.
        let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
        let layout = Layout::for_names([ASK, SEARCH].into_iter().chain(names));
        Registry {
            client,
            loaded: BTreeMap::new(),
            contacts: BTreeMap::new(),
            progress,
            renderer,
            layout,
            search: None,
            last_tick: 0,
        }
    }

    /// Stops the renderer: after this, the terminal is the caller's.
    pub(crate) fn finish(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        self.search = None;
        if let Some(renderer) = self.renderer.take() {
            renderer.shutdown_and_wait();
        }
    }

    fn headline(&self, total: usize, unit: &'static str) -> Headline {
        Headline::over(self.progress.add_child(ASK), ASK, total, unit, self.layout.clone())
    }

    /// One request on a line of its own under the headline, `name` naming
    /// it and `phase` saying what it asks.
    fn one<T>(&self, name: &str, phase: &str, ask: impl FnOnce(&Client) -> Res<T>) -> Res<T> {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let mut headline = self.headline(1, "requests");
        let line = Line::over(headline.add_child(name.to_owned()), name, self.layout.clone());
        line.phase(phase.to_owned());
        let answer = ask(&self.client);
        drop(line);
        headline.inc();
        answer
    }

    /// Asks `/resolve` about each named package, as a package, in parallel:
    /// a name that is a stack or a sim, or that the Cactarium does not know,
    /// fails here with the Cactarium's own words.
    pub(crate) fn check_named(&self, specs: &[(String, String)]) -> Res<()> {
        if specs.is_empty() {
            return Ok(());
        }
        let headline = Mutex::new(self.headline(specs.len(), "packages"));
        let answers = crate::par::parallel_map(specs, |(entry, req)| {
            let item = headline.lock().expect("progress poisoned").add_child(entry.clone());
            let line = Line::over(item, entry, self.layout.clone());
            line.phase("resolving".to_owned());
            let answer = self.client.resolve(req, "package");
            drop(line);
            headline.lock().expect("progress poisoned").inc();
            answer
        })?;
        for answer in answers {
            let resolved = answer?;
            if resolved.kind != "package" {
                bail!("{} is a {}, not a package; install a stack with `cactup install`", resolved.slug, resolved.kind);
            }
        }
        Ok(())
    }

    /// Which package each pin is, in one request (or as few as the size
    /// limit allows): the lookup is rate-limited per address.
    pub(crate) fn lookup(&self, pins: &[PinQuery]) -> Res<Vec<PinInfo>> {
        if pins.is_empty() {
            return Ok(Vec::new());
        }
        let phase = format!("looking up {} repos", pins.len());
        self.one("the installation's repos", &phase, |c| c.lookup(pins))
    }

    /// The package's contact, as its versions answer gave it.
    fn contact(&self, id: &PkgId) -> Option<&str> {
        self.contacts.get(id).map(String::as_str)
    }
}

impl Drop for Registry {
    fn drop(&mut self) {
        self.stop();
    }
}

/// `owner`, `name` of a registered package's id.
fn owner_name(id: &PkgId) -> Res<(&str, &str)> {
    id.as_str().split_once('/').ok_or_else(|| anyhow!("{id} is not a registered package"))
}

impl Facts for Registry {
    fn load(&mut self, ids: &[PkgId]) -> Res<()> {
        let want: Vec<PkgId> = (ids.iter())
            .filter(|id| !id.is_repo() && !self.loaded.contains_key(*id))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if want.is_empty() {
            return Ok(());
        }
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let headline = Mutex::new(self.headline(want.len(), "packages"));
        let (client, layout) = (&self.client, &self.layout);
        let answers = crate::par::parallel_map(&want, |id| {
            let item = headline.lock().expect("progress poisoned").add_child(id.to_string());
            let line = Line::over(item, id.as_str(), layout.clone());
            line.phase("reading its versions".to_owned());
            let answer = owner_name(id).and_then(|(o, n)| client.package_versions_if_any(o, n));
            drop(line);
            headline.lock().expect("progress poisoned").inc();
            answer
        })?;
        for (id, answer) in want.into_iter().zip(answers) {
            let Some(versions) = answer.with_context(|| format!("Failed to read the versions of {id}"))? else {
                self.loaded.insert(id, None);
                continue;
            };
            if let Some(contact) = &versions.contact {
                self.contacts.insert(id.clone(), contact.clone());
            }
            let releases = (versions.versions.iter())
                .map(|v| Release::from_served(&id, v, versions.contact.as_deref()))
                .collect::<Res<Vec<Release>>>()?;
            self.loaded.insert(id, Some(releases));
        }
        Ok(())
    }

    fn releases(&self, id: &PkgId) -> Option<&[Release]> {
        self.loaded.get(id).and_then(|r| r.as_deref())
    }

    fn snapshot(&mut self, id: &PkgId, at: &str) -> Res<Option<Snapshot>> {
        let (owner, name) = owner_name(id)?;
        let shown = if is_commit_id(at) || at.len() > 12 { at.get(..12).unwrap_or(at) } else { at };
        let label = format!("{id}@{shown}");
        let read = self.one(&label, "reading the package there", |c| c.package_at_if_any(owner, name, at))?;
        let Some((version, untrusted)) = read else { return Ok(None) };
        let release = Release::from_served(id, &version, self.contact(id))?;
        Ok(Some(Snapshot { release, untrusted }))
    }

    fn providers(&mut self, requirement: &deps::Requirement) -> Res<Vec<Provider>> {
        let label = requirement.to_string();
        let found = self.one(&label, "asking what provides it", |c| c.providers(requirement))?;
        (found.providers.into_iter())
            .map(|p| {
                Ok(Provider { package: PkgId::parse(&p.package)?, version: p.version, arrangement: p.arrangement, thorn: p.thorn })
            })
            .collect()
    }

    fn tick(&mut self, steps: usize) {
        if self.search.is_some() && steps < self.last_tick + TICK_EVERY {
            return;
        }
        self.last_tick = steps;
        if self.search.is_none() {
            self.search = Some(Line::over(self.progress.add_child(SEARCH), SEARCH, self.layout.clone()));
        }
        if let Some(line) = &self.search {
            line.phase(format!("{steps} steps"));
        }
    }
}

/// A base repo of the installation, as the resolver needs it.
struct BaseRepo<'l> {
    name: String,
    url: Option<String>,
    /// The commit it is at on disk (else the one the list pins), when
    /// that is a full commit id.
    commit: Option<String>,
    git: bool,
    /// Its arrangement thorns the live list still checks out.
    thorns: Vec<&'l Component>,
    flesh: bool,
}

/// The installation as [`deps::resolve`] and [`deps::plan_remove`] see it:
/// every added package at its release, and every base repo, mapped to its
/// package by the lookup (or a pseudo-package `repo:<name>` when it is no
/// registered package's), each with the thorns it has enabled. Present
/// thorns carry the facts read from the tree (`local`), where they were read:
/// what is installed, rather than what the Cactarium scanned.
pub(crate) fn present(
    reg: &mut Registry,
    install_root: &Path,
    base: &Thornlist,
    added: &[AddedPackage],
    local: Option<&LocalFacts>,
) -> Res<Vec<Present>> {
    let root = base.root();
    // What compose drops from the base: the checkouts of replaced repos that
    // an added package claims.
    let replaced: BTreeSet<&str> = added.iter().flat_map(|a| a.replaces.iter().map(String::as_str)).collect();
    let claimed: BTreeSet<&str> = added.iter().flat_map(|a| a.thorns.iter().map(|t| t.checkout.as_str())).collect();
    let fetched = crate::fetch::FetchState::read(install_root)?;
    let normalized = crate::fetch::git::normalize_url;

    let mut repos: Vec<BaseRepo> = Vec::new();
    for c in base.components() {
        let thorn = crate::sim::trim::is_arrangement_thorn(base, c);
        let flesh = crate::fetch::is_flesh(root, c);
        if !thorn && !flesh {
            continue;
        }
        let i = match repos.iter().position(|r| r.name == c.repo) {
            Some(i) => i,
            None => {
                let on_disk = (fetched.as_ref())
                    .and_then(|f| f.repos.get(&c.repo))
                    .filter(|r| c.url.as_deref().is_some_and(|u| normalized(u) == normalized(&r.url)))
                    .map(|r| r.head.clone());
                let commit = on_disk.or_else(|| c.commit.clone()).filter(|c| is_commit_id(c));
                let git = c.ty == ComponentType::Git;
                repos.push(BaseRepo { name: c.repo.clone(), url: c.url.clone(), commit, git, thorns: Vec::new(), flesh: false });
                repos.len() - 1
            }
        };
        let r = &mut repos[i];
        r.flesh |= flesh;
        let dropped = replaced.contains(c.repo.as_str()) && claimed.contains(c.checkout.as_str());
        if thorn && !dropped {
            r.thorns.push(c);
        }
    }
    repos.retain(|r| !r.thorns.is_empty() || r.flesh);

    // One lookup for every pinned git repo, each pin asked once (one repo
    // under two spellings is one pin, as the Cactarium compares URLs).
    let mut pins: Vec<PinQuery> = Vec::new();
    let mut pin_of: Vec<Option<usize>> = Vec::new();
    for r in &repos {
        let (Some(url), Some(commit), true) = (&r.url, &r.commit, r.git) else {
            pin_of.push(None);
            continue;
        };
        let same = |p: &PinQuery| p.commit == *commit && cactup_ecosystem_common::url::same_repo_lenient(&p.url, url);
        let i = match pins.iter().position(same) {
            Some(i) => i,
            None => {
                pins.push(PinQuery { url: url.clone(), commit: commit.clone() });
                pins.len() - 1
            }
        };
        pin_of.push(Some(i));
    }
    let answers = reg.lookup(&pins)?;
    let mapped: Vec<Option<&PinInfo>> = pin_of.iter().map(|i| i.map(|i| &answers[i])).collect();
    let package_of = |info: &PinInfo| info.package.as_deref().and_then(|p| PkgId::parse(p).ok());

    // Every version of every package present, in one wave.
    let mut ids: Vec<PkgId> = added.iter().filter_map(|a| PkgId::parse(&a.package).ok()).collect();
    for info in mapped.iter().copied().flatten() {
        if let (Some(id), Some(_)) = (package_of(info), &info.version) {
            ids.push(id);
        }
    }
    reg.load(&ids)?;

    let mut out: Vec<Present> = Vec::new();
    let taken = |out: &[Present], id: &PkgId| out.iter().any(|p| &p.release.id == id);
    for a in added {
        let id = PkgId::parse(&a.package).with_context(|| format!("installation.toml adds {}", a.package))?;
        if taken(&out, &id) {
            bail!("installation.toml adds {id} twice");
        }
        let at_commit = crate::commands::add::is_commit(&a.version) || crate::commands::add::is_tip(&a.version);
        let served = if at_commit {
            reg.snapshot(&id, &a.commit)?.map(|s| s.release)
        } else {
            let fits = |r: &&Release| r.at.version().is_some_and(|v| v.to_string() == a.version) && r.commit == a.commit;
            reg.releases(&id).and_then(|rs| rs.iter().find(fits)).cloned()
        };
        let checkouts: Vec<(&str, &str)> =
            a.thorns.iter().map(|t| (t.checkout.as_str(), t.repo_path.as_str())).collect();
        let release = match served {
            Some(r) => with_local_facts(r, &checkouts, local),
            None => unknown(&id, &a.commit, &a.url, false, &checkouts, local),
        };
        let own = a.thorns.first().map(|t| crate::commands::add::clone_of(&a.repo, t)).unwrap_or_default();
        let libraries = (a.libraries.iter())
            .filter_map(|l| {
                let inside = if l.inside == own {
                    None
                } else {
                    let holder = a.libraries.iter().find(|h| h.clone_name() == l.inside)?;
                    Some(PkgId::parse(&holder.package).ok()?)
                };
                let id = PkgId::parse(&l.package).ok()?;
                Some(InstalledLibrary { id, commit: l.commit.clone(), into: l.into.clone(), inside })
            })
            .collect();
        let enabled = checkouts.iter().map(|(c, _)| thorn_of(c).to_owned()).collect();
        out.push(Present { release, enabled, origin: Origin::Added, libraries });
    }
    for (r, info) in repos.iter().zip(&mapped) {
        let checkouts: Vec<(&str, &str)> =
            r.thorns.iter().map(|c| (c.checkout.as_str(), c.repo_path.as_deref().unwrap_or(""))).collect();
        let commit = r.commit.clone().unwrap_or_default();
        let url = r.url.clone().unwrap_or_default();
        let mapped_id = info.and_then(package_of).filter(|id| !taken(&out, id));
        let release = match (mapped_id, info) {
            (Some(id), Some(info)) => {
                let fits = |rel: &&Release| {
                    info.version.as_deref().is_some_and(|v| rel.at.version().is_some_and(|at| at.to_string() == v))
                        && rel.commit == commit
                };
                match reg.releases(&id).and_then(|rs| rs.iter().find(fits)).cloned() {
                    Some(rel) => with_local_facts(rel, &checkouts, local),
                    None => unknown(&id, &commit, &url, info.flesh, &checkouts, local),
                }
            }
            // The flesh is a registered package's or nothing the resolver
            // can speak of: an unmapped one has no thorns to clash with.
            _ if r.thorns.is_empty() => continue,
            _ => unknown(&PkgId::repo(&r.name), &commit, &url, false, &checkouts, local),
        };
        let enabled = checkouts.iter().map(|(c, _)| thorn_of(c).to_owned()).collect();
        out.push(Present { release, enabled, origin: Origin::Base { repo: r.name.clone() }, libraries: Vec::new() });
    }
    Ok(out)
}

/// The thorn name of an `Arrangement/Thorn` checkout.
fn thorn_of(checkout: &str) -> &str {
    checkout.rsplit('/').next().unwrap_or(checkout)
}

fn arrangement_of(checkout: &str) -> &str {
    checkout.rsplit_once('/').map_or("", |(a, _)| a)
}

/// A present release with the facts of its enabled thorns as the tree has
/// them (where they could be read), and every enabled thorn listed even if
/// the release does not name it.
fn with_local_facts(mut r: Release, checkouts: &[(&str, &str)], local: Option<&LocalFacts>) -> Release {
    let Some(local) = local else { return r };
    for (checkout, repo_path) in checkouts {
        let read = local.get(*checkout).cloned().flatten();
        match r.thorns.iter_mut().find(|t| t.thorn.eq_ignore_ascii_case(thorn_of(checkout))) {
            Some(t) => {
                if let Some(facts) = read {
                    t.facts = facts;
                }
            }
            None => r.thorns.push(ThornInfo {
                arrangement: arrangement_of(checkout).to_owned(),
                thorn: thorn_of(checkout).to_owned(),
                repo_path: (*repo_path).to_owned(),
                facts: read.unwrap_or_default(),
            }),
        }
    }
    r
}

/// A present release no version names (or of no registered package): its
/// thorns as the tree has them, no claims.
fn unknown(
    id: &PkgId,
    commit: &str,
    url: &str,
    flesh: bool,
    checkouts: &[(&str, &str)],
    local: Option<&LocalFacts>,
) -> Release {
    let thorns = (checkouts.iter())
        .map(|(checkout, repo_path)| ThornInfo {
            arrangement: arrangement_of(checkout).to_owned(),
            thorn: thorn_of(checkout).to_owned(),
            repo_path: (*repo_path).to_owned(),
            facts: local.and_then(|l| l.get(*checkout).cloned().flatten()).unwrap_or_default(),
        })
        .collect();
    Release {
        id: id.clone(),
        at: At::Unknown(commit.to_owned()),
        commit: commit.to_owned(),
        tag: None,
        url: url.to_owned(),
        contact: None,
        flesh,
        library: false,
        yanked: false,
        yank_reason: None,
        pending: false,
        usable: true,
        thorns,
        claims: Vec::new(),
    }
}

/// The thorns of the live list read from the tree, in parallel with
/// progress, keyed by checkout.
pub(crate) fn local_facts(live: &Thornlist, cactus_root: &Path) -> Res<LocalFacts> {
    let read = crate::sim::trim::thorn_facts(live, cactus_root)?;
    Ok(read.into_iter().map(|(checkout, (_, facts))| (checkout, facts)).collect())
}

/// The `added-for` entries a reader may believe, as the resolver reads them:
/// those of packages with an `[[added]]` entry. Whether each still holds for
/// the package's commit is [`deps::is_requested`]'s call.
pub(crate) fn added_for(meta: &InstallationMeta) -> BTreeMap<PkgId, AddedFor> {
    let mut out = BTreeMap::new();
    for (a, entry) in meta.added_for_present() {
        let (Ok(id), Some(e)) = (PkgId::parse(&a.package), entry) else { continue };
        let choices = (e.choices.iter())
            .filter_map(|c| {
                let dependent = PkgId::parse(&c.dependent).ok()?;
                let alternatives = c.alternatives.iter().map(|a| PkgId::parse(a).ok()).collect::<Option<Vec<_>>>()?;
                Some(ChoiceKey { dependent, alternatives })
            })
            .collect();
        let entry = AddedFor { requested: e.requested, by: e.by.clone(), choices, for_commit: e.for_commit.clone() };
        out.insert(id, entry);
    }
    out
}

/// An entry as `installation.toml` records it.
pub(crate) fn record_of(a: &AddedFor) -> crate::installation::AddedFor {
    crate::installation::AddedFor {
        requested: a.requested,
        by: a.by.clone(),
        for_commit: a.for_commit.clone(),
        choices: (a.choices.iter())
            .map(|c| crate::installation::AddedForChoice {
                dependent: c.dependent.to_string(),
                alternatives: c.alternatives.iter().map(ToString::to_string).collect(),
            })
            .collect(),
    }
}

/// What may go ahead without asking, and how.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Confirm {
    /// `--yes`: no question asked.
    pub yes: bool,
    /// `--dry-run`: show the plan, change nothing.
    pub dry_run: bool,
    /// A terminal to ask on.
    pub interactive: bool,
}

/// Prints a plan when it goes beyond what was named (or for `--dry-run`),
/// and asks before going on: `Proceed? [Y/n]` on a terminal, a refusal
/// without one unless `--yes`. Whatever it is, the lines of note (releases
/// awaiting review, notes) are printed. Whether to go on.
pub(crate) fn show_and_confirm(lines: &[(LineKind, String)], beyond_named: bool, how: Confirm) -> Res<bool> {
    let show_all = beyond_named || how.dry_run;
    for (kind, line) in lines {
        match kind {
            LineKind::Header | LineKind::Step if !show_all => {}
            LineKind::Header => println!("{}", line.bold()),
            LineKind::Pending => println!("{}", line.red()),
            LineKind::Rebuild => println!("{}", line.yellow()),
            LineKind::Step | LineKind::Note => println!("{line}"),
        }
    }
    if how.dry_run {
        println!("Nothing changed (--dry-run).");
        return Ok(false);
    }
    if !beyond_named || how.yes {
        return Ok(true);
    }
    if !how.interactive {
        // Plan §4.5's last row.
        bail!("The plan goes beyond the packages named, and there is no terminal to confirm it on; pass --yes, or --dry-run to see it");
    }
    use std::io::Write;
    eprint!("Proceed? [Y/n] ");
    std::io::stderr().flush()?;
    let answer = crate::commands::read_line_interruptibly()?;
    match answer.as_deref().map(str::trim) {
        Some("" | "y" | "Y" | "yes" | "Yes") => Ok(true),
        _ => {
            println!("Nothing changed.");
            Ok(false)
        }
    }
}

/// The refusal inside a resolver error, as its text (which ends with the way
/// past); any other error as it is.
pub(crate) fn refusal_text(e: anyhow::Error) -> anyhow::Error {
    match e.downcast::<deps::Refusal>() {
        Ok(r) => anyhow!("{r}"),
        Err(e) => e,
    }
}
