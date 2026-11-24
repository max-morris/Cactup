//! What `cactup add` and `cactup remove` ask Cactarium, and what they
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
use cactup_ecosystem_common::api::{
    PackageVersion, PinInfo, PinQuery, ReadQuery, ReadRequest, ReadStatus, ThornInfo, MAX_READ_ITEMS,
};
use cactup_ecosystem_common::ccl::ThornFacts;
use cactup_ecosystem_common::slug::is_commit_id;
use colored::Colorize;

use crate::cactarium::Client;
use crate::deps::{
    self, AddedFor, At, ChoiceKey, Facts, InstalledLibrary, LineKind, Origin, PkgId, Present, Provider, Release,
    Snapshot, Unanswered, UnreadKind,
};
use crate::installation::{AddedPackage, InstallationMeta};
use crate::progress::{Headline, Layout, Line};
use crate::thornlist::{Component, ComponentType, Thornlist};
use crate::Res;

/// The headline over every request to Cactarium.
const ASK: &str = "ask Cactarium";
/// The line a long search shows on.
const SEARCH: &str = "look for a plan";
/// The line a wait for Cactarium's read limit shows on.
const READ_LIMIT: &str = "Cactarium's read limit";
/// The headline over reading the installation's clones.
const CLONES: &str = "read the installation's clones";
/// How many search steps go by between two updates of the search line.
const TICK_EVERY: usize = 1024;

/// The thorns read from the installed tree: checkout → its `.ccl` facts
/// (`None`: unreadable). What [`crate::sim::trim::thorn_facts`] gives, keyed
/// by checkout.
pub(crate) type LocalFacts = BTreeMap<String, Option<ThornFacts>>;

/// Cactarium, as [`deps::resolve`] reads it: every package's versions
/// loaded once, in parallel waves, each request on a line under a headline;
/// snapshots and provider searches one at a time, each on its line too. One
/// renderer for the whole conversation, shut down by [`Registry::finish`] (or
/// on drop), so nothing may print to the terminal while one is alive. Every
/// request gives way to Ctrl-C at once (the client's own doing); none has a
/// limit on its whole length, only the patient client's on the head of each
/// answer and on each stall.
pub(crate) struct Registry {
    client: Client,
    loaded: BTreeMap<PkgId, Option<Vec<Release>>>,
    contacts: BTreeMap<PkgId, String>,
    progress: std::sync::Arc<prodash::tree::Root>,
    renderer: Option<crate::manifest::JoinHandle>,
    layout: Layout,
    search: Option<Line>,
    last_tick: usize,
    /// Cactarium has no `/providers` (it is older than this cactup).
    no_providers: bool,
    /// Snapshots not answered now, by package and what was asked, with why.
    unanswered: BTreeMap<(PkgId, String), Unanswered>,
    /// When the server's window for reads at a commit opened (the first read
    /// since the last wait), for an older server's 429 without `Retry-After`.
    window_start: Option<std::time::Instant>,
    /// How long this command has waited for the read limit so far.
    waited: std::time::Duration,
    /// The packages read at a commit, and the commits (full) they were read at.
    read: BTreeSet<(PkgId, String)>,
    /// What batch reads answered, by package and full commit: what a read
    /// of one of them needs no request for.
    batch: BTreeMap<(PkgId, String), Batched>,
    /// No batch request again in this command: Cactarium has no batch
    /// reads (older than this cactup), or answered one with something cactup
    /// cannot use at all. Every read left is a single one, as before.
    no_batch: bool,
    /// Batch requests sent, and rounds of them (for a test's count).
    pub(crate) batch_requests: usize,
    pub(crate) batch_rounds: usize,
    /// The waits between batch rounds, in order (for a test).
    pub(crate) pauses: Vec<std::time::Duration>,
    /// Items a batch answered in a way cactup cannot use: read singly, never
    /// asked in a batch again in this command.
    unusable: BTreeSet<(PkgId, String)>,
    /// When batch reads end in this command: set by the first prefetch, and
    /// shared by every later one (a command reads in batches for at most
    /// [`BATCH_DEADLINE`] in all, however many phases it reads in).
    batch_until: Option<std::time::Instant>,
}

/// What a batch read answered for one package at one commit.
#[derive(Clone)]
enum Batched {
    /// Read: what the single read answers, and whether it is untrusted.
    Read(Box<PackageVersion>, bool),
    /// Not readable there, with why.
    Unread(Unanswered),
}

/// How long to wait before asking again for what a batch round did not read:
/// after a round that read something, briefly (what the server is still
/// fetching is usually ready by then); after one that read nothing (the
/// server's fetch slots were busy), longer, and at most [`STALLS`] times in a
/// row before leaving the rest to single reads.
const ROUND_WAIT: std::time::Duration = std::time::Duration::from_millis(1500);
const STALL_WAIT: std::time::Duration = std::time::Duration::from_secs(3);
const STALLS: usize = 3;
/// The longest batch reads take in a command, requests and waits together,
/// over every phase that reads in batches (one deadline for the command, set
/// by its first), and the most rounds one phase makes: then what is left is
/// read singly when needed. The single reads' [`WAIT_BUDGET`] is apart from
/// it, so a command waits at most both: 5 minutes of batch reads, then up to
/// 5 minutes for the read limit of single reads.
const BATCH_DEADLINE: std::time::Duration = std::time::Duration::from_secs(300);
const MAX_ROUNDS: usize = 30;
/// The line the batch phase shows what it is doing on.
const BATCH: &str = "batch reads";

