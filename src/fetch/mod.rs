//! Native component fetcher (spec §3.2): planner/executor over the parsed
//! thornlist. git via gix, http(s)/ftp via reqwest, svn/cvs/hg/darcs via the
//! system tools.
//!
//! Implemented by the FETCH stream (design/_impl_fetch.md).

// Consumed by refetch/install in Phases 4/5; the allows come off then.
pub mod download;
pub mod external;
pub mod git;
pub mod library;
pub mod link;
pub(crate) mod retry;
pub(crate) mod watch;

use crate::format::{self, Format, Toml};
use crate::thornlist::{Component, ComponentType, Thornlist};
use crate::Res;
use anyhow::{bail, Context};
use git::{DirtyReason, RepoState};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The number of parallel fetch workers, mirroring GetComponents'
/// `process_components` pool.
const WORKERS: usize = 4;

/// How deep [`execute`]'s renderer draws into the progress tree. Levels:
///  1. the overall "fetch components" bar
///  2. one line per in-flight component, and no more: everything gix would
///     report below it (phase status, per-phase bars, per-thread
///     delta-resolution workers) is collapsed onto that one line by
///     [`crate::progress::Line`], which is also what keeps gix's throughput
///     chatter out of the scrollback
const PROGRESS_MAX_LEVEL: prodash::progress::key::Level = 2;

/// A pure, read-only classification of everything the fetch would do.
/// Produced by [`plan`]; nothing on disk changes until [`execute`].
#[derive(Debug)]
pub struct Plan {
    /// One entry per distinct git repo directory (repo-keyed — `cactusbase`
    /// alone backs ~15 thorns), fetchable now.
    pub git: Vec<GitRepoPlan>,
    /// Dirty repos, skipped. `refetch --overwrite-modified` backs their
    /// modified files up and moves them into `git` via [`Plan::force`];
    /// `refetch --overwrite <names>` does the same for a subset, via
    /// [`Plan::force_where`].
    pub skipped: Vec<SkippedRepo>,
    /// http/https/ftp components (plain downloads, always refreshed).
    pub downloads: Vec<Component>,
    /// svn/cvs/hg/darcs components (system tools).
    pub external: Vec<Component>,
    /// Components needing an arrangement symlink after fetching.
    pub links: Vec<Component>,
    /// Libraries placed inside another clone (`!CHECKOUT = .` sections whose
    /// `!TARGET` is inside `repos/`; see [`library`]), in the order they are
    /// placed: after every clone, a library inside another library after it.
    pub libraries: Vec<library::Placement>,
    /// The thornlist's `ROOT` define (typically `Cactus`).
    pub root: String,
}

#[derive(Debug)]
pub enum GitAction {
    /// Repo absent: depth-1 single-branch clone.
    Clone,
    /// Clean, HEAD already on the wanted branch: fetch + fast-forward.
    Update,
    /// Clean, wanted branch not yet checked out locally: fetch by explicit
    /// refspec, create the branch, check it out.
    Align,
    /// A `!REPO_COMMIT` pin on a clean repo: check the commit out detached,
    /// fetching it only if the repo lacks it. A repo already at its pin is
    /// left alone, offline — a pinned repo is never fast-forwarded.
    Pin,
}

#[derive(Debug)]
pub struct GitRepoPlan {
    pub repo: String,
    /// Absolute repo directory: `<install_root>/<root>/repos/<repo>`.
    pub dir: PathBuf,
    pub url: String,
    /// `None` = the remote's default branch (no `!REPO_BRANCH`). For a pinned
    /// repo, the branch or tag the pin was taken from: a label, and where to
    /// look for the commit when the server will not send it by id.
    pub branch: Option<String>,
    /// The `!REPO_COMMIT` pin: `Clone` and `Pin` check this commit out.
    pub commit: Option<gix::ObjectId>,
    pub action: GitAction,
    /// The checkout names (thorns) this repo backs, for reporting.
    pub checkouts: Vec<String>,
    /// `Some(description)` when a dirty repo was forced into the plan.
    pub forced: Option<String>,
    /// `Some(url)` when this repo's `origin` must be re-pointed at `url` before
    /// it is fetched — the forced heal for `DirtyReason::RemoteUrlChanged`.
    /// `align` fetches from whatever `origin` names, so without this a forced
    /// refetch of a re-pointed repo silently fetches the old upstream;
    /// persisting the URL (in `execute`, via `git::set_origin_url`) is also
    /// what stops the next probe from re-flagging it.
    pub retarget: Option<String>,
    /// What the plan could not settle as it should have, said on the repo's
    /// line: the remote's default branch not told, so the clone goes back to
    /// the label a pinned fetch recorded (which `align` refuses loudly when
    /// it is a tag).
    pub note: Option<String>,
}

#[derive(Debug)]
pub struct SkippedRepo {
    pub repo: String,
    pub dir: PathBuf,
    pub url: String,
    pub branch: Option<String>,
    /// The `!REPO_COMMIT` pin, if any: forcing this repo pins it.
    pub commit: Option<gix::ObjectId>,
    pub reason: DirtyReason,
    pub untracked: Vec<String>,
    pub checkouts: Vec<String>,
    /// The repo's last fetch record, for where a forced, detached clone goes.
    pub record: Option<RepoRecord>,
}

impl Plan {
    /// Move every skipped repo into the fetchable set. See [`Plan::force_where`].
    pub fn force(&mut self) -> Res<Vec<String>> {
        self.force_where(|_| true)
    }

    /// Move the skipped repos `select` accepts into the fetchable set (after
    /// the caller has backed up their modified files), leaving the rest
    /// skipped. Returns the names moved, in plan order.
    ///
    /// A forced repo with no pin and no `!REPO_BRANCH`, its HEAD on no branch,
    /// goes where [`branch_to_follow`] says, which may ask its remote: those
    /// questions are asked on the network fan-out, a line each, all within one
    /// [`git::REMOTE_ASK_TIMEOUT`], before anything moves; Ctrl-C fails the
    /// call.
    pub fn force_where(&mut self, select: impl Fn(&SkippedRepo) -> bool) -> Res<Vec<String>> {
        let mut moved = Vec::new();
        // `drain` with a filter would leave the remaining `skipped` reordered
        // (retain-while-draining semantics); take the whole vec instead and
        // rebuild both parts in their original relative order.
        let taken = std::mem::take(&mut self.skipped);
        let (forced, kept): (Vec<SkippedRepo>, Vec<SkippedRepo>) = taken.into_iter().partition(|s| select(s));
        self.skipped = kept;
        // A skipped repo with no !REPO_BRANCH has `branch: None`; unlike
        // `plan()`'s clean-repo path, nothing has resolved it to the repo's
        // actual head branch yet. Do that here too: the branch HEAD is on,
        // else where a detached clone goes back to. A pinned repo needs no
        // branch: it is checked out by commit.
        let on_branch = |s: &SkippedRepo| git::head_of(&s.dir).ok().map(|(b, _)| b).filter(|b| b != git::DETACHED);
        let wants: Vec<(usize, Option<String>)> = (forced.iter().enumerate())
            .filter(|(_, s)| s.commit.is_none() && s.branch.is_none())
            .map(|(i, s)| (i, on_branch(s)))
            .collect();
        let asks: Vec<ToFollow> = (wants.iter())
            .filter(|(_, b)| b.is_none())
            .map(|(i, _)| &forced[*i])
            .map(|s| ToFollow { repo: &s.repo, dir: &s.dir, url: &s.url, listed: None, record: s.record.as_ref() })
            .collect();
        let follows = if asks.is_empty() {
            Vec::new()
        } else {
            let (progress, renderer) = crate::manifest::setup_prodash();
            let follows = follow_all(progress.add_child(FOLLOWING), &asks);
            renderer.shutdown_and_wait();
            follows?
        };
        let mut follows = follows.into_iter();
        let mut resolved: BTreeMap<usize, Follow> = BTreeMap::new();
        for (i, branch) in wants {
            let follow = match branch {
                Some(b) => Follow { branch: Some(b), note: None },
                None => follows.next().expect("one answer per ask"),
            };
            resolved.insert(i, follow);
        }
        for (i, s) in forced.into_iter().enumerate() {
            // A `RemoteUrlChanged` repo's `origin` still names the *old*
            // upstream until `execute` calls `set_origin_url`; against that,
            // `at_local_origin_tip`'s "already at the tip" reading is
            // meaningless (the local `refs/remotes/origin/<branch>` it
            // consults points at the old remote too), and `Update` would
            // fetch from the wrong remote entirely. Always Align, and always
            // carry the URL to retarget to before this item is ever fetched.
            let retarget =
                matches!(&s.reason, DirtyReason::RemoteUrlChanged { .. }).then(|| s.url.clone());
            let action = if s.commit.is_some() {
                GitAction::Pin
            } else if retarget.is_some() {
                GitAction::Align
            } else {
                match &s.branch {
                    Some(b) if git::at_local_origin_tip(&s.dir, b) => GitAction::Update,
                    _ => GitAction::Align,
                }
            };
            // Resolved above, for one with neither a pin nor a listed branch.
            let (branch, note) = match resolved.remove(&i) {
                Some(Follow { branch, note }) => (branch, note),
                None => (s.branch, None),
            };
            moved.push(s.repo.clone());
            self.git.push(GitRepoPlan {
                repo: s.repo,
                dir: s.dir,
                url: s.url,
                branch,
                commit: s.commit,
                action,
                checkouts: s.checkouts,
                forced: Some(s.reason.describe()),
                retarget,
                note,
            });
        }
        Ok(moved)
    }
}

/// Where a clone goes back to, and what to say about it on its line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Follow {
    pub branch: Option<String>,
    pub note: Option<String>,
}

/// Where a clone goes back to, as far as cactup can tell without asking its
/// remote.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Known {
    /// This branch.
    Branch(String),
    /// The remote's default branch, which only the remote can tell; `label`
    /// is what to fall back to when it cannot be asked (a pinned fetch's).
    RemoteDefault { label: Option<String> },
}

/// The branch a clone that is no longer pinned goes back onto: the list's
/// (`!REPO_BRANCH`), else the one the last fetch followed (unpinned, from the
/// same URL), else the remote's default (as the clone knows it, when its
/// `origin` is that remote; else as the remote says: a clone made pinned
/// knows none), else, last, the label a pinned fetch from that URL recorded.
/// A pinned fetch's label is only where its pin came from (a tag, as often
/// as not): a branch to align to only when nothing else names one, and then
/// `align` says plainly if it is none. A label that was another real branch
/// (`develop`) is not followed once the list stops pinning: the remote's
/// default is, as for a fresh clone. This is the part decided offline;
/// [`branch_to_follow`] asks the remote when it must. Used wherever cactup
/// decides this, so they all agree: the fetch plan, a forced repo, and the
/// base a removal brings back.
pub fn known_branch(dir: &Path, url: &str, listed: Option<&str>, record: Option<&RepoRecord>) -> Known {
    let same = |u: &str| git::normalize_url(u) == git::normalize_url(url);
    if let Some(b) = listed {
        return Known::Branch(b.to_owned());
    }
    let record = record.filter(|r| same(&r.url));
    if let Some(b) = record.filter(|r| r.commit.is_none()).and_then(|r| r.branch.clone()) {
        return Known::Branch(b);
    }
    // The clone's own refs speak for its `origin`, which is the list's
    // remote only when the URLs agree.
    if git::origin_url(dir).is_some_and(|u| same(&u))
        && let Some(b) = git::default_branch(dir)
    {
        return Known::Branch(b);
    }
    Known::RemoteDefault { label: record.and_then(|r| r.branch.clone()) }
}

/// [`known_branch`], the remote asked when only it can tell (by `until`).
/// Unanswered (offline, timed out), the clone goes back to the label and the
/// note says why. Ctrl-C fails it ("Interrupted"), never a fallback.
pub fn branch_to_follow(
    dir: &Path,
    url: &str,
    listed: Option<&str>,
    record: Option<&RepoRecord>,
    until: std::time::Instant,
) -> Res<Follow> {
    match known_branch(dir, url, listed, record) {
        Known::Branch(b) => Ok(Follow { branch: Some(b), note: None }),
        Known::RemoteDefault { label } => match git::ask_remote_default_branch(url, until) {
            Ok(b) => Ok(Follow { branch: Some(b), note: None }),
            Err(e) if gix::interrupt::is_triggered() => Err(e),
            Err(e) => Ok(unanswered(label, &e)),
        },
    }
}

/// Where a clone goes when its remote could not say its default branch.
fn unanswered(label: Option<String>, e: &anyhow::Error) -> Follow {
    let then = match &label {
        Some(l) => format!("going back to {l}, the label of its last, pinned fetch"),
        None => "and nothing else names a branch".to_owned(),
    };
    Follow { branch: label, note: Some(format!("its remote's default branch is unknown ({e:#}); {then}")) }
}

/// Where a fetch of the list puts the clone of `url` at `dir` back on a
/// branch after cactup had it elsewhere (a package that replaced a base
/// repo, removed), decided as [`plan`] and [`Plan::force_where`] decide it,
/// so a removal reads the base where its restore will put it: the list's
/// branch; else, no clone on disk, the remote's default (a clone takes it);
/// else a clone on a branch of that remote keeps it (a clean one is
/// fast-forwarded there); else, detached, [`known_branch`]. Offline: the
/// caller asks the remote what it must (the default branch and its tip are
/// one listing). Upstream may move between the read and the restore: that
/// race is accepted, the restore takes the branch's tip then.
pub fn known_after_restore(dir: &Path, url: &str, listed: Option<&str>, record: Option<&RepoRecord>) -> Known {
    if let Some(b) = listed {
        return Known::Branch(b.to_owned());
    }
    if !dir.exists() {
        return Known::RemoteDefault { label: None };
    }
    let same = git::origin_url(dir).is_some_and(|u| git::normalize_url(&u) == git::normalize_url(url));
    if same && let Some(b) = git::head_of(dir).ok().map(|(b, _)| b).filter(|b| b != git::DETACHED) {
        return Known::Branch(b);
    }
    known_branch(dir, url, None, record)
}

/// The headline over the remote asks of a phase.
const FOLLOWING: &str = "asking remotes for their default branches";

/// One clone whose branch may take asking its remote.
struct ToFollow<'a> {
    repo: &'a str,
    dir: &'a Path,
    url: &'a str,
    listed: Option<&'a str>,
    record: Option<&'a RepoRecord>,
}

/// [`branch_to_follow`] for each of `items`, in order, under `parent` (a
/// headline over a line each), the remotes asked on the network fan-out
/// ([`git::ask_all`]), all within one [`git::REMOTE_ASK_TIMEOUT`]. Ctrl-C
/// fails the whole call.
fn follow_all(item: prodash::tree::Item, items: &[ToFollow]) -> Res<Vec<Follow>> {
    let layout = crate::progress::Layout::for_names(items.iter().map(|t| t.repo).chain([FOLLOWING]));
    let headline = std::sync::Mutex::new(crate::progress::Headline::over(
        item,
        FOLLOWING,
        items.len(),
        "repos",
        layout.clone(),
    ));
    let started = std::time::Instant::now();
    let answers = git::ask_all(items, |t| {
        let line = crate::progress::Line::over(
            headline.lock().expect("follow headline poisoned").add_child(t.repo.to_owned()),
            t.repo,
            layout.clone(),
        );
        line.phase("asking for its default branch".to_owned());
        let follow = branch_to_follow(t.dir, t.url, t.listed, t.record, started + follow_timeout(t.url));
        drop(line);
        headline.lock().expect("follow headline poisoned").inc();
        follow
    });
    answers?.into_iter().collect()
}

