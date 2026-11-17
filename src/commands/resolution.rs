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
/// How often a request in flight checks for Ctrl-C.
const POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// The thorns read from the installed tree: checkout → its `.ccl` facts
/// (`None`: unreadable). What [`crate::sim::trim::thorn_facts`] gives, keyed
/// by checkout.
pub(crate) type LocalFacts = BTreeMap<String, Option<ThornFacts>>;

/// Runs `request` on a thread of its own and waits for it, checking `stop`
/// every [`POLL`]: once it is set, fails with "interrupted" at once and
/// leaves the request to finish (or time out) on its own. Blocking reqwest
/// cannot be cancelled, and a version list can take a while on a slow link.
pub(crate) fn interruptibly<T: Send + 'static>(
    request: impl FnOnce() -> Res<T> + Send + 'static,
    stop: &std::sync::atomic::AtomicBool,
) -> Res<T> {
    let (send, receive) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = send.send(request());
    });
    loop {
        if stop.load(std::sync::atomic::Ordering::Relaxed) {
            bail!("Interrupted");
        }
        match receive.recv_timeout(POLL) {
            Ok(answer) => return answer,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => bail!("A request to the Cactarium died"),
        }
    }
}

/// The Cactarium, as [`deps::resolve`] reads it: every package's versions
/// loaded once, in parallel waves, each request on a line under a headline;
/// snapshots and provider searches one at a time, each on its line too. One
/// renderer for the whole conversation, shut down by [`Registry::finish`] (or
/// on drop), so nothing may print to the terminal while one is alive. Every
/// request gives way to Ctrl-C at once ([`interruptibly`]); none has a limit
/// on its whole length, only the client's on connecting and on each stall.
pub(crate) struct Registry {
    client: Client,
    loaded: BTreeMap<PkgId, Option<Vec<Release>>>,
    contacts: BTreeMap<PkgId, String>,
    progress: std::sync::Arc<prodash::tree::Root>,
    renderer: Option<prodash::render::line::JoinHandle>,
    layout: Layout,
    search: Option<Line>,
    last_tick: usize,
    /// The Cactarium has no `/providers` (it is older than this cactup).
    no_providers: bool,
}