#[cfg(test)]
thread_local! {
    /// Both waits between batch rounds, scaled down for a test (the stall's
    /// twice the other's, as the real ones are).
    pub(crate) static TEST_ROUND_WAIT: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
    /// The batch phase's deadline, scaled down for a test.
    pub(crate) static TEST_BATCH_DEADLINE: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

fn batch_deadline() -> std::time::Duration {
    #[cfg(test)]
    if let Some(d) = TEST_BATCH_DEADLINE.with(|d| d.get()) {
        return d;
    }
    BATCH_DEADLINE
}

fn round_wait(stalled: bool) -> std::time::Duration {
    #[cfg(test)]
    if let Some(w) = TEST_ROUND_WAIT.with(|w| w.get()) {
        return if stalled { w * 2 } else { w };
    }
    if stalled { STALL_WAIT } else { ROUND_WAIT }
}

/// What a batch item's status says, in the single read's words.
fn batch_why(id: &PkgId, status: &ReadStatus, error: Option<&str>) -> Option<Unanswered> {
    let message = error.unwrap_or_default();
    let (kind, why) = match status {
        ReadStatus::Forbidden => (UnreadKind::Untrusted, "it reads an untrusted package only at released versions".to_owned()),
        // The server's words say which: no such commit (is it pushed?), or no such package.
        ReadStatus::NotFound => match error {
            Some(error) => (UnreadKind::Missing, error.to_owned()),
            None => return Some(missing_commit(id)),
        },
        ReadStatus::BadRequest => (UnreadKind::Invalid, message.to_owned()),
        ReadStatus::Upstream => (UnreadKind::Retry, "it could not reach the package's repository just now".to_owned()),
        ReadStatus::Ok | ReadStatus::NotRead | ReadStatus::Unknown => return None,
    };
    Some(Unanswered { kind, why })
}

/// An older server's read limit without `Retry-After`: 12 reads a minute,
/// in a window that opens at the first read.
const READ_WINDOW: std::time::Duration = std::time::Duration::from_secs(60);
/// The longest one wait for the read limit lasts, whatever `Retry-After` says.
const MAX_WAIT: std::time::Duration = std::time::Duration::from_secs(120);
/// The longest a command waits for the read limit in all: past it, the reads
/// left are unread.
const WAIT_BUDGET: std::time::Duration = std::time::Duration::from_secs(300);

#[cfg(test)]
thread_local! {
    /// The read window, scaled down for a test.
    pub(crate) static TEST_READ_WINDOW: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

fn read_window() -> std::time::Duration {
    #[cfg(test)]
    if let Some(w) = TEST_READ_WINDOW.with(|w| w.get()) {
        return w;
    }
    READ_WINDOW
}

fn wait_budget() -> std::time::Duration {
    #[cfg(test)]
    if let Some(w) = TEST_WAIT_BUDGET.with(|w| w.get()) {
        return w;
    }
    WAIT_BUDGET
}

#[cfg(test)]
thread_local! {
    /// The wait budget, scaled down for a test.
    pub(crate) static TEST_WAIT_BUDGET: std::cell::Cell<Option<std::time::Duration>> =
        const { std::cell::Cell::new(None) };
}

/// A read at a commit that Cactarium did not answer with the package: its
/// status and why, in words that say what is true (they follow
/// "Cactarium did not read <id> there: ").
fn unanswered_why(r: &crate::cactarium::Refused) -> Unanswered {
    let (status, message) = (r.status, r.message.as_str());
    let (kind, why) = match (status, r.code.as_deref()) {
        (429, _) => (UnreadKind::Retry, "its read limit held after waiting; try again in a few minutes".to_owned()),
        (403, _) => (UnreadKind::Untrusted, "it reads an untrusted package only at released versions".to_owned()),
        (400, _) => (UnreadKind::Invalid, message.to_owned()),
        (502, Some("upstream")) => (UnreadKind::Retry, "it could not reach the package's repository just now".to_owned()),
        // No Cactarium error body: a proxy in front of it answered.
        (_, None) => (UnreadKind::Retry, format!("it (or a proxy in front of it) failed ({status}); try again later")),
        _ => (UnreadKind::Retry, format!("it failed ({status}): {message}")),
    };
    Unanswered { kind, why }
}

/// The 404 of a read at a commit: the package's repository has no such commit.
fn missing_commit(id: &PkgId) -> Unanswered {
    let why = format!("{id} is at a commit unknown to Cactarium (is it pushed?)");
    Unanswered { kind: UnreadKind::Missing, why }
}

/// A read at a commit refused for that read's own reason, as its status and
/// message.
fn refused_read(e: &anyhow::Error) -> Option<&crate::cactarium::Refused> {
    e.downcast_ref::<crate::cactarium::Refused>().filter(|r| r.is_unread())
}

impl Registry {
    /// A conversation with the Cactarium server that `client` talks to; `names` sizes
    /// the progress lines (package ids and repo names known so far).
    pub(crate) fn new<'a>(client: &Client, names: impl IntoIterator<Item = &'a str>) -> Registry {
        // Only bars: no line here ever calls `info`/`fail`.
        let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
        let layout = Layout::for_names([ASK, SEARCH, CLONES, READ_LIMIT, BATCH].into_iter().chain(names));
        Registry {
            client: client.clone().patient(),
            loaded: BTreeMap::new(),
            contacts: BTreeMap::new(),
            progress,
            renderer,
            layout,
            search: None,
            last_tick: 0,
            no_providers: false,
            unanswered: BTreeMap::new(),
            window_start: None,
            waited: std::time::Duration::ZERO,
            read: BTreeSet::new(),
            batch: BTreeMap::new(),
            no_batch: false,
            batch_requests: 0,
            batch_rounds: 0,
            pauses: Vec::new(),
            unusable: BTreeSet::new(),
            batch_until: None,
        }
    }

    /// Stops the renderer: after this, the terminal is the caller's. Says
    /// whether Cactarium could not list providers.
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
        let answer = request(self.client.clone());
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
            let answer = job();
            drop(line);
            headline.lock().expect("progress poisoned").inc();
            answer
        })
    }

    /// `work` for each of `items` on the local disk, in parallel, each on a
    /// line named by `name` under one headline, in the order given.
    fn local<I: Sync, T: Send>(
        &self,
        items: &[I],
        unit: &'static str,
        name: impl Fn(&I) -> String + Sync,
        phase: &str,
        work: impl Fn(&I) -> T + Sync,
    ) -> Res<Vec<T>> {
        if items.is_empty() {
            return Ok(Vec::new());
        }
        let item = self.progress.add_child(CLONES);
        let headline = Mutex::new(Headline::over(item, CLONES, items.len(), unit, self.layout.clone()));
        crate::par::parallel_map(items, |i| {
            let label = name(i);
            let child = headline.lock().expect("progress poisoned").add_child(label.clone());
            let line = Line::over(child, &label, self.layout.clone());
            line.phase(phase.to_owned());
            let out = work(i);
            drop(line);
            headline.lock().expect("progress poisoned").inc();
            out
        })
    }

    /// Asks `/resolve` about each named package, as a package, in parallel:
    /// a name that is a stack or a sim, or that Cactarium does not know,
    /// fails here with Cactarium's own words.
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

    /// Each package read at a commit, in parallel: what Cactarium serves
    /// there (the version that froze it, or the package read now), or `None`
    /// when it has no such package or commit.
    pub(crate) fn read_at(&mut self, at: &[(PkgId, String)]) -> Res<Vec<Read>> {
        // What a batch read answered first; the rest read one by one.
        let mut batched: Vec<Option<Read>> = Vec::with_capacity(at.len());
        for (id, commit) in at {
            batched.push(self.batched(id, commit));
        }
        let rest: Vec<(PkgId, String)> =
            at.iter().zip(&batched).filter(|(_, b)| b.is_none()).map(|(k, _)| k.clone()).collect();
        let mut singles = self.read_singly(&rest)?.into_iter();
        Ok(batched.into_iter().map(|b| b.unwrap_or_else(|| singles.next().expect("one read each"))).collect())
    }

    /// [`Registry::read_at`], each read a single request.
    fn read_singly(&mut self, at: &[(PkgId, String)]) -> Res<Vec<Read>> {
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
            // Rate-limited: read again, one at a time, once the limit lifts.
            // Any other refusal of the read: that package is unread, the
            // command goes on.
            let refused = (answer.as_ref().err()).and_then(refused_read).map(unanswered_why);
            let status = (answer.as_ref().err()).and_then(refused_read).map(|r| r.status);
            let answer = match (status, refused) {
                (Some(429), _) => match self.read_one(id, commit)? {
                    Ok(read) => Ok(read),
                    Err(why) => {
                        out.push(Read::Refused(why));
                        continue;
                    }
                },
                (_, Some(why)) => {
                    out.push(Read::Refused(why));
                    continue;
                }
                _ => answer,
            };
            out.push(match answer {
                Ok(Some((version, _))) => Read::Found(Box::new(Release::from_served(id, &version, self.contact(id))?)),
                Ok(None) => Read::Missing,
                Err(e) => return Err(e.context(format!("Failed to read {id} at {commit}"))),
            });
        }
        Ok(out)
    }

    /// One read at a commit, waiting out the server's read limit: on a 429,
    /// for its `Retry-After` (at most [`MAX_WAIT`]), else until the window
    /// this command opened has passed, then again, at most twice, and never
    /// past [`WAIT_BUDGET`] of waiting in all. `Err(why)`: not answered.
    fn read_one(&mut self, id: &PkgId, at: &str) -> Res<Result<Option<(PackageVersion, bool)>, Unanswered>> {
        let label = match at {
            "head" | "master" => format!("{id}@{at}"),
            commit => format!("{id}#{}", commit.get(..12).unwrap_or(commit)),
        };
        for attempt in 0..3 {
            let (who, at_owned) = (id.clone(), at.to_owned());
            let read = self.one(&label, "reading the package there", move |c| {
                let (owner, name) = owner_name(&who)?;
                c.package_at_if_any(owner, name, &at_owned)
            });
            // The window opened when the server counted the first read: no
            // later than its answer came back, so waiting from then is safe.
            self.window_start.get_or_insert_with(std::time::Instant::now);
            let refused = read.as_ref().err().and_then(refused_read);
            match refused {
                None => return read.map(Ok),
                Some(r) if r.status == 429 && attempt < 2 => {
                    let retry_after = r.retry_after;
                    if !self.wait_for_limit(retry_after)? {
                        let why = format!(
                            "its read limit held after {} of waiting; try again later",
                            minutes(self.waited)
                        );
                        return Ok(Err(Unanswered { kind: UnreadKind::Retry, why }));
                    }
                }
                Some(r) => return Ok(Err(unanswered_why(r))),
            }
        }
        unreachable!("the last attempt answers")
    }

    /// Reads `items` (packages at full commits) in batches of at most
    /// [`MAX_READ_ITEMS`], in a stable order, keeping what each answers for
    /// the reads to come. What the server has not read yet comes back
    /// `not-read`, and is asked again, round after round: 1.5 s later while
    /// some of it is being read in the background (`in-progress`), 3 s later
    /// when none of it is (the server's network slots were taken, or its
    /// time ran out), at most [`STALLS`] such rounds in a row. The phase,
    /// requests and rate-limit waits included, keeps within [`MAX_ROUNDS`]
    /// rounds and the command's batch deadline ([`BATCH_DEADLINE`] from its
    /// first prefetch, shared by all of them; apart from the single reads'
    /// [`WAIT_BUDGET`]): one that starts past it asks nothing. What is still not read
    /// then is left to a single read when it is needed; so is an item cactup
    /// cannot use (answered for another package or commit, a version that is
    /// no release of it), which no batch of this command asks for again (it
    /// counts as done on the bar), and everything after an answer it cannot
    /// use at all (no batch request again in this command). A Cactarium without batch reads leaves everything to
    /// single reads. Shown as packages read out of all, with one line saying
    /// what the phase is doing.
    pub(crate) fn prefetch(&mut self, items: &[(PkgId, String)]) -> Res<()> {
        let known = |k: &(PkgId, String)| self.batch.contains_key(k) || self.unusable.contains(k);
        let mut pending: Vec<(PkgId, String)> = (items.iter())
            .filter(|(id, c)| !id.is_repo() && is_commit_id(c) && !known(&(id.clone(), c.clone())))
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if pending.is_empty() || self.no_batch {
            return Ok(());
        }
        let deadline = *self.batch_until.get_or_insert_with(|| std::time::Instant::now() + batch_deadline());
        if std::time::Instant::now() >= deadline {
            return Ok(());
        }
        let mut headline = self.headline(pending.len(), "packages");
        let status = Line::over(headline.add_child(BATCH.to_owned()), BATCH, self.layout.clone());
        let mut stalls = 0;
        for round in 1..=MAX_ROUNDS {
            self.batch_rounds += 1;
            let (mut next, mut in_progress, mut stopped) = (Vec::new(), false, false);
            for chunk in pending.chunks(MAX_READ_ITEMS) {
                if stopped || std::time::Instant::now() >= deadline {
                    stopped = true;
                    break;
                }
                status.phase(format!("round {round}: reading {} packages", chunk.len()));
                let Some(answer) = self.read_batch(chunk, &status, deadline)? else {
                    stopped = true;
                    break;
                };
                for ((id, commit), item) in chunk.iter().zip(answer.items) {
                    let key = (id.clone(), commit.clone());
                    // Answered for what was asked (a package name in any case, the commit
                    // exactly), or the item is of no use: read singly, never asked again.
                    if !item.package.eq_ignore_ascii_case(id.as_str()) || item.commit != *commit {
                        self.unusable.insert(key);
                        headline.inc();
                        continue;
                    }
                    let read = match (&item.status, item.version) {
                        (ReadStatus::Ok, Some(version)) => {
                            // Every server with batch reads says `untrusted`; absent, it would
                            // read as trusted.
                            match Release::from_served(id, &version, self.contact(id)) {
                                Ok(r) if r.commit == *commit => Batched::Read(Box::new(version), item.untrusted),
                                _ => {
                                    self.unusable.insert(key);
                                    headline.inc();
                                    continue;
                                }
                            }
                        }
                        (ReadStatus::Ok, None) => {
                            self.unusable.insert(key);
                            headline.inc();
                            continue;
                        }
                        (status, _) => match batch_why(id, status, item.error.as_deref()) {
                            Some(why) => Batched::Unread(why),
                            None => {
                                in_progress |= item.in_progress;
                                next.push(key);
                                continue;
                            }
                        },
                    };
                    self.batch.insert(key, read);
                    headline.inc();
                }
            }
            if next.is_empty() || stopped {
                break;
            }
            // None of what is left is being read: ask again a little later, a few times.
            let stalled = !in_progress;
            stalls = if stalled { stalls + 1 } else { 0 };
            if stalls > STALLS || round == MAX_ROUNDS {
                break;
            }
            let doing = if stalled { "waiting for Cactarium's network" } else { "waiting for what it is reading" };
            if !self.pause(round_wait(stalled), deadline, &status, doing)? {
                break;
            }
            pending = next;
        }
        Ok(())
    }

    /// One batch request, the read limit waited out (at most twice, each
    /// wait shown on `status`), never sent or waited for past `deadline`.
    /// `None`: not answered (the deadline reached, a Cactarium without batch
    /// reads, an answer cactup cannot use, the limit outlasting the
    /// deadline, the server failing): those reads are left to single ones.
    /// A Cactarium without batch reads, and one whose batch answer cactup
    /// cannot use at all, is not sent another batch request in this command.
    fn read_batch(
        &mut self,
        chunk: &[(PkgId, String)],
        status: &Line,
        deadline: std::time::Instant,
    ) -> Res<Option<cactup_ecosystem_common::api::ReadAnswer>> {
        let request = ReadRequest {
            items: chunk.iter().map(|(id, c)| ReadQuery { package: id.to_string(), commit: c.clone() }).collect(),
        };
        for attempt in 0..3 {
            if gix::interrupt::is_triggered() {
                bail!("Interrupted");
            }
            // Past the phase's deadline, not another request: what is left is read singly.
            if std::time::Instant::now() >= deadline {
                return Ok(None);
            }
            self.batch_requests += 1;
            let answer = self.client.read_packages(&request);
            if let Err(e) = &answer
                && e.downcast_ref::<crate::cactarium::OldCactarium>().is_some()
            {
                self.no_batch = true;
                return Ok(None);
            }
            // The request counted against the window of reads made now.
            self.window_start.get_or_insert_with(std::time::Instant::now);
            let e = match answer {
                Ok(answer) => return Ok(Some(answer)),
                Err(e) => e,
            };
            if e.downcast_ref::<crate::cactarium::UnusableAnswer>().is_some() {
                self.no_batch = true;
                return Ok(None);
            }
            match refused_read(&e) {
                Some(r) if r.status == 429 && attempt < 2 => {
                    let now = std::time::Instant::now();
                    let wait = wait_length(r.retry_after, self.window_start, now, MAX_WAIT);
                    if !self.pause(wait, deadline, status, "waiting for the read limit")? {
                        return Ok(None);
                    }
                    // A new window opens at the next read.
                    self.window_start = None;
                }
                Some(_) => return Ok(None),
                None => return Err(e),
            }
        }
        Ok(None)
    }

    /// Waits `wait` within the batch phase, saying `doing` on `status` with
    /// the seconds left; Ctrl-C ends it at once. The phase has its own time
    /// (its `deadline`), apart from the single reads' [`WAIT_BUDGET`]: a wait
    /// that would reach the deadline is not made at all (what it would wait
    /// for could not be asked after it), and `false` says so.
    fn pause(&mut self, wait: std::time::Duration, deadline: std::time::Instant, status: &Line, doing: &str) -> Res<bool> {
        let now = std::time::Instant::now();
        if now + wait >= deadline {
            return Ok(false);
        }
        let until = now + wait;
        loop {
            if gix::interrupt::is_triggered() {
                bail!("Interrupted");
            }
            let left = until.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            status.phase(format!("{doing}, {} s", left.as_secs() + 1));
            std::thread::sleep(left.min(std::time::Duration::from_millis(50)));
        }
        self.pauses.push(wait);
        Ok(true)
    }

    /// Whether a batch read `id` at `commit` (a test's count).
    #[cfg(test)]
    pub(crate) fn batch_read(&self, id: &PkgId, commit: &str) -> bool {
        matches!(self.batch.get(&(id.clone(), commit.to_owned())), Some(Batched::Read(..)))
    }

    /// What a batch read answered for `id` at `commit`, as a single read
    /// would have: `None` when no batch read it (or what it answered is no
    /// release, which a batch never keeps).
    fn batched(&mut self, id: &PkgId, commit: &str) -> Option<Read> {
        let read = self.batch.get(&(id.clone(), commit.to_owned()))?.clone();
        match read {
            Batched::Read(version, _) => {
                Release::from_served(id, &version, self.contact(id)).ok().map(|r| Read::Found(Box::new(r)))
            }
            Batched::Unread(why) => Some(Read::Refused(why)),
        }
    }

    /// Waits for the server's read limit to lift: `Retry-After` seconds (at
    /// most [`MAX_WAIT`]), or (an older server) until the window opened by
    /// this command's first read has passed; never past what is left of
    /// [`WAIT_BUDGET`]. Shown on a line with the time waited so far; Ctrl-C
    /// ends it at once. `false`: the budget is spent, and the read is not
    /// tried again.
    fn wait_for_limit(&mut self, retry_after: Option<u64>) -> Res<bool> {
        let left_in_budget = wait_budget().saturating_sub(self.waited);
        if left_in_budget.is_zero() {
            return Ok(false);
        }
        let now = std::time::Instant::now();
        let until = now + wait_length(retry_after, self.window_start, now, left_in_budget);
        let mut headline = self.headline(1, "waits");
        // The seconds waited fill the bar and count up beside it; what
        // changes stays at the front of the text, which the layout clips.
        let mut line = Line::counting(headline.add_child(READ_LIMIT), READ_LIMIT, self.layout.clone());
        let total = until.saturating_duration_since(now).as_secs().max(1) as usize;
        prodash::Progress::init(&mut line, Some(total), Some(prodash::unit::label("s")));
        let waited_before = self.waited;
        if !waited_before.is_zero() {
            line.phase(format!("waiting, {} in all before", minutes(waited_before)));
        } else {
            line.phase("waiting".to_owned());
        }
        let outcome = loop {
            if gix::interrupt::is_triggered() {
                break Err(anyhow!("Interrupted"));
            }
            let at = std::time::Instant::now();
            self.waited = waited_before + at.saturating_duration_since(now);
            let left = until.saturating_duration_since(at);
            if left.is_zero() {
                break Ok(());
            }
            prodash::Count::set(&line, at.saturating_duration_since(now).as_secs() as usize);
            std::thread::sleep(left.min(std::time::Duration::from_millis(50)));
        };
        drop(line);
        headline.inc();
        outcome?;
        // A new window opens at the next read.
        self.window_start = None;
        Ok(true)
    }

    /// The package's contact, as its versions answer gave it.
    fn contact(&self, id: &PkgId) -> Option<&str> {
        self.contacts.get(id).map(String::as_str)
    }
}