/// [`git::REMOTE_ASK_TIMEOUT`], or a test's for its stalling `git://` server.
fn follow_timeout(url: &str) -> std::time::Duration {
    #[cfg(test)]
    {
        let ms = TEST_FOLLOW_TIMEOUT_MS.load(std::sync::atomic::Ordering::Relaxed);
        if ms > 0 && url.starts_with("git://") {
            return std::time::Duration::from_millis(ms);
        }
    }
    let _ = url;
    git::REMOTE_ASK_TIMEOUT
}

/// A test's remote-ask time for `git://` URLs (0: the real one; no other
/// test asks one). Process-wide: the asks run on the fan-out's threads.
#[cfg(test)]
pub(crate) static TEST_FOLLOW_TIMEOUT_MS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// What [`execute`] did, per item. Failures are collected, not fatal — the
/// caller decides the exit code (§3.2 step 10) and reports each one.
#[derive(Debug, Default)]
pub struct ExecReport {
    pub repos: Vec<RepoResult>,
    /// Per-checkout symlink outcomes, in thornlist order.
    pub links: Vec<(String, link::LinkOutcome)>,
    /// Files written by http/https downloads.
    pub downloads: Vec<PathBuf>,
    /// Each download's [`download_key`] and SHA-256.
    pub download_sums: Vec<(String, String)>,
    pub failures: Vec<Failure>,
}

#[derive(Debug)]
pub struct RepoResult {
    pub repo: String,
    pub url: String,
    pub branch: Option<String>,
    /// The commit HEAD resolved to after the action (hex).
    pub head: String,
    /// The pin this repo was fetched to, if any (see [`RepoRecord::commit`]).
    pub commit: Option<gix::ObjectId>,
    /// Whether the action moved HEAD (false = already up to date).
    pub changed: bool,
    /// Carried through from the plan: why this repo needed forcing, if it did.
    pub forced: Option<String>,
    /// `Some(url)` when this fetch re-pointed the repo's `origin` at the
    /// thornlist URL — a persistent change to the user's repo, so it is
    /// reported unconditionally, not only under `--verbose`.
    pub retargeted: Option<String>,
    /// What went wrong after the fetch itself succeeded (its branch's
    /// upstream not written), said on the repo's line.
    pub warning: Option<String>,
}

#[derive(Debug)]
pub struct Failure {
    /// The repo or component the failure belongs to.
    pub what: String,
    pub error: String,
}

/// What a fetch leaves in `fetch-state.toml`: a record per repo, and the
/// hash of each download by [`download_key`].
pub struct FetchRecords {
    pub repos: Vec<(String, RepoRecord)>,
    pub downloads: Vec<(String, String)>,
}

/// A download's key in the fetch record: its path under the installation's
/// root, as the thornlist places it (`<target>/<file name>`).
pub fn download_key(install_root: &Path, path: &Path) -> String {
    path.strip_prefix(install_root).unwrap_or(path).to_string_lossy().into_owned()
}

/// Per repo of `list` that is one download (an http/https/ftp checkout that
/// is the only one of its repo, so one file), its hash as the fetch record
/// at `install_root` keeps it: what a build freezes and `sim export` pins.
/// No record, or one that does not read, is no hashes; one a newer cactup
/// wrote is an error, never silently no hashes (which would freeze a build,
/// or export a simulation, without the pins it does have).
pub fn download_hashes(install_root: &Path, list: &Thornlist) -> Res<BTreeMap<String, String>> {
    let state = match FetchState::read(install_root) {
        Ok(Some(state)) => state,
        Err(e) if e.downcast_ref::<crate::format::Newer>().is_some() => return Err(e),
        Ok(None) | Err(_) => return Ok(BTreeMap::new()),
    };
    let mut out = BTreeMap::new();
    for c in list.components() {
        let single = list.components().iter().filter(|o| o.repo == c.repo).count() == 1;
        if !single || !matches!(c.ty, ComponentType::Http | ComponentType::Https | ComponentType::Ftp) {
            continue;
        }
        // Built as `download_component` built the path it recorded, so a
        // target spelled `$ROOT/dl/` or `./dl` finds its record too.
        let path = install_root.join(&c.target).join(download::derive_filename(c.name.as_deref(), &c.checkout));
        if let Some(sum) = state.downloads.get(&download_key(install_root, &path)) {
            out.insert(c.repo.clone(), sum.clone());
        }
    }
    Ok(out)
}

impl ExecReport {
    /// The `fetch-state.toml` records for everything successfully fetched.
    pub fn records(&self) -> FetchRecords {
        FetchRecords { repos: self.repo_records(), downloads: self.download_sums.clone() }
    }

    fn repo_records(&self) -> Vec<(String, RepoRecord)> {
        self.repos
            .iter()
            .map(|r| {
                (
                    r.repo.clone(),
                    RepoRecord {
                        url: r.url.clone(),
                        branch: r.branch.clone(),
                        head: r.head.clone(),
                        commit: r.commit.map(|c| c.to_string()),
                    },
                )
            })
            .collect()
    }
}

/// The line a finished git item leaves in the scrollback: green when the
/// fetch left everything else alone, yellow when it did not — overwriting a
/// dirty repo and re-pointing an `origin` are persistent changes to the
/// user's tree, and a color is how they get noticed in a run of eighty
/// repos.
///
/// A repo that was already at the wanted commit leaves *nothing*: in a
/// typical refetch that is most of the tree, and eighty lines of "already up
/// to date" would bury the handful that moved. The caller's summary reports
/// how many were already current; the scrollback is for what happened.
fn announce_repo(line: &crate::progress::Line, item: &GitRepoPlan, head: gix::ObjectId, changed: bool, warning: Option<&str>) {
    let head = head.to_string();
    let short = &head[..12.min(head.len())];
    let what = match (&item.action, item.branch.as_deref()) {
        (GitAction::Clone, _) if item.commit.is_some() => format!("cloned, pinned at {short}"),
        (GitAction::Pin, _) if changed => format!("pinned at {short}"),
        (GitAction::Clone, Some(branch)) => format!("cloned {branch} at {short}"),
        (GitAction::Clone, None) => format!("cloned at {short}"),
        (_, Some(branch)) if changed => format!("{branch} now at {short}"),
        (_, None) if changed => format!("now at {short}"),
        // Unchanged, but something else about the repo was: say what, since
        // the fetch itself has nothing to report.
        _ if item.forced.is_some() || item.retarget.is_some() || item.note.is_some() || warning.is_some() => {
            format!("already at {short}")
        }
        _ => return,
    };
    let what = match &item.note {
        Some(note) => format!("{what} ({note})"),
        None => what,
    };
    let what = match warning {
        Some(warning) => format!("{what}; {warning}"),
        None => what,
    };
    match (&item.forced, &item.retarget) {
        (Some(reason), _) => line.warned(format!("{what}; overwrote {reason}")),
        (None, Some(url)) => line.warned(format!("{what}; origin re-pointed at {url}")),
        (None, None) if item.note.is_some() || warning.is_some() => line.warned(what),
        (None, None) => line.succeeded(what),
    }
}

/// Run the plan: git repos and downloads on a [`WORKERS`]-wide pool keyed by
/// repo (one repo is only ever touched by one worker), externals
/// sequentially, then the symlink pass.
///
/// Progress renders via the crate's prodash line renderer, two levels deep
/// (see [`PROGRESS_MAX_LEVEL`]): an overall "fetch components" bar counts
/// finished items, and each in-flight component gets exactly one line —
/// named for the component, labeled with the phase running right now, and
/// filled by that phase's own counter, so the same bar carries a repo from
/// negotiating through receiving and indexing to checking out. Every
/// component then leaves exactly one line in the scrollback on its way out:
/// green for a clean fetch, yellow when it overwrote something, red when it
/// failed. Errors are collected per item, never fatal to the rest of the
/// fetch; the red line is what makes a failure visible live, next to the
/// work going on around it, rather than only in the final report.
pub fn execute(plan: &Plan, install_root: &Path) -> Res<ExecReport> {
    enum Work<'p> {
        Git(&'p GitRepoPlan),
        Download(&'p Component),
    }

    let (progress, renderer) = crate::manifest::setup_prodash_with(
        Some(crate::manifest::progress_level_filter(PROGRESS_MAX_LEVEL)),
        true,
    );

    // One name column for the whole batch, sized to the longest name it will
    // carry (the headline included, so its numbers line up with the lines
    // below it): every component's phase then starts at the same column
    // instead of shifting about as the phases change — and the headline
    // holds the bar column still for every line under it.
    const HEADLINE: &str = "fetch components";
    let layout = crate::progress::Layout::for_names(
        std::iter::once(HEADLINE)
            .chain(plan.git.iter().map(|item| item.repo.as_str()))
            .chain(plan.downloads.iter().map(|c| c.checkout.as_str())),
    );

    let top = crate::progress::Headline::over(
        progress.add_child(HEADLINE),
        HEADLINE,
        plan.git.len() + plan.downloads.len(),
        "components",
        layout.clone(),
    );
    let top = std::sync::Mutex::new(top);

    let queue: std::sync::Mutex<std::collections::VecDeque<Work>> = std::sync::Mutex::new(
        plan.git
            .iter()
            .map(Work::Git)
            .chain(plan.downloads.iter().map(Work::Download))
            .collect(),
    );
    let report = std::sync::Mutex::new(ExecReport::default());

    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(plan.git.len() + plan.downloads.len()).max(1) {
            scope.spawn(|| loop {
                if gix::interrupt::is_triggered() {
                    return;
                }
                let work = match queue.lock().expect("fetch queue poisoned").pop_front() {
                    Some(work) => work,
                    None => return,
                };
                match work {
                    Work::Git(item) => {
                        // The line is named for the repo alone and labeled
                        // with what is happening to it right now — gix takes
                        // that label over as the fetch moves through its
                        // phases. Keeping the two apart is what lets the
                        // history line below read as "cactusbase  cloned at
                        // a1b2c3d…" instead of repeating the action twice.
                        let mut line = crate::progress::Line::over(
                            top.lock().expect("fetch progress poisoned").add_child(item.repo.clone()),
                            &item.repo,
                            layout.clone(),
                        );
                        line.phase(if item.retarget.is_some() {
                            // Distinct phase: this item's `origin` is about
                            // to be rewritten before anything is fetched, not
                            // a normal clone/update/switch.
                            "repointing".to_string()
                        } else {
                            match (&item.action, item.branch.as_deref()) {
                                (GitAction::Clone, _) => "cloning".to_string(),
                                (GitAction::Pin, _) => "pinning".to_string(),
                                (GitAction::Update, _) => "updating".to_string(),
                                (GitAction::Align, Some(branch)) => format!("switching to {branch}"),
                                (GitAction::Align, None) => "switching".to_string(),
                            }
                        });

                        // Rewrite `origin` before anything else touches the
                        // repo: `align` (below) re-opens it and must see the
                        // thornlist's URL, not the stale one the probe
                        // flagged. A failure here is reported and dropped
                        // exactly like an align/clone failure — no fetch is
                        // attempted for an item whose remote we could not fix.
                        if let Some(url) = &item.retarget
                            && let Err(e) = git::set_origin_url(&item.dir, url)
                        {
                            line.failed(format!("{e:#}"));
                            report.lock().expect("fetch report poisoned").failures.push(Failure {
                                what: item.repo.clone(),
                                error: format!("{e:#}"),
                            });
                            drop(line);
                            top.lock().expect("fetch progress poisoned").inc();
                            continue;
                        }
                        let before = match item.action {
                            GitAction::Clone => None,
                            _ => git::head_of(&item.dir).ok().map(|(_, id)| id),
                        };
                        // Forced (backed-up local edits) or repointed: only
                        // a full overwrite is right there.
                        let force = item.forced.is_some() || item.retarget.is_some();
                        let outcome = match (&item.action, item.branch.as_deref(), item.commit) {
                            (GitAction::Clone, branch, Some(commit)) => {
                                git::clone_pinned(&item.url, branch, commit, &item.dir, &mut line)
                            }
                            (GitAction::Pin, branch, Some(commit)) => {
                                git::pin(&item.dir, branch, commit, force, &mut line)
                            }
                            (GitAction::Pin, _, None) => {
                                Err(anyhow::anyhow!("No commit to pin to (a bug in the fetch plan)"))
                            }
                            (GitAction::Clone, branch, None) => {
                                git::clone(&item.url, branch, &item.dir, &mut line)
                                    .and_then(|()| git::head_of(&item.dir).map(|(_, id)| id))
                            }
                            (_, Some(branch), _) => git::align(&item.dir, branch, force, &mut line),
                            (_, None, _) => {
                                // Reached only when neither the list, the
                                // clone nor the remote names a branch.
                                Err(anyhow::anyhow!(
                                    "No branch to put {} on: the thornlist names none (!REPO_BRANCH), and neither the \
                                     clone nor its remote says which is the default; name one with !REPO_BRANCH",
                                    item.repo
                                ))
                            }
                        };
                        // Assert the repo actually landed on its expected
                        // branch: an independent read of HEAD on disk, taken
                        // here (outside the report lock) so it runs in
                        // parallel with the other workers.
                        // A pinned repo has no branch to be on: it must sit at
                        // its commit instead.
                        let wrong_branch = match (&outcome, &item.branch, item.commit) {
                            (Ok(_), _, Some(pin)) => match git::head_of(&item.dir) {
                                Ok((_, actual)) if actual != pin => {
                                    Some(format!("post-fetch HEAD is at {actual}, expected the pin {pin}"))
                                }
                                _ => None,
                            },
                            (Ok(_), Some(expected), None) => match git::head_of(&item.dir) {
                                Ok((actual, _)) if &actual != expected => {
                                    Some(format!("post-fetch HEAD is on {actual}, expected {expected}"))
                                }
                                _ => None,
                            },
                            _ => None,
                        };
                        // A branch align made gets its upstream, as a clone's does. Writing
                        // it failing leaves the checkout as good as it is: said on the
                        // line, never a failed fetch (its record is written all the same).
                        let warning = match (&outcome, &item.action, item.branch.as_deref(), item.commit) {
                            (Ok(_), GitAction::Align, Some(branch), None) => git::set_upstream_if_absent(&item.dir, branch)
                                .err()
                                .map(|e| format!("its branch's upstream could not be set ({e:#})")),
                            _ => None,
                        };
                        let mut report = report.lock().expect("fetch report poisoned");
                        match outcome {
                            Ok(head) => {
                                let changed = before != Some(head);
                                announce_repo(&line, item, head, changed, warning.as_deref());
                                report.repos.push(RepoResult {
                                    repo: item.repo.clone(),
                                    url: item.url.clone(),
                                    branch: item.branch.clone(),
                                    head: head.to_string(),
                                    commit: item.commit,
                                    changed,
                                    forced: item.forced.clone(),
                                    retargeted: item.retarget.clone(),
                                    warning,
                                });
                                if let Some(error) = wrong_branch {
                                    report.failures.push(Failure { what: item.repo.clone(), error });
                                }
                            }
                            Err(e) => {
                                // Said with what the plan could not settle, which is
                                // likely why (a pinned fetch's tag taken for a branch).
                                let error = match &item.note {
                                    Some(note) => format!("{e:#} ({note})"),
                                    None => format!("{e:#}"),
                                };
                                line.failed(error.clone());
                                report.failures.push(Failure { what: item.repo.clone(), error });
                            }
                        }
                        drop(report);
                        drop(line);
                        top.lock().expect("fetch progress poisoned").inc();
                    }
                    Work::Download(c) => {
                        // `counting`, not `over`: a download has no phases
                        // of its own to report, it just fills the bar with
                        // the bytes it has taken.
                        let mut line = crate::progress::Line::counting(
                            top.lock()
                                .expect("fetch progress poisoned")
                                .add_child(c.checkout.clone()),
                            &c.checkout,
                            layout.clone(),
                        );
                        line.phase("downloading");
                        let outcome = download::download_component(install_root, c, &mut line);
                        let mut report = report.lock().expect("fetch report poisoned");
                        match outcome {
                            // Kept as it was: nothing happened, so the line leaves nothing.
                            Ok(download::Downloaded { path, fetched: false, sha256 }) => {
                                report.download_sums.push((download_key(install_root, &path), sha256));
                                report.downloads.push(path)
                            }
                            Ok(download::Downloaded { path, fetched: true, sha256 }) => {
                                report.download_sums.push((download_key(install_root, &path), sha256));
                                let size = crate::progress::bytes(prodash::Count::step(&line));
                                let name = path.file_name().unwrap_or(path.as_os_str()).to_string_lossy();
                                // The line is already named for the
                                // checkout; name the file only when the two
                                // differ (a `!NAME`, or a `.`-checkout).
                                line.succeeded(if *name == *c.checkout {
                                    format!("downloaded {size}")
                                } else {
                                    format!("downloaded {size} as {name}")
                                });
                                report.downloads.push(path)
                            }
                            Err(e) => {
                                line.failed(format!("{e:#}"));
                                report.failures.push(Failure {
                                    what: c.checkout.clone(),
                                    error: format!("{e:#}"),
                                });
                            }
                        }
                        drop(report);
                        drop(line);
                        top.lock().expect("fetch progress poisoned").inc();
                    }
                }
            });
        }
    });
    // The pool is joined: every clone and checkout of this fetch is over.
    // Placing a library takes this, so it cannot be done from inside the
    // pool, while the clone it goes into may still be being written.
    let clones_done = library::ClonesDone::after_the_pool();

    // External tools (svn/cvs) talk to the terminal directly, so the
    // renderer must be gone before they run — shut it down here rather than
    // at the end of the function.
    drop(top);
    renderer.shutdown_and_wait();

    let mut report = report.into_inner().expect("fetch report poisoned");

    // Interrupted (§ Ctrl-C): the workers drained out with items still
    // queued. Those were never fetched — record each as a failure so the
    // caller's report and exit code see them (in-flight items already failed
    // with gix's own abort error), and skip the externals and the symlink
    // pass: nothing further should run, and returning a success-shaped
    // report here is what once let an interrupted fetch masquerade as
    // complete.
    if gix::interrupt::is_triggered() {
        for work in queue.into_inner().expect("fetch queue poisoned") {
            let what = match work {
                Work::Git(item) => item.repo.clone(),
                Work::Download(c) => c.checkout.clone(),
            };
            report
                .failures
                .push(Failure { what, error: "Interrupted before this component was fetched".into() });
        }
        return Ok(report);
    }

    // Externals sequentially: they spawn system tools that may talk to the
    // terminal, and none occur in the real ET list anyway.
    for c in &plan.external {
        if gix::interrupt::is_triggered() {
            report
                .failures
                .push(Failure { what: c.checkout.clone(), error: "Interrupted".into() });
            continue;
        }
        if let Err(e) = external::fetch_external(install_root, c) {
            report.failures.push(Failure { what: c.checkout.clone(), error: format!("{e:#}") });
        }
    }

    // Symlink pass, after every fetch: repoint-or-create per checkout.
    // Plain targets first: a target containing `..` may deliberately route
    // *through* an arrangement symlink another component creates (the ET
    // list's Fuka section does exactly this), so those resolve last.
    let mut links: Vec<&Component> = plan.links.iter().collect();
    links.sort_by_key(|c| c.target.contains("..") || c.checkout.contains(".."));
    let link_span = crate::timing::span("link pass");
    let pass = link::LinkPass::new(install_root, &plan.root);
    for (i, c) in links.iter().enumerate() {
        // Ctrl-C: the links not yet made are failures, like the components
        // never fetched above, and nothing further runs (no settle pass).
        if gix::interrupt::is_triggered() {
            for c in &links[i..] {
                let error = "Interrupted before this link was made".to_owned();
                report.failures.push(Failure { what: c.checkout.clone(), error });
            }
            return Ok(report);
        }
        match pass.link(c) {
            Ok(outcome) => report.links.push((c.checkout.clone(), outcome)),
            Err(e) => {
                report.failures.push(Failure { what: c.checkout.clone(), error: format!("{e:#}") })
            }
        }
    }
    drop(link_span);

    // Libraries, after every clone above has finished (the pool is joined)
    // and in the plan's order: the one write each makes is a link inside the
    // clone it goes into, so nothing writes inside a dependent while it is
    // being cloned or checked out, and a library inside another library comes
    // after it. One whose dependent (or own clone, or the library it goes
    // inside) failed is not attempted, and says why.
    let failed: std::collections::BTreeSet<String> = report.failures.iter().map(|f| f.what.clone()).collect();
    let mut unplaced = std::collections::BTreeSet::new();
    for (i, p) in plan.libraries.iter().enumerate() {
        if gix::interrupt::is_triggered() {
            for p in &plan.libraries[i..] {
                let error = "Interrupted before this library was placed".to_owned();
                report.failures.push(Failure { what: p.label(), error });
            }
            return Ok(report);
        }
        match library::place(&clones_done, install_root, &plan.root, p, &failed, &unplaced) {
            Ok(outcome) => report.links.push((p.label(), outcome)),
            Err(e) => {
                unplaced.insert(p.repo.clone());
                report.failures.push(Failure { what: p.label(), error: format!("{e:#}") });
            }
        }
    }

    // Last, after all git work: settle the index of every repo that was
    // cloned or aligned (or found already at its tip), so later status walks
    // match by stat instead of re-hashing files written in the index's own
    // second. Best effort and optional for correctness, so its outcome —
    // even an interrupt — never changes the report.
    let repos: Vec<(String, std::path::PathBuf)> = report
        .repos
        .iter()
        .map(|r| (r.repo.clone(), install_root.join(&plan.root).join("repos").join(&r.repo)))
        .collect();
    settle_indexes(&repos);

    Ok(report)
}