impl Registry {
    /// A conversation with the Cactarium `client` talks to; `names` sizes
    /// the progress lines (package ids and repo names known so far).
    pub(crate) fn new<'a>(client: &Client, names: impl IntoIterator<Item = &'a str>) -> Registry {
        // Only bars: no line here ever calls `info`/`fail`.
        let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
        let layout = Layout::for_names([ASK, SEARCH].into_iter().chain(names));
        Registry {
            client: client.clone(),
            loaded: BTreeMap::new(),
            contacts: BTreeMap::new(),
            progress,
            renderer,
            layout,
            search: None,
            last_tick: 0,
            no_providers: false,
        }
    }

    /// Stops the renderer: after this, the terminal is the caller's. Says
    /// whether the Cactarium could not list providers.
    pub(crate) fn finish(mut self) -> Finished {
        self.stop();
        Finished { base: self.client.base().to_owned(), no_providers: self.no_providers }
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

    /// One request, interruptibly, with a clone of the client.
    fn ask<T: Send + 'static>(&self, request: impl FnOnce(Client) -> Res<T> + Send + 'static) -> Res<T> {
        let client = self.client.clone();
        interruptibly(move || request(client), &gix::interrupt::IS_INTERRUPTED)
    }

    /// One request on a line of its own under the headline, `name` naming
    /// it and `phase` saying what it asks.
    fn one<T: Send + 'static>(
        &self,
        name: &str,
        phase: &str,
        request: impl FnOnce(Client) -> Res<T> + Send + 'static,
    ) -> Res<T> {
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let mut headline = self.headline(1, "requests");
        let line = Line::over(headline.add_child(name.to_owned()), name, self.layout.clone());
        line.phase(phase.to_owned());
        let answer = self.ask(request);
        drop(line);
        headline.inc();
        answer
    }

    /// `request` for each of `items`, in parallel, each on a line named by
    /// `name` under one headline, in the order given.
    fn wave<I: Sync, T: Send + 'static>(
        &self,
        items: &[I],
        unit: &'static str,
        name: impl Fn(&I) -> String + Sync,
        phase: &str,
        request: impl Fn(&I, Client) -> Box<dyn FnOnce() -> Res<T> + Send> + Sync,
    ) -> Res<Vec<Res<T>>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        if gix::interrupt::is_triggered() {
            bail!("Interrupted");
        }
        let headline = Mutex::new(self.headline(items.len(), unit));
        crate::par::parallel_map(items, |item| {
            let label = name(item);
            let line = Line::over(headline.lock().expect("progress poisoned").add_child(label.clone()), &label, self.layout.clone());
            line.phase(phase.to_owned());
            let job = request(item, self.client.clone());
            let answer = interruptibly(job, &gix::interrupt::IS_INTERRUPTED);
            drop(line);
            headline.lock().expect("progress poisoned").inc();
            answer
        })
    }

    /// Asks `/resolve` about each named package, as a package, in parallel:
    /// a name that is a stack or a sim, or that the Cactarium does not know,
    /// fails here with the Cactarium's own words.
    pub(crate) fn check_named(&self, specs: &[(String, String)]) -> Res<()> {
        let answers = self.wave(specs, "packages", |(entry, _)| entry.clone(), "resolving", |(_, req), c| {
            let req = req.clone();
            Box::new(move || c.resolve(&req, "package"))
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
        let pins = pins.to_vec();
        self.one("the installation's repos", &phase, move |c| c.lookup(&pins))
    }

    /// Each package read at a commit, in parallel: what the Cactarium serves
    /// there (the version that froze it, or the package read now), or `None`
    /// when it has no such package or commit.
    pub(crate) fn read_at(&mut self, at: &[(PkgId, String)]) -> Res<Vec<Option<Release>>> {
        let name = |(id, commit): &(PkgId, String)| format!("{id}#{}", commit.get(..12).unwrap_or(commit));
        let answers = self.wave(at, "packages", name, "reading it at its commit", |(id, commit), c| {
            let (id, commit) = (id.clone(), commit.clone());
            Box::new(move || {
                let (owner, name) = owner_name(&id)?;
                c.package_at_if_any(owner, name, &commit)
            })
        })?;
        let mut out = Vec::with_capacity(at.len());
        for ((id, commit), answer) in at.iter().zip(answers) {
            let read = answer.with_context(|| format!("Failed to read {id} at {commit}"))?;
            out.push(match read {
                Some((version, _)) => Some(Release::from_served(id, &version, self.contact(id))?),
                None => None,
            });
        }
        Ok(out)
    }

    /// The package's contact, as its versions answer gave it.
    fn contact(&self, id: &PkgId) -> Option<&str> {
        self.contacts.get(id).map(String::as_str)
    }
}

/// What a finished conversation leaves to say.
pub(crate) struct Finished {
    base: String,
    pub(crate) no_providers: bool,
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
        let answers = self.wave(&want, "packages", PkgId::to_string, "reading its versions", |id, c| {
            let id = id.clone();
            Box::new(move || {
                let (owner, name) = owner_name(&id)?;
                c.package_versions_if_any(owner, name)
            })
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
        let shown = if is_commit_id(at) || at.len() > 12 { at.get(..12).unwrap_or(at) } else { at };
        let label = format!("{id}@{shown}");
        let (who, at_owned) = (id.clone(), at.to_owned());
        let read = self.one(&label, "reading the package there", move |c| {
            let (owner, name) = owner_name(&who)?;
            c.package_at_if_any(owner, name, &at_owned)
        })?;
        let Some((version, untrusted)) = read else { return Ok(None) };
        let release = Release::from_served(id, &version, self.contact(id))?;
        Ok(Some(Snapshot { release, untrusted }))
    }

    fn providers(&mut self, requirement: &deps::Requirement) -> Res<Vec<Provider>> {
        let label = requirement.to_string();
        let need = requirement.clone();
        let found = match self.one(&label, "asking what provides it", move |c| c.providers(&need)) {
            Ok(found) => found,
            // An older Cactarium: the refusal stands, without the list.
            Err(e) if e.downcast_ref::<crate::cactarium::OldCactarium>().is_some() => {
                self.no_providers = true;
                return Ok(Vec::new());
            }
            Err(e) => return Err(e),
        };
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

/// How [`present`] reads the installation.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct PresentOptions {
    /// Present packages may move (`--upgrade`, `--upgrade-all`): read their
    /// whole version lists, which the resolver needs anyway. Otherwise each is
    /// read at its own commit only (one small answer each).
    pub full: bool,
    /// A Cactarium without `/lookup` (older than this cactup) is no error:
    /// every base repo is then taken as no registered package's.
    pub without_lookup: bool,
}

/// A present package whose release the Cactarium cannot give: what it
/// depends on is unknown.
#[derive(Clone, Debug)]
pub(crate) struct Unread {
    pub id: PkgId,
    /// As installed: `owner/name@1.2.0`, or at a commit.
    pub label: String,
    pub why: String,
    /// An `[[added]]` package (rather than a base repo's).
    pub added: bool,
}

impl Unread {
    /// `max/z4c@1.0.0 (the Cactarium no longer serves that version)`.
    pub fn line(&self) -> String {
        format!("{} ({})", self.label, self.why)
    }
}

/// The installation as [`deps::resolve`] and [`deps::plan_remove`] see it.
pub(crate) struct PresentSet {
    pub present: Vec<Present>,
    /// The present packages whose release could not be read.
    pub unread: Vec<Unread>,
}

/// The installation as [`deps::resolve`] and [`deps::plan_remove`] see it:
/// every added package at its release, and every base repo, mapped to its
/// package by the lookup (or a pseudo-package `repo:<name>` when it is no
/// registered package's), each with the thorns it has enabled. An `[[added]]`
/// entry that only gives a base repo's own package more thorns (at the base's
/// commit, in the base's clone) is one present package with that repo. Present
/// thorns carry the facts read from the tree (`local`), where they were read:
/// what is installed, rather than what the Cactarium scanned.
pub(crate) fn present(
    reg: &mut Registry,
    install_root: &Path,
    base: &Thornlist,
    added: &[AddedPackage],
    local: Option<&LocalFacts>,
    options: &PresentOptions,
) -> Res<PresentSet> {
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
    let answers = match reg.lookup(&pins) {
        Ok(answers) => answers,
        Err(e) if options.without_lookup && e.downcast_ref::<crate::cactarium::OldCactarium>().is_some() => {
            pin_of.iter_mut().for_each(|i| *i = None);
            Vec::new()
        }
        Err(e) => return Err(e),
    };
    let package_of = |info: &PinInfo| info.package.as_deref().and_then(|p| PkgId::parse(p).ok());
    let mapped: Vec<Option<PkgId>> =
        pin_of.iter().map(|i| i.and_then(|i| package_of(&answers[i])).filter(|id| !id.is_repo())).collect();

    // What to read: each added package at its commit (expecting its recorded
    // version, unless it is at a commit or tip), each mapped base repo at
    // its commit.
    struct Want {
        id: PkgId,
        commit: String,
        version: Option<String>,
    }
    let mut wants: Vec<Want> = Vec::new();
    for a in added {
        let id = PkgId::parse(&a.package).with_context(|| format!("installation.toml adds {}", a.package))?;
        let at_commit = crate::commands::add::is_commit(&a.version) || crate::commands::add::is_tip(&a.version);
        wants.push(Want { id, commit: a.commit.clone(), version: (!at_commit).then(|| a.version.clone()) });
    }
    for (r, id) in repos.iter().zip(&mapped) {
        if let (Some(id), Some(commit)) = (id, &r.commit) {
            wants.push(Want { id: id.clone(), commit: commit.clone(), version: None });
        }
    }
    let fits = |r: &Release, w: &Want| {
        r.commit == w.commit && w.version.as_ref().is_none_or(|v| r.at.version().is_some_and(|at| &at.to_string() == v))
    };
    let mut read: Vec<Option<Release>> = vec![None; wants.len()];
    if options.full {
        reg.load(&wants.iter().map(|w| w.id.clone()).collect::<Vec<_>>())?;
        for (w, slot) in wants.iter().zip(read.iter_mut()) {
            *slot = reg.releases(&w.id).and_then(|rs| rs.iter().find(|r| fits(r, w))).cloned();
        }
    }
    // Read at the commit: what is still missing (everything, unless full).
    let missing: Vec<usize> = (0..wants.len()).filter(|i| read[*i].is_none()).collect();
    let at: Vec<(PkgId, String)> = missing.iter().map(|i| (wants[*i].id.clone(), wants[*i].commit.clone())).collect();
    let answers = reg.read_at(&at)?;
    let mut astray: Vec<PkgId> = Vec::new();
    for (i, answer) in missing.into_iter().zip(answers) {
        match answer {
            Some(r) if fits(&r, &wants[i]) => read[i] = Some(r),
            // Another version froze that commit too (another track): the
            // recorded one is in the whole list.
            Some(_) => astray.push(wants[i].id.clone()),
            None => {}
        }
    }
    if !astray.is_empty() {
        reg.load(&astray)?;
        for (w, slot) in wants.iter().zip(read.iter_mut()) {
            if slot.is_none() && astray.contains(&w.id) {
                *slot = reg.releases(&w.id).and_then(|rs| rs.iter().find(|r| fits(r, w))).cloned();
            }
        }
    }
    let why = |w: &Want| match &w.version {
        Some(v) => format!("the Cactarium does not serve version {v} at commit {}", short(&w.commit)),
        None => format!("the Cactarium cannot read it at commit {}", short(&w.commit)),
    };

    let mut out: Vec<Present> = Vec::new();
    let mut unread: Vec<Unread> = Vec::new();
    let taken = |out: &[Present], id: &PkgId| out.iter().any(|p| &p.release.id == id);
    // An `[[added]]` entry that only gives a base repo's package more thorns.
    let extends = |a: &AddedPackage, r: &BaseRepo, id: &PkgId| {
        a.package == id.as_str() && a.replaces.is_empty() && a.repo == r.name && r.commit.as_deref() == Some(a.commit.as_str())
    };
    let mut merged: BTreeSet<usize> = BTreeSet::new();
    let mut next = added.len();
    for (r, id) in repos.iter().zip(&mapped) {
        let mut checkouts: Vec<(&str, &str)> =
            r.thorns.iter().map(|c| (c.checkout.as_str(), c.repo_path.as_deref().unwrap_or(""))).collect();
        let commit = r.commit.clone().unwrap_or_default();
        let url = r.url.clone().unwrap_or_default();
        let mut libraries = Vec::new();
        let w = id.as_ref().map(|_| {
            next += 1;
            next - 1
        });
        let release = match (id, w) {
            // Two base repos of one package: the second is a repo of its own.
            (Some(id), _) if taken(&out, id) => unknown(&PkgId::repo(&r.name), &commit, &url, false, &checkouts, local),
            (Some(id), Some(w)) => {
                if let Some(i) = added.iter().position(|a| extends(a, r, id)) {
                    merged.insert(i);
                    let a = &added[i];
                    checkouts.extend(a.thorns.iter().map(|t| (t.checkout.as_str(), t.repo_path.as_str())));
                    libraries = installed_libraries(a);
                }
                match &read[w] {
                    Some(rel) => with_local_facts(rel.clone(), &checkouts, local),
                    None => {
                        unread.push(Unread {
                            id: id.clone(),
                            label: format!("{id}#{}", short(&commit)),
                            why: why(&wants[w]),
                            added: false,
                        });
                        unknown(id, &commit, &url, r.flesh, &checkouts, local)
                    }
                }
            }
            // An unmapped flesh has no thorns to clash with: nothing the
            // resolver can speak of.
            _ if r.thorns.is_empty() => continue,
            _ => unknown(&PkgId::repo(&r.name), &commit, &url, false, &checkouts, local),
        };
        let enabled = checkouts.iter().map(|(c, _)| thorn_of(c).to_owned()).collect();
        out.push(Present { release, enabled, origin: Origin::Base { repo: r.name.clone() }, libraries });
    }
    // A base repo whose package an `[[added]]` entry also is (and does not
    // merely extend): the added one is that package, the repo's remaining
    // checkouts a pseudo-package of their own.
    for (i, a) in added.iter().enumerate() {
        if merged.contains(&i) {
            continue;
        }
        let w = &wants[i];
        if let Some(p) = out.iter_mut().find(|p| p.release.id == w.id && matches!(p.origin, Origin::Base { .. })) {
            let Origin::Base { repo } = &p.origin else { unreachable!() };
            p.release.id = PkgId::repo(repo);
            p.release.at = At::Unknown(p.release.commit.clone());
            p.release.claims.clear();
            unread.retain(|u| u.added || u.id != w.id);
        }
        if taken(&out, &w.id) {
            bail!("installation.toml adds {} twice", w.id);
        }
        let checkouts: Vec<(&str, &str)> =
            a.thorns.iter().map(|t| (t.checkout.as_str(), t.repo_path.as_str())).collect();
        let release = match &read[i] {
            Some(r) => with_local_facts(r.clone(), &checkouts, local),
            None => {
                let label = crate::commands::add::describe(&a.package, &a.version);
                unread.push(Unread { id: w.id.clone(), label, why: why(w), added: true });
                unknown(&w.id, &a.commit, &a.url, false, &checkouts, local)
            }
        };
        let enabled = checkouts.iter().map(|(c, _)| thorn_of(c).to_owned()).collect();
        out.push(Present { release, enabled, origin: Origin::Added, libraries: installed_libraries(a) });
    }
    Ok(PresentSet { present: out, unread })
}

/// An added package's libraries, as the resolver holds them.
fn installed_libraries(a: &AddedPackage) -> Vec<InstalledLibrary> {
    let own = a.thorns.first().map(|t| crate::commands::add::clone_of(&a.repo, t)).unwrap_or_default();
    (a.libraries.iter())
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
        .collect()
}

fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
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
/// past); any other error as it is. A Cactarium that could not list
/// providers (`finished.no_providers`) leaves the refusal standing, saying
/// so rather than that nothing provides them.
pub(crate) fn refusal_text(e: anyhow::Error, finished: &Finished) -> anyhow::Error {
    match e.downcast::<deps::Refusal>() {
        Ok(deps::Refusal::Unclaimed(unmet)) if finished.no_providers => {
            let lines: Vec<String> =
                unmet.iter().map(|u| format!("\n  {} ({}) {}", u.thorn, u.package, u.requirement)).collect();
            anyhow!(
                "Requirements nothing in the installation meets, which no manifest declares:{}.\nThe Cactarium at {} \
                 is older than this cactup and cannot list what provides them. Cactup adds only what a manifest \
                 declares: add a provider yourself first, or ask the package's authors to declare the dependency",
                lines.concat(),
                finished.base
            )
        }
        Ok(r) => anyhow!("{r}"),
        Err(e) => e,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};

    /// A request that would hang gives way to the stop flag within a poll
    /// or two, its thread left to finish on its own.
    #[test]
    fn a_request_in_flight_gives_way_to_ctrl_c() {
        let stop = std::sync::Arc::new(AtomicBool::new(false));
        let setter = stop.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            setter.store(true, Ordering::Relaxed);
        });
        let started = std::time::Instant::now();
        let err = interruptibly(
            || {
                std::thread::sleep(std::time::Duration::from_secs(30));
                Ok(())
            },
            &stop,
        )
        .unwrap_err();
        assert_eq!(err.to_string(), "Interrupted");
        assert!(started.elapsed() < std::time::Duration::from_secs(1), "{:?}", started.elapsed());
        // And an answer comes back as it is.
        assert_eq!(interruptibly(|| Ok(7), &AtomicBool::new(false)).unwrap(), 7);
    }
}