/// How long one wait for the read limit lasts: `Retry-After` (whole
/// seconds, at least one, at most [`MAX_WAIT`]), else until the window
/// opened at `window_start` has passed; never past `left` of the budget.
fn wait_length(
    retry_after: Option<u64>,
    window_start: Option<std::time::Instant>,
    now: std::time::Instant,
    left: std::time::Duration,
) -> std::time::Duration {
    let wait = match retry_after {
        Some(secs) => std::time::Duration::from_secs(secs.max(1)).min(MAX_WAIT),
        None => (window_start.unwrap_or(now) + read_window()).saturating_duration_since(now),
    };
    wait.min(left)
}

/// `1 min 20 s`, `45 s`.
fn minutes(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    match (secs / 60, secs % 60) {
        (0, s) => format!("{s} s"),
        (m, 0) => format!("{m} min"),
        (m, s) => format!("{m} min {s} s"),
    }
}

/// A package read at a commit.
pub(crate) enum Read {
    Found(Box<Release>),
    /// No such package or commit.
    Missing,
    /// Not answered with the package (see [`Refused::is_unread`]), with why.
    ///
    /// [`Refused::is_unread`]: crate::cactarium::Refused::is_unread
    Refused(Unanswered),
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
        // What a batch read answered needs no request.
        if let Some(read) = self.batch.get(&(id.clone(), at.to_owned())).cloned() {
            return match read {
                Batched::Read(version, untrusted) => {
                    let release = Release::from_served(id, &version, self.contact(id))?;
                    self.read.insert((id.clone(), release.commit.clone()));
                    Ok(Some(Snapshot { release, untrusted }))
                }
                Batched::Unread(why) => {
                    self.unanswered.insert((id.clone(), at.to_owned()), why);
                    Ok(None)
                }
            };
        }
        // Read once per package and commit for the whole command (the
        // catalog keeps what came back); the read limit waited out.
        let read = match self.read_one(id, at)? {
            Ok(read) => read,
            // Not answered: no such commit as far as this search goes, and
            // the reason kept for a refusal that depends on it.
            Err(why) => {
                self.unanswered.insert((id.clone(), at.to_owned()), why);
                return Ok(None);
            }
        };
        let Some((version, untrusted)) = read else { return Ok(None) };
        match Release::from_served(id, &version, self.contact(id)) {
            Ok(release) => {
                self.read.insert((id.clone(), release.commit.clone()));
                Ok(Some(Snapshot { release, untrusted }))
            }
            // An answer that is no release: that package unread, the command goes on.
            Err(e) => {
                let why = Unanswered { kind: UnreadKind::Invalid, why: e.to_string() };
                self.unanswered.insert((id.clone(), at.to_owned()), why);
                Ok(None)
            }
        }
    }

    fn unanswered(&self, id: &PkgId, at: &str) -> Option<Unanswered> {
        self.unanswered.get(&(id.clone(), at.to_owned())).cloned()
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
    /// Read no base package's release (`--no-deps`: nothing beyond what was
    /// named): base packages are present with their dependencies unknown.
    pub no_base_reads: bool,
}