/// [`git::settle_index`] over `repos` (name, directory), in parallel, behind a
/// phase-scoped bar.
fn settle_indexes(repos: &[(String, std::path::PathBuf)]) {
    let _span = crate::timing::span("settle indexes");
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let settling = progress.add_child("settle indexes");
    settling.init(Some(repos.len()), Some(prodash::unit::label("repos")));
    let settling = std::sync::Mutex::new(settling);
    let _ = crate::par::parallel_map(repos, |(repo, dir)| {
        let current = settling.lock().expect("settle progress poisoned").add_child(repo.clone());
        if let Ok(git::Settled::Rewritten) = git::settle_index(dir) {
            crate::timing::count("settle: indexes rewritten", 1);
        }
        drop(current);
        settling.lock().expect("settle progress poisoned").inc();
    });
    drop(settling);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
}

/// `<install_root>/.cactup/fetch-state.toml` — the per-repo record of what
/// cactup itself fetched (URL, branch, resolved HEAD). Prune only ever
/// removes repos this file (or the current thornlist) accounts for, and it
/// is the input a future rebuild-decision hookup will diff (§7.5).
#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FetchState {
    #[serde(default)]
    pub repos: indexmap::IndexMap<String, RepoRecord>,
    /// Each download's SHA-256 as last fetched, by [`download_key`]. An
    /// additive key: older binaries ignore it.
    #[serde(default, skip_serializing_if = "indexmap::IndexMap::is_empty")]
    pub downloads: indexmap::IndexMap<String, String>,
}

#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RepoRecord {
    pub url: String,
    pub branch: Option<String>,
    /// The commit HEAD resolved to when cactup last fetched this repo.
    pub head: String,
    /// The `!REPO_COMMIT` that fetch pinned the repo to, if any. A pinned
    /// repo sits detached at it, so `branch` is only the label it was taken
    /// from — not a branch HEAD is expected to be on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

/// `fetch-state.toml`'s format. Written by login binaries only (a fetch
/// never runs on a compute node), so it is not `frozen_read`. Every deployed
/// binary requires the `schema` key, which the helper always writes, first.
/// `record` edits the file in place: a repo it did not fetch keeps its
/// record whole, unknown keys included; a repo it did fetch gets a record
/// made anew, which it names to `write_replacing`, so a key a newer binary
/// put on the old record does not outlive the commit it described. The
/// downloads table is a map of hashes: nothing to replace whole.
pub static FETCH_STATE: Format<Toml> = Format {
    name: "fetch state",
    key: "schema",
    current: 1,
    upgrades: &[],
    downgrades: &[],
    down_floor: 1,
    frozen_read: false,
    legacy: None,
    replace_whole: &[],
};

impl FetchState {
    fn path(install_root: &Path) -> PathBuf {
        install_root.join(".cactup").join("fetch-state.toml")
    }

    /// `Ok(None)` when no fetch has ever recorded state (pre-existing
    /// installations) — callers must treat that as "no record", never as
    /// "nothing was fetched by cactup".
    pub fn read(install_root: &Path) -> Res<Option<FetchState>> {
        format::read(&Self::path(install_root), &FETCH_STATE)
    }

    /// Merge `records` over the existing state (repos not touched by this
    /// fetch keep their previous record) and write atomically.
    pub fn record(install_root: &Path, records: &FetchRecords) -> Res<()> {
        let mut state = Self::read(install_root)?.unwrap_or_default();
        for (key, sum) in &records.downloads {
            state.downloads.insert(key.clone(), sum.clone());
        }
        for (repo, record) in &records.repos {
            state.repos.insert(
                repo.clone(),
                RepoRecord {
                    url: record.url.clone(),
                    branch: record.branch.clone(),
                    head: record.head.clone(),
                    commit: record.commit.clone(),
                },
            );
        }
        // The records made anew above: put-back must not reach into them,
        // each named by its key (a repo name may hold a dot).
        let whole: Vec<Vec<String>> =
            records.repos.iter().map(|(repo, _)| vec!["repos".to_owned(), repo.clone()]).collect();
        format::write_replacing_keys(&Self::path(install_root), &state, &FETCH_STATE, &whole)
    }
}

/// The live state of the source trees a thornlist draws from, plus which repo
/// supplies the Cactus flesh. This is the input that lets
/// `build::rebuild_decision` (§7.4/§7.8) notice that sources moved under a
/// config: without it, a refetch that fast-forwards 81 repos — or a thorn
/// edited by hand — leaves every config reading `UpToDate` and silently never
/// gets compiled.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SourceHeads {
    /// repo name → [`git::source_state`] (HEAD commit + worktree summary).
    /// Only repos this thornlist names appear, so a config built from a narrow
    /// list is not invalidated by a repo it does not compile.
    pub heads: BTreeMap<String, String>,
    /// Repos whose worktree has local modifications right now.
    pub dirty: std::collections::BTreeSet<String>,
    /// Repos whose directory is on disk but could not be probed — most often
    /// because it is no longer a git repo at all (someone dropped a
    /// hand-built variant of a thorn tree in place of the checkout). These
    /// are deliberately NOT absent-and-forgotten: a repo that lost its git
    /// identity cannot be shown to still be the commit a config was built
    /// from, so `build::source_delta` must read it as divergence rather than
    /// skip it for want of a state string to compare.
    pub unreadable: std::collections::BTreeSet<String>,
    /// Git repos the thornlist names whose directory is not on disk at all —
    /// divergence for the same reason as `unreadable`, once a config's
    /// recorded baseline knows the repo.
    pub missing: std::collections::BTreeSet<String>,
    /// The repo supplying the Cactus flesh, when this list has one.
    pub flesh: Option<String>,
}

/// The commit half of a [`git::source_state`] string, i.e. ignoring local
/// edits. Distinguishing the two matters: a *moved* flesh commit earns a
/// realclean, while hand-editing flesh sources does not — `make` recompiles
/// what an edit affects, and forcing a from-scratch rebuild on someone
/// iterating on flesh code would be hostile.
pub(crate) fn committed(state: &str) -> &str {
    state.split('+').next().unwrap_or(state)
}

/// Probe every repo this thornlist names — in parallel, because each probe
/// is a full gix status walk with the same "looks hung without progress"
/// duration [`plan`]'s probe loop pays. `progress` is init'ed to the repo
/// count, shows each in-flight repo as a child, and counts probes as they
/// finish. `Ok(Err(why))` only when the tree holds no readable repo at all —
/// callers must read that as "no information", never as "nothing changed",
/// and should show `why`: a config whose sources cactup cannot read gets no
/// baseline and no edit detection, and the user cannot fix what nobody names.
/// `Err` only on interrupt (§2.4).
///
/// A repo that cannot be probed is never guessed at, but it is not dropped on
/// the floor either: it lands in `unreadable` (directory present, not a
/// readable git repo) or `missing` (git component, no directory), both of
/// which `build::source_delta` reads as divergence. Silently omitting them is
/// what let a repo whose `.git` was replaced by a hand-built variant report
/// as "matches what this config was built from".
pub fn source_heads(
    install_root: &Path,
    list: &Thornlist,
    progress: &mut prodash::tree::Item,
) -> Res<Result<SourceHeads, NoSources>> {
    let repos_dir = install_root.join(list.root()).join("repos");
    let mut out = SourceHeads::default();
    let mut seen = std::collections::BTreeSet::new();
    let mut repos: Vec<String> = Vec::new();
    for c in list.components() {
        // Only a list resolved by `thornlist::parse_config_list` carries
        // these (a CRL parse drops them): a plain-list thorn with no repo
        // behind it, which has no source state to take.
        if c.ty == ComponentType::Ignore {
            continue;
        }
        if out.flesh.is_none() && is_flesh(list.root(), c) {
            out.flesh = Some(c.repo.clone());
        }
        if !seen.insert(c.repo.clone()) {
            continue;
        }
        if repos_dir.join(&c.repo).is_dir() {
            repos.push(c.repo.clone());
        } else if c.ty == ComponentType::Git {
            // Only git components own a `repos/<repo>` directory at all —
            // downloads and external checkouts land straight under their
            // `!TARGET`, so their derived repo name is absent by design and
            // must not read as a vanished source.
            out.missing.insert(c.repo.clone());
        }
    }

    progress.init(Some(repos.len()), Some(prodash::unit::label("repos")));
    let progress = std::sync::Mutex::new(progress);
    let states = crate::par::parallel_map(&repos, |repo| {
        let current = progress.lock().expect("source_heads progress poisoned").add_child(repo.clone());
        let state = git::source_state(&repos_dir.join(repo)).map_err(|e| format!("{e:#}"));
        drop(current);
        progress.lock().expect("source_heads progress poisoned").inc();
        state
    })?;
    // The first probe failure, kept for `NoSources::NoneReadable`: when every
    // repo fails, the reason is almost always the same one, and it is the
    // only clue to what is wrong with the tree.
    let mut first_error: Option<(String, String)> = None;
    for (repo, state) in repos.into_iter().zip(states) {
        let state = match state {
            Ok(state) => state,
            Err(e) => {
                first_error.get_or_insert_with(|| (repo.clone(), e));
                out.unreadable.insert(repo);
                continue;
            }
        };
        if state.contains("+") {
            out.dirty.insert(repo.clone());
        }
        out.heads.insert(repo, state);
    }
    // Not one readable repo ⇒ no information about this tree, exactly as
    // before. Reporting an all-`missing`/all-`unreadable` reading as `Some`
    // would be worse than useless: `build` records `heads` as the config's
    // baseline, so an empty map would be stored and every later comparison
    // would then find nothing to compare and read as "unchanged" forever.
    if out.heads.is_empty() {
        return Ok(Err(match first_error {
            Some((repo, error)) => NoSources::NoneReadable { repos_dir, count: out.unreadable.len(), repo, error },
            None if out.missing.is_empty() && list.is_plain() => NoSources::NoLinkedThorns { repos_dir },
            None if out.missing.is_empty() => NoSources::NoGitRepos,
            None => NoSources::NoneOnDisk { repos_dir, count: out.missing.len(), plain: list.is_plain() },
        }));
    }
    Ok(Ok(out))
}

/// Why a [`source_heads`] probe produced no reading — said out loud, because
/// a config cactup cannot read the sources of gets no baseline and therefore
/// no edit or refetch detection, and nothing else would tell the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoSources {
    /// The config's processed thornlist is neither a CRL list nor a plain
    /// Cactus `ThornList` (see `thornlist::parse_config_list`, whose reason
    /// this carries verbatim).
    Unparseable(String),
    /// A CRL list with no git component: nothing to take a HEAD of.
    NoGitRepos,
    /// A plain `ThornList` none of whose thorns (nor the flesh) is a link
    /// into `repos/`: a tree cactup did not fetch, e.g. thorns cloned
    /// straight into `arrangements/`.
    NoLinkedThorns { repos_dir: PathBuf },
    /// Not one of the list's git repos is on disk: for a CRL list, where its
    /// `!DEFINE ROOT` puts them; for a plain list, where its links point.
    NoneOnDisk { repos_dir: PathBuf, count: usize, plain: bool },
    /// The repos are there, but none opened as a git repo.
    NoneReadable { repos_dir: PathBuf, count: usize, repo: String, error: String },
}

impl std::fmt::Display for NoSources {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NoSources::Unparseable(why) => write!(f, "{why}"),
            NoSources::NoGitRepos => write!(f, "its thornlist names no git repos"),
            NoSources::NoLinkedThorns { repos_dir } => write!(
                f,
                "none of its thorns is a link into '{}'",
                repos_dir.display()
            ),
            NoSources::NoneOnDisk { repos_dir, count, plain } => {
                write!(f, "none of the {count} git repo(s) its thornlist names is in '{}'", repos_dir.display())?;
                if !repos_dir.is_dir() {
                    write!(f, " (that directory does not exist")?;
                    if !plain {
                        // The likeliest cause by far: a custom CRL list whose
                        // `!DEFINE ROOT` is not this installation's source root.
                        write!(f, "; does the thornlist's `!DEFINE ROOT` name this installation's Cactus directory?")?;
                    }
                    write!(f, ")")?;
                }
                Ok(())
            }
            NoSources::NoneReadable { repos_dir, count, repo, error } => write!(
                f,
                "none of the {count} repo(s) in '{}' could be read as a git repo ({repo}: {error})",
                repos_dir.display()
            ),
        }
    }
}

/// [`source_heads_with_progress`] off a config's processed thornlist *text*
/// (CRL or plain, see `thornlist::parse_config_list`), folding a parse
/// failure into [`NoSources::Unparseable`] so every caller reports it the
/// same way. `Err` only on interrupt (§2.4).
pub fn probe_sources(
    install_root: &Path,
    cactus_root: &Path,
    processed_thornlist: &str,
) -> Res<Result<SourceHeads, NoSources>> {
    match crate::thornlist::parse_config_list(processed_thornlist, install_root, cactus_root)? {
        Ok(list) => source_heads_with_progress(install_root, &list),
        Err(why) => Ok(Err(NoSources::Unparseable(why))),
    }
}

/// [`source_heads`] behind its own phase-scoped line renderer, for callers
/// that carry no renderer of their own (the submit/run divergence warning,
/// `config delta`, the build's rebuild decision). The renderer's 500ms
/// initial delay means a tree that probes quickly never flashes a bar, and
/// on a non-tty stderr (job logs, pipes) there is no renderer at all.
pub fn source_heads_with_progress(
    install_root: &Path,
    list: &Thornlist,
) -> Res<Result<SourceHeads, NoSources>> {
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let mut probing = progress.add_child("probe sources");
    let result = source_heads(install_root, list, &mut probing);
    drop(probing);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    result
}

/// The flesh is the repo that checks the Cactus make system straight into the
/// Cactus root: `Makefile`, `lib`, `src`. Identifying it by *what it provides*
/// rather than by the Einstein Toolkit's `!NAME = flesh` keeps this working
/// for a list that names it something else. `manifest` and `simfactory2` also
/// target the root, but check out `./manifest`, `./simfactory` and `par`, so
/// neither matches — which matters, because a flesh change costs a full
/// realclean rebuild and those two do not affect compilation at all.
pub(crate) fn is_flesh(root: &str, c: &Component) -> bool {
    c.target.trim_end_matches('/') == root.trim_end_matches('/')
        && matches!(c.checkout.as_str(), "Makefile" | "lib" | "src")
}

/// Classify every component of `list` against the tree at `install_root`
/// (the directory that contains `<root>/`, i.e. the installation root).
/// Read-only; the network is never touched. `progress` is init'ed to the
/// repo count once the group map is built, then incremented as each repo's
/// probe — a plain gix status walk — finishes. The probes run on the
/// [`crate::par`] pool: even so, on ~80 repos they take long enough that
/// without progress the command looks hung before anything appears.
/// How a repo's source reads in a conflict message: its branch, and its pin.
fn source_label(branch: Option<&str>, commit: Option<&str>) -> String {
    let branch = branch.unwrap_or("<default>");
    match commit {
        Some(commit) => format!("{branch} pinned at {commit}"),
        None => branch.to_owned(),
    }
}

pub fn plan(list: &Thornlist, install_root: &Path, progress: &mut prodash::tree::Item) -> Res<Plan> {
    plan_only(list, install_root, progress, None)
}

/// [`plan`], with the clones probed and planned limited to `only` (when
/// given): a clone outside it is never looked at, nor its remote asked. The
/// list is still checked whole (sources, placements).
pub fn plan_only(
    list: &Thornlist,
    install_root: &Path,
    progress: &mut prodash::tree::Item,
    only: Option<&BTreeSet<&str>>,
) -> Res<Plan> {
    let root = list.root().to_owned();
    let repos_dir = install_root.join(&root).join("repos");

    // Group git components by repo directory; the repo, not the thorn, is
    // the unit of fetching and of dirtiness.
    struct Group<'c> {
        url: String,
        branch: Option<String>,
        commit: Option<String>,
        checkouts: Vec<String>,
        first: &'c Component,
    }
    let mut git_groups: BTreeMap<String, Group> = BTreeMap::new();
    let mut downloads = Vec::new();
    let mut external = Vec::new();
    let mut links = Vec::new();
    let mut placements: Vec<(&Component, String, String)> = Vec::new();

    for c in list.components() {
        match c.ty {
            ComponentType::Git => {
                let url = c
                    .url
                    .clone()
                    .with_context(|| format!("git component {} has no !URL", c.checkout))?;
                // Two names that differ only in case are one directory on a
                // case-insensitive filesystem: one clone, claimed twice.
                if let Some(other) =
                    git_groups.keys().find(|k| **k != c.repo && k.eq_ignore_ascii_case(&c.repo))
                {
                    bail!(
                        "Thornlist names repos {other} and {}, which differ only in case: on a case-insensitive \
                         filesystem they are one directory",
                        c.repo
                    );
                }
                match git_groups.get_mut(&c.repo) {
                    None => {
                        git_groups.insert(
                            c.repo.clone(),
                            Group {
                                url,
                                branch: c.branch.clone(),
                                commit: c.commit.clone(),
                                checkouts: vec![c.checkout.clone()],
                                first: c,
                            },
                        );
                    }
                    Some(group) => {
                        // Two sections naming one repo dir must agree, or the
                        // fetch would be order-dependent.
                        if group.url != url || group.branch != c.branch || group.commit != c.commit {
                            bail!(
                                "Thornlist names repo {} twice with conflicting \
                                 sources: {} @ {} (for {}) vs {} @ {} (for {})",
                                c.repo,
                                group.url,
                                source_label(group.branch.as_deref(), group.commit.as_deref()),
                                group.first.checkout,
                                url,
                                source_label(c.branch.as_deref(), c.commit.as_deref()),
                                c.checkout,
                            );
                        }
                        group.checkouts.push(c.checkout.clone());
                    }
                }
                match library::placement_of(&root, c) {
                    Some((dependent, into)) => placements.push((c, dependent, into)),
                    None => links.push(c.clone()),
                }
            }
            ComponentType::Http | ComponentType::Https | ComponentType::Ftp => {
                downloads.push(c.clone());
            }
            // external.rs fetches all four of these directly into
            // <target>/<name-or-checkout> (its documented simplification of
            // the Perl's hg/darcs mirror+symlink), so none of them get an
            // arrangement symlink here.
            ComponentType::Svn | ComponentType::Cvs | ComponentType::Hg | ComponentType::Darcs => {
                external.push(c.clone());
            }
            // `parse` drops them; only `thornlist::parse_config_list` makes
            // any, and its lists never reach the fetch planner.
            ComponentType::Ignore => unreachable!("ignore components are dropped by the parser"),
        }
    }

    // Each library goes inside a clone this list checks out, at a plain path
    // under it; the dependent's source comes along, to check the clone on
    // disk against before anything is placed inside it.
    let mut libraries = Vec::new();
    for (c, dependent, into) in placements {
        library::check_into(&into).with_context(|| format!("The thornlist places {} inside {dependent}", c.repo))?;
        let Some(host) = git_groups.get(&dependent) else {
            bail!(
                "The thornlist places {} inside repos/{dependent}, which it does not check out; a library goes only \
                 inside a clone the same list fetches",
                c.repo
            );
        };
        libraries.push(library::Placement {
            repo: c.repo.clone(),
            url: c.url.clone().unwrap_or_default(),
            dependent,
            dependent_url: host.url.clone(),
            dependent_commit: host.commit.clone(),
            into,
        });
    }
    let libraries = library::order(libraries)?;
    if let Some(only) = only {
        git_groups.retain(|repo, _| only.contains(repo.as_str()));
    }

    let mut plan = Plan {
        git: Vec::new(),
        skipped: Vec::new(),
        downloads,
        external,
        links,
        libraries,
        root,
    };

    // The last-fetch record: a repo sitting on the branch cactup itself put
    // it on (e.g. after `refetch --release A` then `--release B`) is not a
    // user branch switch and must not demand a force flag.
    let fetch_state = FetchState::read(install_root)?;

    progress.init(Some(git_groups.len()), Some(prodash::unit::label("repos")));

    // Probe in parallel (each probe is an independent read-only walk of its
    // own repo), then classify sequentially in the stable BTreeMap order so
    // the plan comes out deterministic.
    let groups: Vec<(String, Group)> = git_groups.into_iter().collect();
    let progress = std::sync::Mutex::new(progress);
    let probes = crate::par::parallel_map(&groups, |(repo, group)| {
        let current = progress.lock().expect("plan progress poisoned").add_child(repo.clone());
        let dir = repos_dir.join(repo);
        // A pinned repo is checked out by commit, so the branch it happens to
        // be on is no reason to skip it — only edits and local commits are.
        // Probing against "no branch" checks exactly those.
        let wanted_branch = if group.commit.is_some() { "" } else { group.branch.as_deref().unwrap_or("") };
        let mut probe = git::probe(&dir, &group.url, wanted_branch);
        if let RepoState::Dirty(DirtyReason::BranchSwitched { head, .. }) = &probe.state
            && fetch_state
                .as_ref()
                .and_then(|s| s.repos.get(repo))
                .is_some_and(|r| r.branch.as_deref() == Some(head))
        {
            // cactup's own doing. Re-probe against the current branch to
            // check the repo is otherwise clean; if so, plan the align.
            let reprobe = git::probe(&dir, &group.url, head);
            if let RepoState::Clean { .. } = reprobe.state {
                probe = git::Probe {
                    state: RepoState::Clean { head_branch: head.clone() },
                    untracked: reprobe.untracked,
                };
            }
        }
        drop(current);
        progress.lock().expect("plan progress poisoned").inc();
        probe
    })?;
    let progress = progress.into_inner().expect("plan progress poisoned");

    // Detached where cactup's last fetch left it, and no longer pinned: where
    // it goes back to may take asking its remote. Asked after the probes, on
    // the network fan-out (`git::REMOTE_ASKS_AT_ONCE` at once), all within
    // one `git::REMOTE_ASK_TIMEOUT` (one not started by then is not asked,
    // and falls back as unanswered), each with a line.
    let records = |repo: &str| fetch_state.as_ref().and_then(|s| s.repos.get(repo));
    let to_follow: Vec<(usize, PathBuf)> = (groups.iter().zip(&probes).enumerate())
        .filter(|(_, ((repo, group), probe))| {
            group.commit.is_none()
                && matches!(&probe.state, RepoState::Detached { head } if records(repo).is_some_and(|r| r.head == head.to_string()))
        })
        .map(|(i, ((repo, _), _))| (i, repos_dir.join(repo)))
        .collect();
    let asks: Vec<ToFollow> = (to_follow.iter())
        .map(|(i, dir)| {
            let (repo, group) = &groups[*i];
            ToFollow { repo, dir, url: &group.url, listed: group.branch.as_deref(), record: records(repo) }
        })
        .collect();
    let follows = if asks.is_empty() { Vec::new() } else { follow_all(progress.add_child(FOLLOWING), &asks)? };
    drop(asks);
    let mut follows: BTreeMap<usize, Follow> = to_follow.into_iter().map(|(i, _)| i).zip(follows).collect();

    for (i, ((repo, group), probe)) in groups.into_iter().zip(probes).enumerate() {
        let follow = follows.remove(&i);
        let dir = repos_dir.join(&repo);
        let commit = match &group.commit {
            Some(hex) => Some(
                gix::ObjectId::from_hex(hex.as_bytes())
                    .with_context(|| format!("Thornlist pins repo {repo} to an invalid commit {hex}"))?,
            ),
            None => None,
        };
        let record = fetch_state.as_ref().and_then(|s| s.repos.get(&repo));
        match probe.state {
            RepoState::Absent => plan.git.push(GitRepoPlan {
                repo,
                dir,
                url: group.url,
                branch: group.branch,
                commit,
                action: GitAction::Clone,
                checkouts: group.checkouts,
                forced: None,
                retarget: None,
                note: None,
            }),
            RepoState::Clean { .. } if commit.is_some() => plan.git.push(GitRepoPlan {
                repo,
                dir,
                url: group.url,
                branch: group.branch,
                commit,
                action: GitAction::Pin,
                checkouts: group.checkouts,
                forced: None,
                retarget: None,
                note: None,
            }),
            RepoState::Clean { head_branch } => {
                // No !REPO_BRANCH = follow whatever branch the clone is on.
                let wanted = group.branch.clone().unwrap_or_else(|| head_branch.clone());
                let action =
                    if head_branch == wanted { GitAction::Update } else { GitAction::Align };
                plan.git.push(GitRepoPlan {
                    repo,
                    dir,
                    url: group.url,
                    branch: Some(wanted),
                    commit: None,
                    action,
                    checkouts: group.checkouts,
                    forced: None,
                    retarget: None,
                    note: None,
                });
            }
            RepoState::Detached { head } => {
                // Detached where cactup's last fetch left it: an earlier pin
                // (the fetch record holds the commit it checked out). That is
                // cactup's state to move — to a new pin, or back onto a
                // branch when the list no longer pins the repo. Detached
                // anywhere else is the user's doing, unless it is already the
                // pin itself.
                let ours = record.is_some_and(|r| r.head == head.to_string());
                // One no branch can be found for stays where it is, skipped as
                // detached, as it was before cactup could ask its remote: a
                // forced fetch says why it cannot move it.
                let action = match (commit, follow) {
                    (Some(pin), _) if pin == head || ours => Some((GitAction::Pin, group.branch.clone(), None)),
                    (None, Some(Follow { branch: Some(branch), note })) => Some((GitAction::Align, Some(branch), note)),
                    _ => None,
                };
                match action {
                    Some((action, branch, note)) => plan.git.push(GitRepoPlan {
                        repo,
                        dir,
                        url: group.url,
                        branch,
                        commit,
                        action,
                        checkouts: group.checkouts,
                        forced: None,
                        retarget: None,
                        note,
                    }),
                    None => plan.skipped.push(SkippedRepo {
                        repo,
                        dir,
                        url: group.url,
                        branch: group.branch,
                        commit,
                        reason: DirtyReason::DetachedHead(head),
                        untracked: probe.untracked,
                        checkouts: group.checkouts,
                        record: record.cloned(),
                    }),
                }
            }
            RepoState::Dirty(reason) => {
                // BranchSwitched against "" happens only when no !REPO_BRANCH
                // is set; that cannot occur (probe compares against the head
                // branch then), but guard the invariant anyway.
                plan.skipped.push(SkippedRepo {
                    repo,
                    dir,
                    url: group.url,
                    branch: group.branch,
                    commit,
                    reason,
                    untracked: probe.untracked,
                    checkouts: group.checkouts,
                    record: record.cloned(),
                });
            }
        }
    }

    Ok(plan)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A miniature list in the shape the real Einstein Toolkit one uses: the
    /// flesh and simfactory2 both target `$ROOT`, one thorn arrangement does
    /// not. `!NAME` is deliberately *not* `flesh` for the flesh section, to
    /// prove detection keys off what the section provides.
    const LIST: &str = "\
!CRL_VERSION = 1.0
!DEFINE ROOT = Cactus
!DEFINE BRANCH = main