/// An added package whose release Cactarium cannot give: what it
/// depends on is unknown.
#[derive(Clone, Debug)]
pub(crate) struct Unread {
    pub id: PkgId,
    /// As installed: `owner/name@1.2.0`, or at a commit.
    pub label: String,
    pub why: String,
}

impl Unread {
    /// `max/z4c@1.0.0: Cactarium does not serve version 1.0.0 at commit …`.
    pub fn line(&self) -> String {
        format!("{}: {}", self.label, self.why)
    }
}

impl PresentSet {
    /// Forgets, among the base packages whose dependencies are unknown, those
    /// `reg` has read at their commits since (the resolver read them).
    pub(crate) fn forget_read(&mut self, reg: &Registry) {
        let read: Vec<String> = (self.present.iter())
            .filter(|p| p.stand_in && reg.read.contains(&(p.release.id.clone(), p.release.commit.clone())))
            .map(|p| format!("{}#", p.release.id))
            .collect();
        self.unchecked.retain(|u| !read.iter().any(|r| u.starts_with(r.as_str())));
    }
}

/// The installation as [`deps::resolve`] and [`deps::plan_remove`] see it.
pub(crate) struct PresentSet {
    pub present: Vec<Present>,
    /// The added packages whose release could not be read.
    pub unread: Vec<Unread>,
    /// The base packages whose dependencies are unknown: at a commit no
    /// version froze (normal for a base that follows branches), not read,
    /// or unreadable now. As installed.
    pub unchecked: Vec<String>,
    /// The commit HEAD is at in each clone cactup fetched (the fetch
    /// record's where the clone cannot be read), by repo.
    pub heads: BTreeMap<String, String>,
}