!TARGET   = $ROOT
!TYPE     = git
!URL      = https://example.invalid/cactus.git
!NAME     = core
!CHECKOUT = doc lib Makefile src

!TARGET   = $ROOT
!TYPE     = git
!URL      = https://example.invalid/simfactory2.git
!CHECKOUT = ./simfactory

!TARGET   = $ROOT/arrangements
!TYPE     = git
!URL      = https://example.invalid/cactusbase.git
!CHECKOUT = CactusBase/Boundary CactusBase/IOUtil
";

    #[test]
    fn source_heads_probes_the_live_tree_and_finds_the_flesh() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repos = root.join("Cactus/repos");
        for name in ["core", "cactusbase", "simfactory2", "unrelated"] {
            git::testrepo::init(&repos.join(name));
            git::testrepo::commit(&repos.join(name), "initial");
        }
        let list = crate::thornlist::parse(LIST).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("test probe");
        let got = source_heads(root, &list, &mut progress).unwrap().unwrap();

        // The flesh is `core` here: it is the section checking Makefile/lib/src
        // straight into the Cactus root. simfactory2 also targets the root but
        // checks out `./simfactory`, so it must not be mistaken for it.
        assert_eq!(got.flesh.as_deref(), Some("core"));
        // Exactly the repos the thornlist names — a repo on disk that this
        // list does not build from is not this config's source.
        assert_eq!(
            got.heads.keys().cloned().collect::<Vec<_>>(),
            vec!["cactusbase".to_string(), "core".to_string(), "simfactory2".to_string()]
        );
        assert!(got.dirty.is_empty(), "a fresh repo has no local modifications");

        // A new commit moves the recorded state — this is what a refetch, or a
        // manual checkout, looks like from the build's point of view.
        let before = got.heads["core"].clone();
        git::testrepo::commit(&repos.join("core"), "second");
        let after = source_heads(root, &list, &mut progress).unwrap().unwrap();
        assert_ne!(after.heads["core"], before);
        assert_eq!(after.heads["cactusbase"], got.heads["cactusbase"], "untouched repos hold still");
    }

    /// The git → non-git transition (and outright removal): a repo the
    /// thornlist names that can no longer be read is recorded, not dropped.
    /// Dropping it is what let `build::source_delta` report a checkout
    /// replaced by a hand-built variant as "unchanged".
    #[test]
    fn source_heads_records_repos_it_cannot_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let repos = root.join("Cactus/repos");
        for name in ["core", "cactusbase", "simfactory2"] {
            git::testrepo::init(&repos.join(name));
            git::testrepo::commit(&repos.join(name), "initial");
        }
        let list = crate::thornlist::parse(LIST).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("test probe");

        // Stand a hand-built variant in for the checkout: same directory, same
        // files, no `.git/`.
        std::fs::remove_dir_all(repos.join("cactusbase/.git")).unwrap();
        let got = source_heads(root, &list, &mut progress).unwrap().unwrap();
        assert!(!got.heads.contains_key("cactusbase"), "no state can be invented for it");
        assert_eq!(got.unreadable.iter().cloned().collect::<Vec<_>>(), vec!["cactusbase".to_string()]);
        assert!(got.missing.is_empty());

        // Removed outright: `missing` rather than `unreadable`, so the report
        // can say which of the two happened.
        std::fs::remove_dir_all(repos.join("cactusbase")).unwrap();
        let got = source_heads(root, &list, &mut progress).unwrap().unwrap();
        assert!(got.unreadable.is_empty());
        assert_eq!(got.missing.iter().cloned().collect::<Vec<_>>(), vec!["cactusbase".to_string()]);
    }

    #[test]
    fn source_heads_is_none_when_nothing_is_inspectable() {
        let tmp = tempfile::tempdir().unwrap();
        let list = crate::thornlist::parse(LIST).unwrap();
        // No repos on disk at all: no information, which callers must not read
        // as "nothing changed".
        let mut progress = prodash::tree::Root::new().add_child("test probe");
        let why = source_heads(tmp.path(), &list, &mut progress).unwrap().unwrap_err();
        assert!(matches!(why, NoSources::NoneOnDisk { .. }), "{why:?}");
        // The message names where it looked, and that the directory is not
        // there at all: the clue to a `!DEFINE ROOT` mismatch.
        let said = why.to_string();
        assert!(said.contains(&tmp.path().join("Cactus/repos").display().to_string()), "{said}");
        assert!(said.contains("does not exist"), "{said}");

        // Neither a CRL list nor a plain ThornList: the reason names the line.
        let junk =
            probe_sources(tmp.path(), &tmp.path().join("Cactus"), "not a thornlist\n").unwrap().unwrap_err();
        assert!(matches!(junk, NoSources::Unparseable(_)), "{junk:?}");
        assert!(junk.to_string().contains("line 1"), "{junk}");

        // A plain list on a tree with no links into repos/ (thorns cloned
        // straight into arrangements/): its own reason, not "names no git
        // repos" (it never names any) nor a `!DEFINE ROOT` hint (it has none).
        std::fs::create_dir_all(tmp.path().join("Cactus/arrangements/CactusBase/Boundary")).unwrap();
        let why =
            probe_sources(tmp.path(), &tmp.path().join("Cactus"), "CactusBase/Boundary\n").unwrap().unwrap_err();
        assert!(matches!(why, NoSources::NoLinkedThorns { .. }), "{why:?}");
        assert!(!why.to_string().contains("DEFINE ROOT"), "{why}");

        // Repos that are there but not git repos: the probe's own error is
        // the clue, so it must survive into the message.
        let repos = tmp.path().join("Cactus/repos");
        for name in ["core", "cactusbase", "simfactory2"] {
            std::fs::create_dir_all(repos.join(name)).unwrap();
        }
        let why = source_heads(tmp.path(), &list, &mut progress).unwrap().unwrap_err();
        assert!(matches!(why, NoSources::NoneReadable { count: 3, .. }), "{why:?}");
    }

    #[test]
    fn committed_splits_the_worktree_summary_off() {
        assert_eq!(committed("abc123"), "abc123");
        assert_eq!(committed("abc123+2mod@1700"), "abc123");
    }

    fn skipped_repo(dir: PathBuf, repo: &str, branch: Option<&str>) -> SkippedRepo {
        SkippedRepo {
            repo: repo.to_string(),
            dir,
            url: "https://example.invalid/repo.git".to_string(),
            branch: branch.map(str::to_string),
            commit: None,
            reason: DirtyReason::WorktreeModified(vec!["src/foo.c".to_string()]),
            untracked: Vec::new(),
            checkouts: vec![repo.to_string()],
            record: None,
        }
    }

    #[test]
    fn force_where_moves_only_the_selected_repo_and_leaves_the_rest_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let a_dir = tmp.path().join("A");
        let b_dir = tmp.path().join("B");
        git::testrepo::init(&a_dir);
        git::testrepo::commit(&a_dir, "initial");
        git::testrepo::init(&b_dir);
        git::testrepo::commit(&b_dir, "initial");

        let mut plan = Plan {
            libraries: Vec::new(),
            git: Vec::new(),
            skipped: vec![skipped_repo(a_dir.clone(), "A", None), skipped_repo(b_dir.clone(), "B", Some("main"))],
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            root: "Cactus".to_string(),
        };

        let moved = plan.force_where(|s| s.repo == "A").unwrap();
        assert_eq!(moved, vec!["A".to_string()]);

        // B stays skipped, in place.
        assert_eq!(plan.skipped.len(), 1);
        assert_eq!(plan.skipped[0].repo, "B");

        // A moved into the fetchable set, forced, and — since it had no
        // !REPO_BRANCH (branch: None) — its branch was filled in from the
        // repo's actual current head rather than left None (which would
        // otherwise make `execute` error out).
        assert_eq!(plan.git.len(), 1);
        let forced = &plan.git[0];
        assert_eq!(forced.repo, "A");
        assert!(forced.forced.is_some());
        // A is WorktreeModified, not RemoteUrlChanged: nothing to retarget.
        assert_eq!(forced.retarget, None);
        let (head_branch, _) = git::head_of(&a_dir).unwrap();
        assert_eq!(forced.branch.as_deref(), Some(head_branch.as_str()));
    }

    #[test]
    fn force_where_retargets_a_remote_url_changed_repo_and_always_aligns() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("R");
        git::testrepo::init(&dir);
        git::testrepo::commit(&dir, "initial");
        // Plant `refs/remotes/origin/main` at HEAD: that is exactly the state
        // a repo is in after cactup fetched it from the URL the thornlist has
        // since moved away from, and it is what makes `at_local_origin_tip`
        // say "already at the tip".
        let head = git::head_of(&dir).unwrap().1;
        let remote_refs = dir.join(".git/refs/remotes/origin");
        std::fs::create_dir_all(&remote_refs).unwrap();
        std::fs::write(remote_refs.join("main"), format!("{head}\n")).unwrap();

        let wanted_url = "https://example.invalid/repo.git".to_string();
        let skipped = SkippedRepo {
            repo: "R".to_string(),
            dir: dir.clone(),
            url: wanted_url.clone(),
            branch: Some("main".to_string()),
            commit: None,
            reason: DirtyReason::RemoteUrlChanged {
                on_disk: "https://old.example.invalid/repo.git".to_string(),
                wanted: wanted_url.clone(),
                modified: Vec::new(),
            },
            untracked: Vec::new(),
            checkouts: vec!["R".to_string()],
            record: None,
        };
        let mut plan = Plan {
            libraries: Vec::new(),
            git: Vec::new(),
            skipped: vec![skipped],
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            root: "Cactus".to_string(),
        };

        let moved = plan.force_where(|_| true).unwrap();
        assert_eq!(moved, vec!["R".to_string()]);
        assert_eq!(plan.git.len(), 1);
        let forced = &plan.git[0];
        assert_eq!(forced.retarget.as_deref(), Some(wanted_url.as_str()));
        // The point of the override: `refs/remotes/origin/main` above was
        // planted at HEAD, so `at_local_origin_tip` reads "already at the
        // tip" and the unforced path would pick `Update` — which would fetch
        // the *old* upstream, since that ref and `origin` both still name it.
        // The control assertion below proves the early-out really does fire
        // on this fixture, so this one is a genuine override, not a repo
        // where `Update` was never reachable anyway.
        assert!(matches!(forced.action, GitAction::Align));
        assert!(git::at_local_origin_tip(&dir, "main"), "fixture must trip the Update early-out");

        // Control: the identical fixture, skipped for a reason that is *not*
        // a URL change, does take the `Update` early-out.
        let mut plan = Plan {
            libraries: Vec::new(),
            git: Vec::new(),
            skipped: vec![skipped_repo(dir.clone(), "R", Some("main"))],
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            root: "Cactus".to_string(),
        };
        plan.force_where(|_| true).unwrap();
        assert!(matches!(plan.git[0].action, GitAction::Update));
        assert_eq!(plan.git[0].retarget, None);
    }

    /// End-to-end: `plan()` classifies a repo whose `origin` has drifted from
    /// the thornlist's `!URL` as `RemoteUrlChanged` and skips it; forcing it
    /// moves it into `plan.git` retargeted at the *new* URL; `execute()` must
    /// then actually fetch from that new URL — not the stale one `origin`
    /// still names on disk — and leave the repo healed (a plain reprobe reads
    /// Clean), which is the whole point of persisting the rewritten URL
    /// rather than fetching from a one-off anonymous remote.
    #[test]
    fn forced_refetch_heals_a_repo_whose_remote_url_changed() {
        let tmp = tempfile::tempdir().unwrap();

        // Two independent source repos standing in for "the old upstream"
        // (what `origin` still names on disk) and "the fork" (what the
        // thornlist has since been repointed at) — distinct histories, so
        // fetching the wrong one is unmistakable in the assertions below.
        let upstream_dir = tmp.path().join("sources/upstream");
        let fork_dir = tmp.path().join("sources/fork");
        git::testrepo::init(&upstream_dir);
        git::testrepo::commit(&upstream_dir, "upstream initial");
        git::testrepo::init(&fork_dir);
        git::testrepo::commit(&fork_dir, "fork initial");
        git::testrepo::commit(&fork_dir, "fork-only commit");
        let (fork_branch, fork_head) = git::head_of(&fork_dir).unwrap();

        // `testrepo::init` never pins a branch name (no `init.defaultBranch`
        // override) — both repos are freshly init'ed the same way, so in
        // practice they land on gix's same built-in default, but that's an
        // assumption worth checking rather than baking in: `align` fetches
        // this exact branch name from the fork, so the fixture is broken if
        // it ever disagrees with what `upstream` was cloned/probed against.
        let (upstream_branch, _) = git::head_of(&upstream_dir).unwrap();
        assert_eq!(
            upstream_branch, fork_branch,
            "fixture repos must share a branch name for align to target it"
        );

        // The installation tree: `R` cloned from `upstream`, exactly as an
        // earlier fetch would have left it — this is the on-disk state the
        // thornlist below no longer matches.
        let root = tmp.path().join("install");
        let repo_dir = root.join("Cactus/repos/R");
        let upstream_url = upstream_dir.to_string_lossy().into_owned();
        let fork_url = fork_dir.to_string_lossy().into_owned();
        let mut clone_progress = prodash::tree::Root::new().add_child("test clone");
        git::clone(&upstream_url, Some(&upstream_branch), &repo_dir, &mut clone_progress).unwrap();

        // A thornlist whose `!URL` names the fork, not the upstream `R` was
        // actually cloned from.
        let list_src = format!(
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET   = $ROOT\n\
             !TYPE     = git\n\
             !URL      = {fork_url}\n\
             !REPO_BRANCH = {fork_branch}\n\
             !NAME     = R\n\
             !CHECKOUT = Makefile\n"
        );
        let list = crate::thornlist::parse(&list_src).unwrap();

        // plan(): must classify `R` as dirty for exactly `RemoteUrlChanged`,
        // never silently fetchable and never some other dirty reason.
        let mut classify = prodash::tree::Root::new().add_child("test plan");
        let mut plan = plan(&list, &root, &mut classify).unwrap();
        assert!(plan.git.is_empty(), "a retargeted repo must not be silently fetchable");
        assert_eq!(plan.skipped.len(), 1);
        assert!(
            matches!(plan.skipped[0].reason, DirtyReason::RemoteUrlChanged { .. }),
            "expected RemoteUrlChanged, got {:?}",
            plan.skipped[0].reason
        );

        // force_where(): moved into `git`, retargeted at the fork's URL, and
        // always `Align` (never `Update` — see force_where's own comment on
        // why the local-origin-tip early-out cannot be trusted here).
        let moved = plan.force_where(|_| true).unwrap();
        assert_eq!(moved, vec!["R".to_string()]);
        assert_eq!(plan.git.len(), 1);
        assert_eq!(plan.git[0].retarget.as_deref(), Some(fork_url.as_str()));
        assert!(matches!(plan.git[0].action, GitAction::Align));

        // execute(): the real end-to-end path — `set_origin_url` rewrites
        // `origin` to the fork before `align` ever runs, so the fetch below
        // must land on the fork's tip, not the upstream `origin` still named
        // on disk a moment ago.
        let report = execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "unexpected failures: {:?}", report.failures);
        assert_eq!(report.repos.len(), 1);
        let result = &report.repos[0];
        assert_eq!(result.repo, "R");
        assert_eq!(result.retargeted.as_deref(), Some(fork_url.as_str()));
        assert_eq!(result.head, fork_head.to_string());

        // The repo's actual on-disk HEAD is the fork's tip, not the
        // upstream's — proof the fetch really went to the new remote.
        let (_, head) = git::head_of(&repo_dir).unwrap();
        assert_eq!(head, fork_head);

        // And the persisted URL is what stops the *next* refetch from
        // re-flagging this repo: probing it again against the fork's URL now
        // reads Clean, whereas before `execute` it read `RemoteUrlChanged`.
        let reprobe = git::probe(&repo_dir, &fork_url, &fork_branch);
        assert!(
            matches!(reprobe.state, RepoState::Clean { .. }),
            "expected Clean after healing, got {:?}",
            reprobe.state
        );
    }

    /// Commit several files — including ones nested in subdirectories — in
    /// one shot. `git::testrepo::commit_file` only supports a single flat
    /// file per commit (each call's tree replaces, rather than extends, the
    /// previous one), which is not enough to build a `cactusbase`-shaped
    /// repo with real `CactusBase/Boundary` and `CactusBase/IOUtil` thorn
    /// directories for the placement tests below — their arrangement
    /// symlinks must resolve to *something*, and a repo whose git history
    /// never actually contains the nested path can't provide that after a
    /// real clone (unlike the working tree right after `commit_file`, a
    /// clone only ever materializes what the last commit's tree records).
    /// A download's hash is found again however its target is spelled
    /// (`$ROOT/dl/`, `./dl`): looked up by the same path it was recorded by.
    #[test]
    fn download_hashes_are_found_by_their_path() {
        let tmp = tempfile::tempdir().unwrap();
        for target in ["Cactus/dl/", "./Cactus/dl", "Cactus//dl"] {
            let list = crate::thornlist::parse(&format!(
                "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!TARGET = {target}\n!TYPE = https\n!URL = https://x/zlib\n!CHECKOUT = zlib.tgz\n"
            ))
            .unwrap();
            let c = &list.components()[0];
            let path = tmp.path().join(&c.target).join("zlib.tgz");
            let records = FetchRecords { repos: Vec::new(), downloads: vec![(download_key(tmp.path(), &path), "f".repeat(64))] };
            FetchState::record(tmp.path(), &records).unwrap();
            assert_eq!(download_hashes(tmp.path(), &list).unwrap().get("zlib").map(String::as_str), Some(&*"f".repeat(64)), "{target}");
        }
    }

    pub(crate) fn commit_files(dir: &Path, files: &[(&str, &str)]) -> gix::ObjectId {
        let repo = gix::open(dir).expect("open repo");

        #[derive(Default)]
        struct Dir<'a> {
            subdirs: std::collections::BTreeMap<&'a str, Dir<'a>>,
            files: Vec<(&'a str, &'a str)>,
        }
        let mut root = Dir::default();
        for (path, content) in files {
            let mut node = &mut root;
            let mut parts = path.split('/').peekable();
            while let Some(part) = parts.next() {
                if parts.peek().is_some() {
                    node = node.subdirs.entry(part).or_default();
                } else {
                    node.files.push((part, content));
                }
            }
        }

        fn write_dir(repo: &gix::Repository, dir: &Dir) -> gix::ObjectId {
            let mut tree = gix::objs::Tree::empty();
            for (name, sub) in &dir.subdirs {
                let sub_id = write_dir(repo, sub);
                tree.entries.push(gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Tree.into(),
                    filename: (*name).into(),
                    oid: sub_id,
                });
            }
            for (name, content) in &dir.files {
                let blob = repo.write_blob(content.as_bytes()).expect("write blob").detach();
                tree.entries.push(gix::objs::tree::Entry {
                    mode: gix::objs::tree::EntryKind::Blob.into(),
                    filename: (*name).into(),
                    oid: blob,
                });
            }
            tree.entries.sort();
            repo.write_object(&tree).expect("write tree").detach()
        }

        let tree_id = write_dir(&repo, &root);
        let parents: Vec<gix::ObjectId> = repo
            .head()
            .ok()
            .and_then(|mut h| h.peel_to_commit().ok())
            .map(|c| vec![c.id])
            .unwrap_or_default();
        let id = repo.commit("HEAD", "add files", tree_id, parents).expect("commit");

        let mut index = repo.index_from_tree(&tree_id).expect("index from tree");
        let mut opts = repo
            .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
            .expect("checkout options");
        opts.destination_is_initially_empty = false;
        opts.overwrite_existing = true;
        gix::worktree::state::checkout(
            &mut index,
            dir,
            repo.objects.clone().into_arc().expect("reopen odb"),
            &gix::progress::Discard,
            &gix::progress::Discard,
            &gix::interrupt::IS_INTERRUPTED,
            opts,
        )
        .expect("checkout");
        index.write(Default::default()).expect("write index");
        id.detach()
    }

    /// Builds two upstream source repos in the real Einstein Toolkit shape —
    /// upstream's own `expressions.th` uses `!DEFINE ROOT = CactusMin`, so a
    /// non-`Cactus` root is a real configuration, not a hypothetical — and
    /// returns thornlist text naming `root`: `core` is the flesh, checking
    /// `doc`/`lib`/`Makefile`/`src` straight into `$ROOT` (a multi-item
    /// `!CHECKOUT`, as the real flesh section has); `cactusbase` is an
    /// arrangement repo whose `!CHECKOUT` names two thorns
    /// (`CactusBase/Boundary`, `CactusBase/IOUtil`) from that one repo. Every
    /// placement case below differs only in the `root` passed here, so a
    /// helper keeps them from turning into copy-pasted setup blocks.
    fn et_shaped_thornlist(sources: &Path, root: &str) -> String {
        let core = sources.join("core");
        git::testrepo::init(&core);
        commit_files(&core, &[("doc", "d\n"), ("lib", "l\n"), ("Makefile", "m\n"), ("src", "s\n")]);

        let cactusbase = sources.join("cactusbase");
        git::testrepo::init(&cactusbase);
        commit_files(
            &cactusbase,
            &[("CactusBase/Boundary/interface.ccl", "b\n"), ("CactusBase/IOUtil/interface.ccl", "i\n")],
        );

        format!(
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = {root}\n\n\
             !TARGET   = $ROOT\n\
             !TYPE     = git\n\
             !URL      = {}\n\
             !NAME     = core\n\
             !CHECKOUT = doc lib Makefile src\n\n\
             !TARGET   = $ROOT/arrangements\n\
             !TYPE     = git\n\
             !URL      = {}\n\
             !CHECKOUT = CactusBase/Boundary CactusBase/IOUtil\n",
            core.to_string_lossy(),
            cactusbase.to_string_lossy(),
        )
    }

    /// Parse `list_src` and run [`plan`] against it, with a throwaway
    /// progress sink — the shared first half of every placement case below.
    fn plan_for(list_src: &str, install_root: &Path) -> Plan {
        let list = crate::thornlist::parse(list_src).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        plan(&list, install_root, &mut progress).unwrap()
    }

    /// After a successful `execute()` of an [`et_shaped_thornlist`] plan:
    /// the flesh checkout items sit directly under `<install_root>/<root>`,
    /// and both arrangement thorns are symlinks that resolve to something
    /// real inside `<install_root>/<root>/repos/cactusbase` — not merely
    /// present, since a dangling symlink is exactly the failure mode that
    /// matters here.
    fn assert_tree_lands_under_root(install_root: &Path, root: &str) {
        let root_dir = install_root.join(root);
        let repos_dir = std::fs::canonicalize(root_dir.join("repos"))
            .unwrap_or_else(|e| panic!("repos dir missing under {}: {e}", root_dir.display()));

        for item in ["doc", "lib", "Makefile", "src"] {
            let link = root_dir.join(item);
            let resolved = std::fs::canonicalize(&link)
                .unwrap_or_else(|e| panic!("flesh item {item} missing or dangling: {e}"));
            assert!(
                resolved.starts_with(repos_dir.join("core")),
                "{item} resolved to {}, not inside {}",
                resolved.display(),
                repos_dir.join("core").display()
            );
        }

        for thorn in ["Boundary", "IOUtil"] {
            let link = root_dir.join("arrangements").join("CactusBase").join(thorn);
            let meta = std::fs::symlink_metadata(&link)
                .unwrap_or_else(|e| panic!("expected a symlink at {}: {e}", link.display()));
            assert!(meta.file_type().is_symlink(), "{} should be a symlink", link.display());
            let resolved = std::fs::canonicalize(&link)
                .unwrap_or_else(|e| panic!("{} is a dangling symlink: {e}", link.display()));
            assert!(
                resolved.starts_with(repos_dir.join("cactusbase")),
                "{} resolved to {}, not inside {}",
                link.display(),
                resolved.display(),
                repos_dir.join("cactusbase").display()
            );
        }
    }

    /// No directory literally named `Cactus` exists anywhere under
    /// `install_root` — the actual reported symptom of the split-installation
    /// bug: fetching honored a custom `!DEFINE ROOT` while the rest of
    /// `install` still assumed `Cactus`, so a stray `Cactus/` (empty, or half
    /// the tree) showed up alongside the real root.
    fn assert_no_cactus_dir_anywhere(install_root: &Path) {
        fn walk(dir: &Path, found: &mut Vec<PathBuf>) {
            let Ok(entries) = std::fs::read_dir(dir) else { return };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.file_name().and_then(|n| n.to_str()) == Some("Cactus") {
                    found.push(path.clone());
                }
                // Don't follow symlinks (arrangement thorns) or descend into
                // `.git` — neither can contain a directory relevant here,
                // and a symlink cycle back into a repo is otherwise possible.
                if path.is_dir()
                    && !path.is_symlink()
                    && path.file_name().and_then(|n| n.to_str()) != Some(".git")
                {
                    walk(&path, found);
                }
            }
        }
        let mut found = Vec::new();
        if install_root.exists() {
            walk(install_root, &mut found);
        }
        assert!(found.is_empty(), "found stray Cactus dir(s): {found:?}");
    }

    /// A clone made pinned (it knows no remote branch) from a list naming a
    /// pin and the tag it came from, then fetched from a list naming neither (a stack
    /// release's list, then a base that follows its branch): back onto the
    /// remote's default branch, as the remote says, never skipped as the
    /// user's detached HEAD; and a forced fetch of it resolves a branch too.
    #[test]
    fn a_clone_made_pinned_goes_back_onto_the_remote_s_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("sources/upstream");
        git::testrepo::init(&upstream);
        let c1 = git::testrepo::commit_tree(&upstream, &[("v", "1\n", false)]);
        // The pin taken from a tag, which the list names as its branch.
        gix::open(&upstream).unwrap().reference("refs/tags/v1.0.0", c1, gix::refs::transaction::PreviousValue::Any, "tag").unwrap();
        git::testrepo::commit_tree(&upstream, &[("v", "2\n", false)]);
        let (branch, tip) = git::head_of(&upstream).unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let root = tmp.path().join("install");
        let repo_dir = root.join("Cactus/repos/R");
        let list = |pinned: bool| {
            let head = if pinned { format!("!REPO_BRANCH = v1.0.0\n!REPO_COMMIT = {c1}\n") } else { String::new() };
            crate::thornlist::parse(&format!(
                "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\n!TARGET = $ROOT\n!TYPE = git\n\
                 !URL = {url}\n{head}!NAME = R\n!CHECKOUT = Makefile\n"
            ))
            .unwrap()
        };
        let planned = |list: &Thornlist| {
            let mut progress = prodash::tree::Root::new().add_child("test plan");
            plan(list, &root, &mut progress).unwrap()
        };
        let p = planned(&list(true));
        let report = execute(&p, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        FetchState::record(&root, &report.records()).unwrap();
        assert_eq!(git::default_branch(&repo_dir), None, "a clone made pinned knows no remote branch");

        let p = planned(&list(false));
        assert!(matches!(p.git[0].action, GitAction::Align), "{:?}", p.git);
        assert_eq!(p.git[0].branch.as_deref(), Some(branch.as_str()));
        let report = execute(&p, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(git::head_of(&repo_dir).unwrap(), (branch.clone(), tip));
        FetchState::record(&root, &report.records()).unwrap();

        // A detached HEAD the user made, forced: a branch resolved all the same.
        let mut progress = prodash::tree::Root::new().add_child("test checkout");
        git::pin(&repo_dir, None, c1, false, &mut progress).unwrap();
        let mut skipped = planned(&list(false));
        assert!(skipped.git.is_empty(), "the user's detached HEAD is skipped");
        skipped.force().unwrap();
        assert_eq!(skipped.git[0].branch.as_deref(), Some(branch.as_str()));
    }

    /// A local server that accepts `git://` connections and never answers:
    /// a remote that stalls. Its URL for `repo`.
    fn stalling_remote(repo: &str) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let mut held = Vec::new();
            for stream in listener.incoming().flatten() {
                held.push(stream);
            }
        });
        format!("git://127.0.0.1:{port}/{repo}.git")
    }

    /// `n` clones made pinned (each from a tag it names as its branch), then
    /// pointed at `url`, list and record alike: what an installation of a
    /// stack release looks like to a list that stops pinning. The root, and
    /// the list that no longer pins them.
    fn clones_made_pinned(tmp: &Path, n: usize, url: &str) -> (PathBuf, Thornlist) {
        let upstream = tmp.join("sources/upstream");
        git::testrepo::init(&upstream);
        let c1 = git::testrepo::commit_tree(&upstream, &[("v", "1\n", false)]);
        gix::open(&upstream).unwrap().reference("refs/tags/v1.0.0", c1, gix::refs::transaction::PreviousValue::Any, "tag").unwrap();
        let local = upstream.to_string_lossy().into_owned();
        let root = tmp.join("install");
        let sections = |url: &str, pinned: bool| -> String {
            let head = if pinned { format!("!REPO_BRANCH = v1.0.0\n!REPO_COMMIT = {c1}\n") } else { String::new() };
            (0..n)
                .map(|i| format!("\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {url}\n{head}!NAME = R{i}\n!CHECKOUT = A{i}/T\n"))
                .collect()
        };
        let list = |url: &str, pinned: bool| {
            crate::thornlist::parse(&format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}", sections(url, pinned))).unwrap()
        };
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let p = plan(&list(&local, true), &root, &mut progress).unwrap();
        let report = execute(&p, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        FetchState::record(&root, &report.records()).unwrap();
        for i in 0..n {
            git::set_origin_url(&root.join(format!("Cactus/repos/R{i}")), url).unwrap();
        }
        let state = root.join(".cactup/fetch-state.toml");
        let text = std::fs::read_to_string(&state).unwrap().replace(&local, url);
        std::fs::write(&state, text).unwrap();
        (root, list(url, false))
    }

    /// Eighty clones made pinned whose list stops pinning ask their remotes
    /// where to go back to: in parallel, on the probe's pool, each within
    /// the ask's time; a remote that does not answer leaves its clone going
    /// back to the label of its pinned fetch, said on its line.
    #[test]
    fn the_remote_asks_of_a_plan_go_in_parallel_and_time_out() {
        TEST_FOLLOW_TIMEOUT_MS.store(1000, std::sync::atomic::Ordering::Relaxed);
        let tmp = tempfile::tempdir().unwrap();
        let url = stalling_remote("r");
        let (root, list) = clones_made_pinned(tmp.path(), 8, &url);
        let started = std::time::Instant::now();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let p = plan(&list, &root, &mut progress).unwrap();
        let took = started.elapsed();
        assert_eq!(p.git.len(), 8, "{:?}", p.skipped);
        for g in &p.git {
            assert!(matches!(g.action, GitAction::Align), "{g:?}");
            assert_eq!(g.branch.as_deref(), Some("v1.0.0"), "{g:?}");
            let note = g.note.as_deref().unwrap_or_default();
            assert!(note.contains("did not answer") && note.contains("going back to v1.0.0"), "{note}");
        }
        // Eight asks of a second each: together, not one after another.
        assert!(took < std::time::Duration::from_secs(4), "{took:?}");
    }

    /// A plan narrowed to some clones looks at no other: not probed, and
    /// its remote not asked (what `add` and `remove` plan).
    #[test]
    fn a_narrowed_plan_asks_nothing_of_the_clones_it_leaves_out() {
        TEST_FOLLOW_TIMEOUT_MS.store(1000, std::sync::atomic::Ordering::Relaxed);
        let tmp = tempfile::tempdir().unwrap();
        let url = stalling_remote("r");
        let (root, list) = clones_made_pinned(tmp.path(), 3, &url);
        let started = std::time::Instant::now();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let p = plan_only(&list, &root, &mut progress, Some(&BTreeSet::new())).unwrap();
        assert!(p.git.is_empty() && p.skipped.is_empty(), "{p:?}");
        assert!(started.elapsed() < std::time::Duration::from_millis(800), "{:?}", started.elapsed());
    }

    /// Ctrl-C while a plan asks remotes fails the plan ("Interrupted"),
    /// at once, never a fallback that would read as a plan. The interrupt
    /// flag is the process's: run in a child of its own.
    #[test]
    fn an_interrupt_while_a_plan_asks_remotes_fails_it() {
        if std::env::var_os("CACTUP_TEST_INTERRUPT_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["fetch::tests::an_interrupt_while_a_plan_asks_remotes_fails_it", "--exact", "--test-threads=1"])
                .env("CACTUP_TEST_INTERRUPT_CHILD", "1")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
            return;
        }
        TEST_FOLLOW_TIMEOUT_MS.store(20_000, std::sync::atomic::Ordering::Relaxed);
        let tmp = tempfile::tempdir().unwrap();
        let url = stalling_remote("r");
        let (root, list) = clones_made_pinned(tmp.path(), 3, &url);
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        let started = std::time::Instant::now();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let e = plan(&list, &root, &mut progress).unwrap_err();
        assert!(format!("{e:#}").to_lowercase().contains("interrupted"), "{e:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(3), "{:?}", started.elapsed());
    }

    /// A list of `n` fresh clones from `url` (each its own repo, so each its
    /// own fetch): the plan to clone them under `root`.
    fn fresh_clones(root: &Path, n: usize, url: &str) -> Plan {
        let sections: String = (0..n)
            .map(|i| format!("\n!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {url}\n!NAME = S{i}\n!CHECKOUT = S{i}/T\n"))
            .collect();
        let list = crate::thornlist::parse(&format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{sections}")).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        plan(&list, root, &mut progress).unwrap()
    }

    /// A remote that accepts the connection and never answers fails each
    /// fetch from it once nothing has moved for the silence bound — clones,
    /// and aligns of clones already there — each its own failure saying
    /// so, rather than holding the fetch (and the process) for ever.
    #[test]
    fn a_fetch_from_a_remote_that_never_answers_fails_after_the_silence() {
        TEST_FOLLOW_TIMEOUT_MS.store(1000, std::sync::atomic::Ordering::Relaxed);
        git::TEST_SILENCE_MS.store(1000, std::sync::atomic::Ordering::Relaxed);
        let tmp = tempfile::tempdir().unwrap();
        let url = stalling_remote("r");
        let (root, list) = clones_made_pinned(tmp.path(), 2, &url);
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let mut p = plan(&list, &root, &mut progress).unwrap();
        assert!(p.git.iter().all(|g| matches!(g.action, GitAction::Align)), "{:?}", p.git);
        p.git.extend(fresh_clones(&root, 2, &url).git);
        let started = std::time::Instant::now();
        let report = execute(&p, &root).unwrap();
        let took = started.elapsed();
        assert_eq!(report.failures.len(), 4, "{:?}", report.failures);
        for f in &report.failures {
            assert!(f.error.contains(&format!("{url} did not answer within 1 s")), "{f:?}");
        }
        // Gone too: a clone given up on leaves nothing to trip the next fetch.
        assert!(!root.join("Cactus/repos/S0").exists());
        // Four at once, each given up on after its second of silence.
        assert!(took < std::time::Duration::from_secs(5), "{took:?}");
    }

    /// Ctrl-C while fetches hang in their handshake fails the fetch at once
    /// ("Interrupted"), however long the silence bound. The interrupt flag
    /// is the process's: run in a child of its own.
    #[test]
    fn an_interrupt_while_a_fetch_hangs_in_its_handshake_fails_it_at_once() {
        if std::env::var_os("CACTUP_TEST_INTERRUPT_CHILD").is_none() {
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "fetch::tests::an_interrupt_while_a_fetch_hangs_in_its_handshake_fails_it_at_once",
                    "--exact",
                    "--test-threads=1",
                ])
                .env("CACTUP_TEST_INTERRUPT_CHILD", "1")
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
            return;
        }
        git::TEST_SILENCE_MS.store(60_000, std::sync::atomic::Ordering::Relaxed);
        let tmp = tempfile::tempdir().unwrap();
        let url = stalling_remote("r");
        let root = tmp.path().join("install");
        let p = fresh_clones(&root, 3, &url);
        let fetching = std::thread::spawn(move || {
            let report = execute(&p, &root);
            (report, std::time::Instant::now())
        });
        std::thread::sleep(std::time::Duration::from_millis(500));
        gix::interrupt::IS_INTERRUPTED.store(true, std::sync::atomic::Ordering::SeqCst);
        let interrupted = std::time::Instant::now();
        let (report, returned) = fetching.join().unwrap();
        let report = report.unwrap();
        assert_eq!(report.failures.len(), 3, "{:?}", report.failures);
        assert!(report.failures.iter().all(|f| f.error.to_lowercase().contains("interrupted")), "{:?}", report.failures);
        let took = returned.duration_since(interrupted);
        assert!(took < std::time::Duration::from_millis(900), "{took:?}");
    }

    /// An ssh remote whose command hangs (and runs its own child, as `sh -c
    /// 'ssh …'` does) is given up on after the silence, and the processes
    /// it started are killed, not left behind. The ssh command is the
    /// process's environment: run in a child of its own.
    #[test]
    fn a_hung_ssh_command_is_killed_when_its_fetch_is_given_up_on() {
        let Some(dir) = std::env::var_os("CACTUP_TEST_SSH_CHILD") else {
            let tmp = tempfile::tempdir().unwrap();
            let ssh = tmp.path().join("ssh");
            std::fs::write(&ssh, "#!/bin/sh\nsleep 600 &\necho $! > \"$(dirname \"$0\")/sleep.pid\"\nwait\n").unwrap();
            std::fs::set_permissions(&ssh, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
            let out = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["fetch::tests::a_hung_ssh_command_is_killed_when_its_fetch_is_given_up_on", "--exact", "--test-threads=1"])
                .env("CACTUP_TEST_SSH_CHILD", tmp.path())
                .env("GIT_SSH_COMMAND", &ssh)
                .output()
                .unwrap();
            let said = String::from_utf8_lossy(&out.stdout);
            assert!(out.status.success() && said.contains("1 passed"), "{said}{}", String::from_utf8_lossy(&out.stderr));
            return;
        };
        let dir = PathBuf::from(dir);
        git::TEST_SILENCE_MS.store(1000, std::sync::atomic::Ordering::Relaxed);
        let dest = dir.join("clone");
        let mut progress = prodash::tree::Root::new().add_child("test clone");
        let started = std::time::Instant::now();
        let e = git::clone("ssh://example.invalid/r.git", None, &dest, &mut progress).unwrap_err();
        assert!(format!("{e:#}").contains("did not answer within 1 s"), "{e:#}");
        assert!(started.elapsed() < std::time::Duration::from_secs(4), "{:?}", started.elapsed());
        let pid = std::fs::read_to_string(dir.join("sleep.pid")).unwrap();
        let gone = || std::fs::read_to_string(format!("/proc/{}/stat", pid.trim())).map_or(true, |s| s.contains(") Z "));
        let waited = std::time::Instant::now();
        while !gone() && waited.elapsed() < std::time::Duration::from_secs(5) {
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(gone(), "the ssh command's child {} outlived its fetch", pid.trim());
    }

    /// Where a restore puts a clone, offline: none on disk, the remote's
    /// default (a fresh clone's), whatever a fetch record says; one on a
    /// branch of the list's remote keeps that branch, not the default.
    #[test]
    fn a_restore_keeps_a_clone_s_branch_and_clones_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("sources/upstream");
        git::testrepo::init(&upstream);
        let c1 = git::testrepo::commit_tree(&upstream, &[("v", "1\n", false)]);
        let (main, _) = git::head_of(&upstream).unwrap();
        gix::open(&upstream).unwrap().reference("refs/heads/develop", c1, gix::refs::transaction::PreviousValue::Any, "branch").unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let pinned = RepoRecord { url: url.clone(), branch: Some("v1".into()), head: c1.to_string(), commit: Some(c1.to_string()) };
        let absent = tmp.path().join("nowhere");
        assert_eq!(known_after_restore(&absent, &url, None, Some(&pinned)), Known::RemoteDefault { label: None });

        let dir = tmp.path().join("clone");
        let mut progress = prodash::tree::Root::new().add_child("test");
        git::clone(&url, Some("develop"), &dir, &mut progress).unwrap();
        let clone = gix::open(&dir).unwrap();
        clone.reference(format!("refs/remotes/origin/{main}"), c1, gix::refs::transaction::PreviousValue::Any, "test").unwrap();
        std::fs::write(clone.git_dir().join("refs/remotes/origin/HEAD"), format!("ref: refs/remotes/origin/{main}\n")).unwrap();
        assert_eq!(git::default_branch(&dir).as_deref(), Some(main.as_str()));
        assert_eq!(known_after_restore(&dir, &url, None, None), Known::Branch("develop".into()));
        assert_eq!(known_after_restore(&dir, &url, Some("main"), None), Known::Branch("main".into()));
    }

    /// A clone forced out of a detached HEAD the user made goes back to the
    /// branch its last fetch followed, not to the remote's default; and when
    /// the list moved it to another remote, the clone's own refs (the old
    /// remote's) say nothing about the new one's default: that remote does.
    /// Either way the branch `align` puts it on gets its upstream.
    #[test]
    fn a_forced_clone_follows_its_record_and_the_new_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("sources/upstream");
        git::testrepo::init(&upstream);
        let c1 = git::testrepo::commit_tree(&upstream, &[("v", "1\n", false)]);
        let c2 = git::testrepo::commit_tree(&upstream, &[("v", "2\n", false)]);
        let (main, _) = git::head_of(&upstream).unwrap();
        gix::open(&upstream).unwrap().reference("refs/heads/develop", c2, gix::refs::transaction::PreviousValue::Any, "branch").unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let root = tmp.path().join("install");
        let repo_dir = root.join("Cactus/repos/R");
        let list = |url: &str, branch: Option<&str>| {
            let branch = branch.map(|b| format!("!REPO_BRANCH = {b}\n")).unwrap_or_default();
            crate::thornlist::parse(&format!(
                "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\n!TARGET = $ROOT\n!TYPE = git\n\
                 !URL = {url}\n{branch}!NAME = R\n!CHECKOUT = Makefile\n"
            ))
            .unwrap()
        };
        let planned = |list: &Thornlist| {
            let mut progress = prodash::tree::Root::new().add_child("test plan");
            plan(list, &root, &mut progress).unwrap()
        };
        let p = planned(&list(&url, Some("develop")));
        let report = execute(&p, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        FetchState::record(&root, &report.records()).unwrap();
        // The clone knows the remote's default (main), as a full clone would.
        let clone = gix::open(&repo_dir).unwrap();
        clone.reference(format!("refs/remotes/origin/{main}"), c2, gix::refs::transaction::PreviousValue::Any, "test").unwrap();
        std::fs::write(clone.git_dir().join("refs/remotes/origin/HEAD"), format!("ref: refs/remotes/origin/{main}\n")).unwrap();
        assert_eq!(git::default_branch(&repo_dir).as_deref(), Some(main.as_str()));
        // align gave develop its upstream, as a clone would have.
        let config = std::fs::read_to_string(clone.git_dir().join("config")).unwrap();
        assert!(config.contains("[branch \"develop\"]") && config.contains("merge = refs/heads/develop"), "{config}");

        let mut progress = prodash::tree::Root::new().add_child("test checkout");
        git::pin(&repo_dir, None, c1, false, &mut progress).unwrap();
        let mut skipped = planned(&list(&url, None));
        assert!(skipped.git.is_empty(), "the user's detached HEAD is skipped");
        skipped.force().unwrap();
        assert_eq!(skipped.git[0].branch.as_deref(), Some("develop"), "the branch its last fetch followed");

        // Another remote, whose default is trunk: neither the record (the old
        // remote's) nor the clone's refs speak for it.
        let other = tmp.path().join("sources/other");
        git::testrepo::init(&other);
        let t1 = git::testrepo::commit_tree(&other, &[("t", "1\n", false)]);
        let repo = gix::open(&other).unwrap();
        repo.reference("refs/heads/trunk", t1, gix::refs::transaction::PreviousValue::Any, "branch").unwrap();
        std::fs::write(repo.git_dir().join("HEAD"), "ref: refs/heads/trunk\n").unwrap();
        let other_url = other.to_string_lossy().into_owned();
        let mut skipped = planned(&list(&other_url, None));
        assert!(skipped.git.is_empty());
        assert!(matches!(skipped.skipped[0].reason, DirtyReason::RemoteUrlChanged { .. }));
        skipped.force().unwrap();
        assert_eq!(skipped.git[0].branch.as_deref(), Some("trunk"), "the new remote's default");
        let report = execute(&skipped, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(git::head_of(&repo_dir).unwrap(), ("trunk".to_owned(), t1));
    }

    /// A branch a fetch aligns to gets its upstream, as a clone's does, and
    /// one the user set stays; the clone's `config` keeps its mode through
    /// any edit (a group-shared tree's 0664), the origin's re-pointing too;
    /// and a config that cannot be written leaves the fetch done, its record
    /// written, said as a warning.
    #[test]
    fn a_branch_gets_its_upstream_and_the_config_keeps_its_mode() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("up");
        git::testrepo::init(&upstream);
        let c1 = git::testrepo::commit_tree(&upstream, &[("v", "1\n", false)]);
        let repo = gix::open(&upstream).unwrap();
        repo.reference("refs/heads/feature", c1, gix::refs::transaction::PreviousValue::Any, "branch").unwrap();
        repo.reference("refs/heads/mine", c1, gix::refs::transaction::PreviousValue::Any, "branch").unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        let list = |branch: &str| {
            crate::thornlist::parse(&format!(
                "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\n!TARGET = $ROOT\n!TYPE = git\n\
                 !URL = {url}\n!REPO_BRANCH = {branch}\n!NAME = R\n!CHECKOUT = Makefile\n"
            ))
            .unwrap()
        };
        let fetch = |branch: &str| {
            let mut progress = prodash::tree::Root::new().add_child("test plan");
            let p = plan(&list(branch), &root, &mut progress).unwrap();
            let report = execute(&p, &root).unwrap();
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            FetchState::record(&root, &report.records()).unwrap();
            report
        };
        let (main, _) = git::head_of(&upstream).unwrap();
        fetch(&main);
        let config_path = gix::open(&dir).unwrap().git_dir().join("config");
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o664)).unwrap();
        // One the user gave an upstream of their own.
        let mut text = std::fs::read_to_string(&config_path).unwrap();
        text.push_str("[branch \"mine\"]\n\tremote = fork\n\tmerge = refs/heads/elsewhere\n");
        std::fs::write(&config_path, text).unwrap();
        fetch("feature");
        fetch("mine");
        let text = std::fs::read_to_string(&config_path).unwrap();
        assert!(text.contains("[branch \"feature\"]\n\tremote = origin\n\tmerge = refs/heads/feature"), "{text}");
        assert!(text.contains("merge = refs/heads/elsewhere") && !text.contains("merge = refs/heads/mine"), "{text}");
        assert_eq!(mode(&config_path), 0o664, "the upstream's write keeps the mode");
        git::set_origin_url(&dir, &url).unwrap();
        assert_eq!(mode(&config_path), 0o664, "re-pointing origin keeps the mode");

        // A config that cannot be written: the fetch stands, with a warning.
        upstream_fails(&dir, true);
        let report = fetch("feature");
        upstream_fails(&dir, false);
        // (feature has its upstream already: nothing to write, nothing to warn.)
        assert!(report.repos[0].warning.is_none());
        let fresh = gix::open(&upstream).unwrap();
        fresh.reference("refs/heads/other", c1, gix::refs::transaction::PreviousValue::Any, "branch").unwrap();
        upstream_fails(&dir, true);
        let report = fetch("other");
        upstream_fails(&dir, false);
        let r = &report.repos[0];
        assert!(r.warning.as_deref().is_some_and(|w| w.contains("upstream could not be set")), "{r:?}");
        assert_eq!(git::head_of(&dir).unwrap().0, "other", "the checkout stands");
        let state = FetchState::read(&root).unwrap().unwrap();
        assert_eq!(state.repos["R"].branch.as_deref(), Some("other"), "and its record is written");
    }

    /// Make writing `dir`'s upstream fail, or no longer.
    fn upstream_fails(dir: &Path, fail: bool) {
        let mut hook = git::FAIL_UPSTREAM.lock().unwrap();
        hook.retain(|d| d != dir);
        if fail {
            hook.push(dir.to_owned());
        }
    }

    /// Regression pin for the split-installation bug's planning half:
    /// §3.2 pins, end to end against a real upstream: a pinned repo is
    /// cloned detached at its commit (not the tip); a refetch leaves it
    /// there without touching the network; moving the pin moves it; dropping
    /// the pin puts it back on its branch; and a detached HEAD cactup did
    /// not leave is the user's, skipped like any other local state.
    #[test]
    fn pinned_repos_follow_their_pin_and_nothing_else() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("sources/upstream");
        git::testrepo::init(&upstream);
        let c1 = git::testrepo::commit_tree(&upstream, &[("v", "1\n", false)]);
        let c2 = git::testrepo::commit_tree(&upstream, &[("v", "2\n", false)]);
        let c3 = git::testrepo::commit_tree(&upstream, &[("v", "3\n", false)]);
        let (branch, _) = git::head_of(&upstream).unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let root = tmp.path().join("install");
        let repo_dir = root.join("Cactus/repos/R");

        let list = |pin: Option<&gix::ObjectId>| {
            let pin = pin.map(|c| format!("!REPO_COMMIT = {c}\n")).unwrap_or_default();
            crate::thornlist::parse(&format!(
                "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n\n!TARGET = $ROOT\n!TYPE = git\n\
                 !URL = {url}\n!REPO_BRANCH = {branch}\n{pin}!NAME = R\n!CHECKOUT = Makefile\n"
            ))
            .unwrap()
        };
        let fetch = |list: &Thornlist| {
            let mut progress = prodash::tree::Root::new().add_child("test plan");
            let planned = plan(list, &root, &mut progress).unwrap();
            let report = execute(&planned, &root).unwrap();
            assert!(report.failures.is_empty(), "{:?}", report.failures);
            FetchState::record(&root, &report.records()).unwrap();
            (planned, report)
        };
        let content = || std::fs::read_to_string(repo_dir.join("v")).unwrap();

        // Absent: cloned straight onto the pin, detached, though c3 is the tip.
        let (planned, _) = fetch(&list(Some(&c2)));
        assert!(matches!(planned.git[0].action, GitAction::Clone));
        assert_eq!(git::head_of(&repo_dir).unwrap(), (git::DETACHED.to_owned(), c2));
        assert_eq!(content(), "2\n");
        let recorded = FetchState::read(&root).unwrap().unwrap();
        assert_eq!(recorded.repos["R"].commit.as_deref(), Some(c2.to_string().as_str()));

        // Upstream moves on, and is then unreachable: a refetch of the same
        // pin plans a Pin that changes nothing and needs no network.
        git::testrepo::commit_tree(&upstream, &[("v", "4\n", false)]);
        let away = tmp.path().join("sources/away");
        std::fs::rename(&upstream, &away).unwrap();
        let (planned, report) = fetch(&list(Some(&c2)));
        assert!(matches!(planned.git[0].action, GitAction::Pin));
        assert!(!report.repos[0].changed, "a pinned repo is never moved by a refetch");
        assert_eq!(git::head_of(&repo_dir).unwrap().1, c2);
        std::fs::rename(&away, &upstream).unwrap();

        // Moving the pin: the detached HEAD is cactup's (the fetch record
        // holds it), so it moves.
        fetch(&list(Some(&c3)));
        assert_eq!(git::head_of(&repo_dir).unwrap(), (git::DETACHED.to_owned(), c3));
        assert_eq!(content(), "3\n");

        // Dropping the pin: back onto the branch, at its tip.
        let (planned, _) = fetch(&list(None));
        assert!(matches!(planned.git[0].action, GitAction::Align));
        assert_eq!(git::head_of(&repo_dir).unwrap().0, branch);
        assert_eq!(FetchState::read(&root).unwrap().unwrap().repos["R"].commit, None);
        assert_eq!(content(), "4\n");

        // A detached HEAD the user made is theirs: skipped, never moved. (A
        // clean checkout of c1 that cactup's fetch record does not hold.)
        let mut progress = prodash::tree::Root::new().add_child("test checkout");
        git::pin(&repo_dir, None, c1, false, &mut progress).unwrap();
        let tip = RepoRecord { url: url.clone(), branch: Some(branch.clone()), head: c3.to_string(), commit: None };
        FetchState::record(&root, &FetchRecords { repos: vec![("R".to_owned(), tip)], downloads: Vec::new() }).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let mut skipped = plan(&list(Some(&c3)), &root, &mut progress).unwrap();
        assert!(skipped.git.is_empty());
        assert!(matches!(skipped.skipped[0].reason, DirtyReason::DetachedHead(id) if id == c1));
        // Forcing it pins it all the same.
        skipped.force().unwrap();
        assert!(matches!(skipped.git[0].action, GitAction::Pin));
    }

    fn exhaustive_fetch_state() -> FetchState {
        let mut repos = indexmap::IndexMap::new();
        for (name, branch) in [("CarpetX", "main"), ("Cactus", "master")] {
            repos.insert(
                name.to_owned(),
                RepoRecord {
                    url: format!("https://github.com/example/{name}.git"),
                    branch: Some(branch.to_owned()),
                    head: "3f2c9e1a00112233445566778899aabbccddeeff".to_owned(),
                    commit: Some("3f2c9e1a00112233445566778899aabbccddeeff".to_owned()),
                },
            );
        }
        let mut downloads = indexmap::IndexMap::new();
        downloads.insert("Cactus/dl/data.tar.gz".to_owned(), "ab".repeat(32));
        FetchState { repos, downloads }
    }

    #[test]
    fn fetch_state_format_generic_tests() {
        use crate::format::testing::*;
        let sample = exhaustive_fetch_state();
        descriptor_is_consistent(&FETCH_STATE);
        let fixtures = fixture_dir("fetch-state");
        shape_is_pinned(&FETCH_STATE, &fixtures, &sample);
        every_fixture_loads::<FetchState, _>(&FETCH_STATE, &fixtures);
        newer_is_refused::<FetchState, _>(&FETCH_STATE, "fetch-state.toml", "repos = 7\n", &sample);
        unknown_keys_survive(
            &FETCH_STATE,
            "fetch-state.toml",
            &sample,
            &["repos", "CarpetX"],
            |s: &mut FetchState| s.repos["CarpetX"].commit = None,
            &["repos", "CarpetX", "commit"],
        );
    }

    /// `record` through a real file, from the shapes that shipped: the
    /// `schema` key a deployed binary requires is always written, first; keys
    /// this binary does not know survive on records it did not touch and
    /// do not on one it replaced; a newer file is refused and left alone.
    #[test]
    fn record_keeps_schema_and_unknown_keys_and_refuses_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let file = root.join(".cactup/fetch-state.toml");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let fixture = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/formats/fetch-state/v1-nodownloads.toml"),
        )
        .unwrap();
        // A newer binary's additions: top level, on each repo record.
        let future = fixture
            .replace("[repos.CarpetX]\n", "[repos.CarpetX]\nnewer-key = 1\n")
            .replace("[repos.Cactus]\n", "[repos.Cactus]\nnewer-key = 2\n")
            + "\n[future]\nx = 1\n";
        std::fs::write(&file, &future).unwrap();

        let moved = RepoRecord {
            url: "https://github.com/example/CarpetX.git".to_owned(),
            branch: Some("main".to_owned()),
            head: "ffffffffffffffffffffffffffffffffffffffff".to_owned(),
            commit: None,
        };
        let sum = "cd".repeat(32);
        let records = FetchRecords {
            repos: vec![("CarpetX".to_owned(), moved)],
            downloads: vec![("Cactus/dl/data.tar.gz".to_owned(), sum.clone())],
        };
        FetchState::record(root, &records).unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.starts_with("schema = 1\n"), "{text}");
        let doc: toml::Table = toml::from_str(&text).unwrap();
        assert_eq!(doc["future"]["x"].as_integer(), Some(1), "{text}");
        assert_eq!(doc["repos"]["Cactus"]["newer-key"].as_integer(), Some(2), "an untouched record keeps its keys");
        assert!(doc["repos"]["CarpetX"].get("newer-key").is_none(), "a replaced record starts clean: {text}");
        assert_eq!(doc["repos"]["CarpetX"]["head"].as_str(), Some("ffffffffffffffffffffffffffffffffffffffff"));
        assert_eq!(doc["downloads"]["Cactus/dl/data.tar.gz"].as_str(), Some(sum.as_str()));

        // A newer file: refused, byte for byte as it was.
        let newer = text.replacen("schema = 1", "schema = 2", 1);
        std::fs::write(&file, &newer).unwrap();
        let err = format!("{:#}", FetchState::record(root, &records).unwrap_err());
        assert!(err.contains("upgrade cactup") && err.contains("fetch-state.toml"), "{err}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), newer);
        assert!(FetchState::read(root).is_err());
    }

    /// A first fetch (no file yet) writes `schema` too.
    #[test]
    fn a_first_record_writes_the_schema_key() {
        let tmp = tempfile::tempdir().unwrap();
        FetchState::record(tmp.path(), &FetchRecords { repos: Vec::new(), downloads: Vec::new() }).unwrap();
        let text = std::fs::read_to_string(tmp.path().join(".cactup/fetch-state.toml")).unwrap();
        assert!(text.starts_with("schema = 1\n"), "{text}");
    }

    /// `plan()` must place every repo under the thornlist's declared root,
    /// not a hardcoded `Cactus` — before the fix, the fetcher already got
    /// this right (it read `list.root()`), but nothing enforced it, so a
    /// future regression here would silently reopen the exact same split
    /// between where components are planned and where `install` looks.
    #[test]
    fn plan_places_every_repo_under_the_declared_root() {
        let tmp = tempfile::tempdir().unwrap();
        let sources = tmp.path().join("sources");
        let install_root = tmp.path().join("install");
        let list_src = et_shaped_thornlist(&sources, "MyTree");

        let plan = plan_for(&list_src, &install_root);

        assert!(!plan.git.is_empty(), "the ET-shaped list must plan at least one repo");
        let expected_repos_dir = install_root.join("MyTree").join("repos");
        for repo in &plan.git {
            assert!(
                repo.dir.starts_with(&expected_repos_dir),
                "{} planned at {}, not under {}",
                repo.repo,
                repo.dir.display(),
                expected_repos_dir.display()
            );
            assert!(
                !repo.dir.starts_with(install_root.join("Cactus")),
                "{} must not be planned under Cactus/ when the declared root is MyTree",
                repo.repo
            );
        }
    }

    /// Regression pin for the split-installation bug's execution half:
    /// `execute()` must actually materialize repos, the flesh, and the
    /// arrangement symlinks under the declared root — and must not leave a
    /// stray `Cactus/` behind, which is the literal symptom a user saw (the
    /// live thornlist and convenience symlink in `Cactus/`, the fetched
    /// sources in `MyTree/`, one installation split across two directories).
    #[test]
    fn a_custom_root_places_the_whole_tree_under_it() {
        let tmp = tempfile::tempdir().unwrap();
        let sources = tmp.path().join("sources");
        let install_root = tmp.path().join("install");
        let list_src = et_shaped_thornlist(&sources, "MyTree");

        let plan = plan_for(&list_src, &install_root);
        let report = execute(&plan, &install_root).unwrap();
        assert!(report.failures.is_empty(), "unexpected failures: {:?}", report.failures);

        assert!(install_root.join("MyTree/repos/core").is_dir());
        assert!(install_root.join("MyTree/repos/cactusbase").is_dir());
        assert_tree_lands_under_root(&install_root, "MyTree");
        assert_no_cactus_dir_anywhere(&install_root);
    }

    /// Control case: the identical ET-shaped list, but with the ordinary
    /// `!DEFINE ROOT = Cactus`, must still place everything under `Cactus/`
    /// — proving the assertions above track the thornlist's declared root
    /// rather than passing vacuously (e.g. because the helper always leaves
    /// something under `Cactus/` regardless of what's asserted).
    #[test]
    fn the_stock_cactus_root_still_places_everything_under_cactus() {
        let tmp = tempfile::tempdir().unwrap();
        let sources = tmp.path().join("sources");
        let install_root = tmp.path().join("install");
        let list_src = et_shaped_thornlist(&sources, "Cactus");

        let plan = plan_for(&list_src, &install_root);
        let report = execute(&plan, &install_root).unwrap();
        assert!(report.failures.is_empty(), "unexpected failures: {:?}", report.failures);

        assert_tree_lands_under_root(&install_root, "Cactus");
    }

    /// A `!DEFINE ROOT` with more than one path component (upstream allows
    /// this) must still be honored in full — `install_root.join(root)` has to
    /// handle a multi-component root exactly like a single-component one, not
    /// just happen to work for the common case.
    #[test]
    fn a_multi_component_root_lands_under_its_full_nested_path() {
        let tmp = tempfile::tempdir().unwrap();
        let sources = tmp.path().join("sources");
        let install_root = tmp.path().join("install");
        let list_src = et_shaped_thornlist(&sources, "a/b");

        let plan = plan_for(&list_src, &install_root);
        let report = execute(&plan, &install_root).unwrap();
        assert!(report.failures.is_empty(), "unexpected failures: {:?}", report.failures);

        assert_tree_lands_under_root(&install_root, "a/b");
        assert_no_cactus_dir_anywhere(&install_root);
    }
}