/// The installation as [`deps::resolve`] and [`deps::plan_remove`] see it:
/// every added package at its release, and every base repo, mapped to its
/// package by the lookup (or a pseudo-package `local:<name>` when it is no
/// registered package's), each with the thorns it has enabled. An `[[added]]`
/// entry that only gives a base repo's own package more thorns (at the base's
/// commit, in the base's clone) is one present package with that repo. Present
/// thorns carry the facts read from the tree (`local`), where they were read:
/// what is installed, rather than what Cactarium scanned.
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
    // Where HEAD is in each clone cactup fetched (a pull by hand moves it
    // without the fetch record), read once for the command, in parallel.
    let records: Vec<(String, String)> = (fetched.iter())
        .flat_map(|f| f.repos.iter().map(|(name, r)| (name.clone(), r.head.clone())))
        .collect();
    let read_heads = reg.local(&records, "clones", |(name, _)| name.clone(), "reading where HEAD is", |(name, _)| {
        crate::commands::add::clone_head(install_root, root, name)
    })?;
    let mut clone_heads: BTreeMap<String, String> = BTreeMap::new();
    let mut heads: BTreeMap<String, String> = BTreeMap::new();
    for ((name, recorded), read) in records.into_iter().zip(read_heads) {
        if let Some(head) = &read {
            clone_heads.insert(name.clone(), head.clone());
        }
        heads.insert(name, read.unwrap_or(recorded));
    }

    // A flesh package takes the place of the base's flesh repos, every
    // component of them (compose drops them).
    let flesh_gone = crate::commands::add::flesh_replaced(base, added);
    let mut repos: Vec<BaseRepo> = Vec::new();
    for c in base.components().iter().filter(|c| !flesh_gone.contains(c.repo.as_str())) {
        let thorn = crate::sim::trim::is_arrangement_thorn(base, c);
        let flesh = crate::fetch::is_flesh(root, c);
        if !thorn && !flesh {
            continue;
        }
        let i = match repos.iter().position(|r| r.name == c.repo) {
            Some(i) => i,
            None => {
                // Where HEAD is in the clone, for a clone cactup fetched
                // from this URL.
                let on_disk = (fetched.as_ref())
                    .and_then(|f| f.repos.get(&c.repo))
                    .filter(|r| c.url.as_deref().is_some_and(|u| normalized(u) == normalized(&r.url)))
                    .and_then(|_| heads.get(&c.repo).cloned());
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
    // under two spellings is one pin, as Cactarium compares URLs).
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
    let info_of = |i: usize| pin_of[i].map(|i| &answers[i]);
    let mapped: Vec<Option<PkgId>> =
        (0..repos.len()).map(|i| info_of(i).and_then(package_of).filter(|id| !id.is_repo())).collect();

    // An `[[added]]` entry that only gives a base repo's package more thorns:
    // the same package, in the base's own clone, replacing nothing. Its
    // thorns live at whatever commit the base has the clone at (compose
    // writes them so), so it is matched by package and clone, never commit.
    // The same predicate compose writes them by (`add::base_clone_of`),
    // and the package is the one the lookup says the clone is. Only the
    // first base repo of a package is that package (a second is a repo of
    // its own), so only it is extended.
    let extends = |a: &AddedPackage, r: &BaseRepo, id: &PkgId| {
        a.package == id.as_str() && crate::commands::add::base_clone_of(a, base).is_some_and(|c| c.repo == r.name)
    };
    let mut first: BTreeSet<&PkgId> = BTreeSet::new();
    let extension_of: Vec<Option<usize>> = (repos.iter().zip(&mapped))
        .map(|(r, id)| {
            let id = id.as_ref().filter(|id| first.insert(id))?;
            added.iter().position(|a| extends(a, r, id))
        })
        .collect();

    // What to read: each added package at its commit (expecting its recorded
    // version, unless it is at a commit or tip), an extension excepted (the
    // base is read for it); each base repo the lookup says a version froze,
    // at its commit, expecting that version. A base repo at a commit no
    // version froze is not read: that would read the package now (a fetch
    // of its repository, rate-limited by the server), for every repo of a
    // base that follows branches.
    struct Want {
        id: PkgId,
        commit: String,
        version: Option<String>,
    }
    let mut wants: Vec<Want> = Vec::new();
    let mut want_of_added: Vec<Option<usize>> = Vec::new();
    for (i, a) in added.iter().enumerate() {
        let id = PkgId::parse(&a.package).with_context(|| format!("installation.toml adds {}", a.package))?;
        if extension_of.contains(&Some(i)) {
            want_of_added.push(None);
            continue;
        }
        let at_commit = crate::commands::add::is_commit(&a.version) || crate::commands::add::is_tip(&a.version);
        want_of_added.push(Some(wants.len()));
        wants.push(Want { id, commit: a.commit.clone(), version: (!at_commit).then(|| a.version.clone()) });
    }
    let mut want_of_repo: Vec<Option<usize>> = Vec::new();
    for (i, (r, id)) in repos.iter().zip(&mapped).enumerate() {
        let frozen = info_of(i).and_then(|info| info.version.clone());
        match (id, &r.commit, frozen) {
            (Some(id), Some(commit), Some(version)) if !options.no_base_reads => {
                want_of_repo.push(Some(wants.len()));
                wants.push(Want { id: id.clone(), commit: commit.clone(), version: Some(version) });
            }
            _ => want_of_repo.push(None),
        }
    }
    // Read up front, in batches: every base package at a commit no version
    // froze (its whole claims, so nothing about it is left unchecked), and
    // every added package at a commit or tip. Under `--no-deps` the base is
    // not read. What a batch does not read is read singly, as before: an
    // added package below, a base package when the plan depends on it.
    let mut batch: Vec<(PkgId, String)> = Vec::new();
    if !options.no_base_reads {
        // A base repo whose package an `[[added]]` entry is becomes a repo of
        // no package (below): nothing to read for it.
        let added_ids: BTreeSet<&PkgId> = want_of_added.iter().flatten().map(|w| &wants[*w].id).collect();
        let mut first_of: BTreeSet<&PkgId> = BTreeSet::new();
        for (i, (r, id)) in repos.iter().zip(&mapped).enumerate() {
            let (Some(id), Some(commit)) = (id, &r.commit) else { continue };
            let frozen = info_of(i).is_some_and(|info| info.version.is_some());
            if first_of.insert(id) && !frozen && !added_ids.contains(id) {
                batch.push((id.clone(), commit.clone()));
            }
        }
    }
    batch.extend(wants.iter().filter(|w| w.version.is_none()).map(|w| (w.id.clone(), w.commit.clone())));
    reg.prefetch(&batch)?;
    let fits = |r: &Release, w: &Want| {
        r.commit == w.commit && w.version.as_ref().is_none_or(|v| r.at.version().is_some_and(|at| &at.to_string() == v))
    };
    let mut read: Vec<Option<Release>> = vec![None; wants.len()];
    let mut refused: Vec<Option<Unanswered>> = vec![None; wants.len()];
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
            Read::Found(r) if fits(&r, &wants[i]) => read[i] = Some(*r),
            // Another version froze that commit too (another track): the
            // one recorded, or the one the lookup named, is in the whole list.
            Read::Found(_) => astray.push(wants[i].id.clone()),
            Read::Missing => {}
            Read::Refused(why) => refused[i] = Some(why),
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
    let why = |i: usize| match (&refused[i], &wants[i].version) {
        (Some(u), _) => format!("Cactarium did not read it there: {}", u.why),
        (None, Some(v)) => format!("Cactarium does not serve version {v} at commit {}", short(&wants[i].commit)),
        (None, None) => format!("Cactarium cannot read it at commit {}", short(&wants[i].commit)),
    };

    let mut out: Vec<Present> = Vec::new();
    let mut unread: Vec<Unread> = Vec::new();
    let mut unchecked: Vec<String> = Vec::new();
    let taken = |out: &[Present], id: &PkgId| out.iter().any(|p| &p.release.id == id);
    for (i, (r, id)) in repos.iter().zip(&mapped).enumerate() {
        let mut checkouts: Vec<(&str, &str)> =
            r.thorns.iter().map(|c| (c.checkout.as_str(), c.repo_path.as_deref().unwrap_or(""))).collect();
        let commit = r.commit.clone().unwrap_or_default();
        let url = r.url.clone().unwrap_or_default();
        // Whether a repo not read is the flesh: the lookup's word when it
        // says so (a registered flesh), else the tree's (its components are
        // the root's). A release read says for itself.
        let flesh = r.flesh || info_of(i).is_some_and(|info| info.flesh);
        let mut libraries = Vec::new();
        let mut stand_in = false;
        let release = match id {
            // Two base repos of one package: the second is a repo of its own.
            Some(id) if taken(&out, id) => unknown(&PkgId::repo(&r.name), &commit, &url, false, &checkouts, local),
            Some(id) => {
                if let Some(e) = extension_of[i] {
                    let a = &added[e];
                    checkouts.extend(a.thorns.iter().map(|t| (t.checkout.as_str(), t.repo_path.as_str())));
                    libraries = installed_libraries(a);
                }
                let batched = match want_of_repo[i] {
                    None => match reg.batched(id, &commit) {
                        Some(Read::Found(rel)) => Some(*rel),
                        _ => None,
                    },
                    Some(_) => None,
                };
                match want_of_repo[i].and_then(|w| read[w].clone()).or(batched) {
                    Some(rel) => with_local_facts(rel, &checkouts, local),
                    // Not read (no version froze its commit, `--no-deps`, or
                    // the read went unanswered): the resolver reads it when
                    // its plan depends on it. What the lookup says it forks
                    // is known already.
                    None => {
                        unchecked.push(format!("{id}#{}", short(&commit)));
                        stand_in = true;
                        let mut r = unknown(id, &commit, &url, flesh, &checkouts, local);
                        r.claims = info_of(i).map(|info| fork_claims(&info.fork_of)).unwrap_or_default();
                        r
                    }
                }
            }
            // An unmapped flesh has no thorns to clash with: nothing the
            // resolver can speak of.
            None if r.thorns.is_empty() => continue,
            None => unknown(&PkgId::repo(&r.name), &commit, &url, false, &checkouts, local),
        };
        let enabled = checkouts.iter().map(|(c, _)| thorn_of(c).to_owned()).collect();
        out.push(Present { release, enabled, origin: Origin::Base { repo: r.name.clone() }, libraries, stand_in });
    }
    // A base repo whose package an `[[added]]` entry also is (and does not
    // merely extend): the added one is that package, the repo's remaining
    // checkouts a pseudo-package of their own.
    for (i, a) in added.iter().enumerate() {
        let Some(w) = want_of_added[i] else { continue };
        let id = &wants[w].id;
        if let Some(p) = out.iter_mut().find(|p| &p.release.id == id && matches!(p.origin, Origin::Base { .. })) {
            let Origin::Base { repo } = &p.origin else { unreachable!() };
            p.release.id = PkgId::repo(repo);
            p.release.at = At::Unknown(p.release.commit.clone());
            p.release.claims.clear();
            // A repo of no package now: nothing to read for it.
            p.stand_in = false;
        }
        if taken(&out, id) {
            bail!("installation.toml adds {id} twice");
        }
        let checkouts: Vec<(&str, &str)> =
            a.thorns.iter().map(|t| (t.checkout.as_str(), t.repo_path.as_str())).collect();
        let release = match &read[w] {
            Some(r) => with_local_facts(r.clone(), &checkouts, local),
            None => {
                let label = crate::commands::add::describe(&a.package, &a.version);
                unread.push(Unread { id: id.clone(), label, why: why(w) });
                unknown(id, &a.commit, &a.url, false, &checkouts, local)
            }
        };
        let enabled = checkouts.iter().map(|(c, _)| thorn_of(c).to_owned()).collect();
        out.push(Present { release, enabled, origin: Origin::Added, libraries: installed_libraries(a), stand_in: false });
    }
    // A stand-in's thorns are every thorn its clone has, where the clone is
    // at the stand-in's commit: only what they depend on is unknown until it
    // is read.
    let scans: Vec<(usize, String)> = (out.iter().enumerate())
        .filter(|(_, p)| p.stand_in)
        .filter_map(|(i, p)| match &p.origin {
            Origin::Base { repo } if clone_heads.get(repo) == Some(&p.release.commit) => Some((i, repo.clone())),
            _ => None,
        })
        .collect();
    let scanned = reg.local(&scans, "clones", |(_, repo)| repo.clone(), "reading its thorns", |(_, repo)| {
        scan_thorns(&install_root.join(root).join("repos").join(repo))
    })?;
    for ((i, _), thorns) in scans.into_iter().zip(scanned) {
        let p = &mut out[i];
        let arrangement = p.release.thorns.first().map(|t| t.arrangement.clone()).unwrap_or_default();
        for t in thorns {
            let Some(name) = t.name else { continue };
            if p.release.thorn(&name).is_none() {
                let arrangement = t.arrangement.unwrap_or_else(|| arrangement.clone());
                let repo_path = t.dir.to_string_lossy().into_owned();
                p.release.thorns.push(ThornInfo { arrangement, thorn: name, repo_path, facts: t.facts });
            }
        }
    }
    Ok(PresentSet { present: out, unread, unchecked, heads })
}

/// Base packages that come back, each with the added package whose removal brings it back.
pub(crate) type Returning = Vec<(PkgId, Present)>;

/// The base packages that would come back when an added package of
/// `bringing` that replaced base repos goes (the targets, and the packages
/// that may be swept with them: never one the user named), each with that
/// package: every base repo it replaced, where the restore will put it (its
/// pin; else the tip of the branch [`crate::fetch::known_after_restore`]
/// names, the decision the restore makes, asked of the remote in one listing),
/// with the
/// thorns the base checks out from it, read as present packages are: one
/// lookup of them all, a version that froze the commit from the version
/// list, else one batch read of them all (single reads where it reads
/// nothing). The remotes are asked on the network fan-out, a line each, all
/// within one [`crate::fetch::git::REMOTE_ASK_TIMEOUT`]; Ctrl-C fails it. The ones that
/// cannot be read, said in words, each with the package whose going brings
/// it back.
pub(crate) fn returning_base(
    reg: &mut Registry,
    install_root: &Path,
    base: &Thornlist,
    added: &[AddedPackage],
    bringing: &BTreeSet<PkgId>,
) -> Res<(Returning, Vec<(PkgId, String)>)> {
    struct Back<'l> {
        replacer: PkgId,
        repo: String,
        url: String,
        pin: Option<String>,
        listed: Option<&'l str>,
        checkouts: Vec<(&'l str, &'l str)>,
    }
    let mut backs: Vec<Back> = Vec::new();
    for a in added.iter().filter(|a| !a.replaces.is_empty()) {
        let replacer = PkgId::parse(&a.package)?;
        if !bringing.contains(&replacer) {
            continue;
        }
        for repo in &a.replaces {
            let comps: Vec<&Component> = (base.components().iter())
                .filter(|c| &c.repo == repo && crate::sim::trim::is_arrangement_thorn(base, c))
                .collect();
            let Some(first) = comps.first() else { continue };
            let url = first.url.clone().unwrap_or_default();
            let pin = first.commit.clone().filter(|c| is_commit_id(c));
            let checkouts = comps.iter().map(|c| (c.checkout.as_str(), c.repo_path.as_deref().unwrap_or(""))).collect();
            backs.push(Back { replacer: replacer.clone(), repo: repo.clone(), url, pin, listed: first.branch.as_deref(), checkouts });
        }
    }
    // Where each that follows a branch comes back: its remote asked, all at once.
    let records = crate::fetch::FetchState::read(install_root)?;
    let repos_dir = install_root.join(base.root()).join("repos");
    let asks: Vec<&Back> = backs.iter().filter(|b| b.pin.is_none()).collect();
    let tips: Vec<Result<String, String>> = if asks.is_empty() {
        Vec::new()
    } else {
        let headline = Mutex::new(reg.headline(asks.len(), "repos"));
        let layout = reg.layout.clone();
        let window = crate::fetch::git::AskWindow::now();
        let answers = crate::fetch::git::ask_all(&asks, |b| -> Res<Result<String, String>> {
            let line = Line::over(headline.lock().expect("headline poisoned").add_child(b.repo.clone()), &b.repo, layout.clone());
            line.phase("asking for the branch tip".to_owned());
            let record = records.as_ref().and_then(|r| r.repos.get(&b.repo));
            // One listing either way: a branch's tip, or the remote's default and its tip.
            let asked = match crate::fetch::known_after_restore(&repos_dir.join(&b.repo), &b.url, b.listed, record) {
                crate::fetch::Known::Branch(branch) => crate::fetch::git::ask_remote_tip(&b.url, Some(&branch), window),
                crate::fetch::Known::RemoteDefault { .. } => crate::fetch::git::ask_remote_head(&b.url, window).map(|(_, id)| id),
            };
            let tip = match asked {
                Ok(id) => Ok(id.to_string()),
                Err(e) if gix::interrupt::is_triggered() => return Err(e),
                Err(e) => Err(format!("the branch tip is unknown ({e:#})")),
            };
            drop(line);
            headline.lock().expect("headline poisoned").inc();
            Ok(tip)
        });
        answers?.into_iter().collect::<Res<Vec<_>>>()?
    };
    // However little there was to ask, an interrupt is not a reading.
    if gix::interrupt::is_triggered() {
        bail!("Interrupted");
    }
    if backs.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    let mut tips = tips.into_iter();
    let commits: Vec<Result<String, String>> =
        backs.iter().map(|b| b.pin.clone().map_or_else(|| tips.next().expect("a tip per ask"), Ok)).collect();
    // Which package each is, in one lookup.
    let pins: Vec<PinQuery> = (backs.iter().zip(&commits))
        .filter_map(|(b, c)| c.as_ref().ok().map(|c| PinQuery { url: b.url.clone(), commit: c.clone() }))
        .collect();
    let infos = reg.lookup(&pins)?;
    let mut info_of = pins.iter().zip(infos).map(|(p, i)| ((p.url.clone(), p.commit.clone()), i)).collect::<BTreeMap<_, _>>();
    let (mut out, mut unread) = (Vec::new(), Vec::new());
    let mut wanted: Vec<(usize, PkgId, String, Option<String>)> = Vec::new();
    for (k, (b, commit)) in backs.iter().zip(&commits).enumerate() {
        let commit = match commit {
            Ok(c) => c.clone(),
            Err(why) => {
                unread.push((b.replacer.clone(), format!("base repo {}: {why}", b.repo)));
                continue;
            }
        };
        let info = info_of.remove(&(b.url.clone(), commit.clone()));
        match info.as_ref().and_then(|i| i.package.as_deref()).and_then(|p| PkgId::parse(p).ok()) {
            Some(id) => wanted.push((k, id, commit, info.and_then(|i| i.version))),
            None => unread.push((b.replacer.clone(), format!("base repo {}: Cactarium knows no package at {}", b.repo, short(&commit)))),
        }
    }
    // Frozen ones from their version lists, the rest in one batch read.
    let frozen: Vec<PkgId> = wanted.iter().filter(|w| w.3.is_some()).map(|w| w.1.clone()).collect();
    reg.load(&frozen)?;
    let unfrozen: Vec<(PkgId, String)> = wanted.iter().filter(|w| w.3.is_none()).map(|w| (w.1.clone(), w.2.clone())).collect();
    reg.prefetch(&unfrozen)?;
    for (k, id, commit, version) in wanted {
        let b = &backs[k];
        let release = match version {
            Some(v) => {
                let fits = |r: &&Release| r.commit == commit && r.at.version().is_some_and(|at| at.to_string() == v);
                reg.releases(&id).and_then(|rs| rs.iter().find(fits)).cloned()
            }
            None => reg.snapshot(&id, &commit)?.map(|s| s.release),
        };
        let Some(release) = release else {
            let why = reg.unanswered(&id, &commit).map_or_else(|| "Cactarium cannot read it there".to_owned(), |u| u.why);
            unread.push((b.replacer.clone(), format!("base repo {}: {id} at {}: {why}", b.repo, short(&commit))));
            continue;
        };
        let release = with_local_facts(release, &b.checkouts, None);
        let enabled = b.checkouts.iter().map(|(c, _)| thorn_of(c).to_owned()).collect();
        let origin = Origin::Base { repo: b.repo.clone() };
        out.push((b.replacer.clone(), Present { release, enabled, origin, libraries: Vec::new(), stand_in: false }));
    }
    Ok((out, unread))
}

/// The lookup's `fork-of` targets (`owner/name@<range>`) as fork claims.
fn fork_claims(targets: &[String]) -> Vec<cactup_ecosystem_common::api::ClaimInfo> {
    (targets.iter())
        .map(|t| {
            // `owner/name@<range>`, `owner/name#<commit>`, or a bare `owner/name`.
            let (target, range, sha) = match (t.split_once('@'), t.split_once('#')) {
                (Some((target, range)), _) => (target.to_owned(), Some(range.to_owned()), None),
                (None, Some((target, sha))) => (target.to_owned(), None, Some(sha.to_owned())),
                (None, None) => (t.clone(), None, None),
            };
            cactup_ecosystem_common::api::ClaimInfo {
                kind: "fork-of".to_owned(),
                target,
                range,
                sha,
                needs: Vec::new(),
                into: None,
                needed_by: Vec::new(),
                alternatives: Vec::new(),
                note: None,
            }
        })
        .collect()
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

/// Reads the present stand-ins `ids` at their commits, exactly (the read
/// limit waited out): each read release takes its stand-in's place, its
/// enabled thorns kept. The ones not read, with why: one read refused is no
/// failure of the command.
pub(crate) fn read_stand_ins(
    reg: &mut Registry,
    set: &mut PresentSet,
    ids: &[PkgId],
) -> Res<Vec<(PkgId, Unanswered)>> {
    let mut unread = Vec::new();
    for id in ids {
        let Some(p) = set.present.iter_mut().find(|p| &p.release.id == id && p.stand_in && !id.is_repo()) else {
            continue;
        };
        let commit = p.release.commit.clone();
        match reg.snapshot(id, &commit)? {
            Some(snapshot) => {
                let mut release = snapshot.release;
                for t in release.thorns.iter_mut() {
                    if let Some(local) = p.release.thorn(&t.thorn) {
                        t.facts = local.facts.clone();
                    }
                }
                p.release = release;
                p.stand_in = false;
                set.unchecked.retain(|u| !u.starts_with(&format!("{id}#")));
            }
            None => {
                let why = reg.unanswered(id, &commit).unwrap_or_else(|| missing_commit(id));
                unread.push((id.clone(), why));
            }
        }
    }
    Ok(unread)
}

/// The thorns of a clone, as the crate's `ccl::scan_tree` finds them (a
/// directory with an `interface.ccl` at the root, one level down, or two:
/// hidden directories and symlinks skipped, CCL files read only when they are
/// regular files of sane size), with their facts from the CCL alone. Unlike
/// `scan_tree`, it never reads the thorns' sources (thousands of files a
/// thorn, on a network filesystem): only the thorns' names and CCL facts are
/// wanted of a stand-in's tree.
fn scan_thorns(root: &Path) -> Vec<cactup_ecosystem_common::ccl::ScannedThorn> {
    use std::path::PathBuf;
    fn subdirs(dir: &Path) -> Vec<PathBuf> {
        let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
        let mut v: Vec<PathBuf> = rd
            .filter_map(Result::ok)
            .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .map(|e| PathBuf::from(e.file_name()))
            .collect();
        v.sort();
        v
    }
    let scan = |rel: &Path, out: &mut Vec<cactup_ecosystem_common::ccl::ScannedThorn>| {
        let dir = root.join(rel);
        let Some(interface) = crate::sim::trim::read_ccl(&dir.join("interface.ccl")) else { return };
        let configuration = crate::sim::trim::read_ccl(&dir.join("configuration.ccl"));
        let mut parts = rel.components().map(|c| c.as_os_str().to_string_lossy().into_owned());
        let (arrangement, name) = match (parts.next(), parts.next()) {
            (Some(a), Some(t)) => (Some(a), Some(t)),
            (Some(t), None) => (None, Some(t)),
            _ => (None, None),
        };
        let facts = ThornFacts::scan(&interface, configuration.as_deref());
        out.push(cactup_ecosystem_common::ccl::ScannedThorn { dir: rel.to_owned(), name, arrangement, facts });
    };
    let mut out = Vec::new();
    scan(Path::new(""), &mut out);
    for a in subdirs(root) {
        scan(&a, &mut out);
        for t in subdirs(&root.join(&a)) {
            scan(&a.join(&t), &mut out);
        }
    }
    out
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
            // The plan unshown: the command's last line says what to rebuild.
            LineKind::Header | LineKind::Step | LineKind::Rebuild if !show_all => {}
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
        bail!("No terminal to confirm a plan beyond the packages named.\nPass --yes, or --dry-run to see the plan.");
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
            let line = |u: &deps::Unmet| match u.ignored.as_slice() {
                [] => format!("\n  {} ({}) {}", u.thorn, u.package, u.requirement),
                ignored => format!(
                    "\n  {} ({}) {}; --ignore-claim {} dropped a dependency that may declare it",
                    u.thorn,
                    u.package,
                    u.requirement,
                    ignored.join(", ")
                ),
            };
            let lines: Vec<String> = unmet.iter().map(line).collect();
            let what = if unmet.iter().any(|u| !u.ignored.is_empty()) { "Requirements" } else { "Undeclared requirements" };
            anyhow!(
                "{what} not met by the installation:{}.\nCactarium at {} is too old to list providers.\n\
                 Add a provider first, or ask the package authors to declare the dependency",
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

    /// A stand-in's tree scan finds what the crate's `scan_tree` finds, with
    /// the same CCL facts, and never reads sources: a thorn whose sources
    /// include a header is scanned as one with none.
    #[test]
    fn a_tree_scan_reads_ccl_only() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let thorn = |rel: &str, iface: &str| {
            std::fs::create_dir_all(root.join(rel).join("src")).unwrap();
            std::fs::write(root.join(rel).join("interface.ccl"), iface).unwrap();
        };
        thorn("Arr/A", "IMPLEMENTS: a\nUSES INCLUDE HEADER: loop.hxx\n");
        std::fs::write(root.join("Arr/A/src/a.cc"), "#include \"loop.hxx\"\n").unwrap();
        thorn("B", "IMPLEMENTS: b\nINHERITS: a\n");
        std::fs::write(root.join("B/configuration.ccl"), "REQUIRES Loop\n").unwrap();
        thorn(".hidden/C", "IMPLEMENTS: c\n");
        // Latin-1 in both files: read as the crate reads them, not dropped.
        std::fs::create_dir_all(root.join("Arr/D")).unwrap();
        std::fs::write(root.join("Arr/D/interface.ccl"), b"# caf\xe9\nIMPLEMENTS: d\n").unwrap();
        std::fs::write(root.join("Arr/D/configuration.ccl"), b"# na\xefve\nREQUIRES THORNS: B\n").unwrap();
        let ours = scan_thorns(root);
        let crate_scan = cactup_ecosystem_common::ccl::scan_tree(root);
        let names = |v: &[cactup_ecosystem_common::ccl::ScannedThorn]| {
            v.iter().map(|t| (t.dir.clone(), t.name.clone(), t.arrangement.clone())).collect::<Vec<_>>()
        };
        assert_eq!(names(&ours), names(&crate_scan));
        for (a, b) in ours.iter().zip(&crate_scan) {
            let mut b = b.facts.clone();
            b.needs_includes.clear();
            assert_eq!(a.facts, b);
        }
        assert!(crate_scan[0].facts.needs_includes == ["loop.hxx"], "the crate reads sources");
        let d = ours.iter().find(|t| t.name.as_deref() == Some("D")).expect("a thorn with Latin-1 in its CCL");
        assert_eq!((d.facts.implements.as_deref(), d.facts.requires_thorns.as_slice()), (Some("d"), &["B".to_owned()][..]));
        assert!(ours.iter().all(|t| t.facts.needs_includes.is_empty()));
    }

    /// After a batch round that read something, the rest are asked again
    /// shortly; after one that read nothing, a little later.
    #[test]
    fn round_waits() {
        assert_eq!(round_wait(false), std::time::Duration::from_millis(1500));
        assert_eq!(round_wait(true), std::time::Duration::from_secs(3));
        assert_eq!(STALLS, 3);
    }

    /// One wait lasts what `Retry-After` says, at most two minutes; without
    /// it, until the window has passed; never past the budget left.
    #[test]
    fn a_wait_is_capped() {
        use std::time::Duration;
        let now = std::time::Instant::now();
        let all = Duration::from_secs(300);
        assert_eq!(wait_length(Some(7), None, now, all), Duration::from_secs(7));
        assert_eq!(wait_length(Some(0), None, now, all), Duration::from_secs(1));
        assert_eq!(wait_length(Some(100_000), None, now, all), MAX_WAIT);
        assert_eq!(wait_length(Some(100_000), None, now, Duration::from_secs(30)), Duration::from_secs(30));
        let opened = now - Duration::from_secs(45);
        assert_eq!(wait_length(None, Some(opened), now, all), READ_WINDOW - Duration::from_secs(45));
        assert_eq!(minutes(Duration::from_secs(80)), "1 min 20 s");
        assert_eq!(minutes(Duration::from_secs(300)), "5 min");
    }

    /// The lookup's `fork-of` targets, in each form a claim takes.
    #[test]
    fn lookup_fork_targets_read_as_fork_claims() {
        let claims = fork_claims(&["et/carpet@=2026.5.1".into(), "et/carpetx#0123456789ab".into(), "et/hydro".into()]);
        let read: Vec<(&str, Option<&str>, Option<&str>)> =
            claims.iter().map(|c| (c.target.as_str(), c.range.as_deref(), c.sha.as_deref())).collect();
        assert_eq!(
            read,
            [("et/carpet", Some("=2026.5.1"), None), ("et/carpetx", None, Some("0123456789ab")), ("et/hydro", None, None)]
        );
        assert!(claims.iter().all(|c| c.kind == "fork-of"));
    }
}
