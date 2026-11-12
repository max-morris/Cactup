//! Native component fetcher (spec §3.2): planner/executor over the parsed
//! thornlist. git via gix, http(s)/ftp via reqwest, svn/cvs/hg/darcs via the
//! system tools.
//!
//! Implemented by the FETCH stream (design/_impl_fetch.md).

// Consumed by refetch/install in Phases 4/5; the allows come off then.
pub mod download;
pub mod external;
pub mod git;
pub mod link;
pub(crate) mod retry;

use crate::format::{self, Format, Toml};
use crate::thornlist::{Component, ComponentType, Thornlist};
use crate::Res;
use anyhow::{bail, Context};
use git::{DirtyReason, RepoState};
use std::collections::BTreeMap;
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
    /// The thornlist names no branch and `branch` is the remote's default
    /// (with `branch: None`, `execute` asks the remote which that is):
    /// recorded, so a plan that trusts the record need not ask again (see
    /// [`DefaultBranches`]).
    pub follows_default: bool,
    /// `branch` is the thornlist's `!REPO_BRANCH`: recorded, so a later
    /// plan knows cactup put the repo on it (see
    /// [`RepoRecord::named_branch`]).
    pub named_branch: bool,
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
}

#[derive(Debug)]
pub struct SkippedRepo {
    pub repo: String,
    pub dir: PathBuf,
    pub url: String,
    pub branch: Option<String>,
    /// As [`GitRepoPlan::follows_default`], for when the repo is forced.
    /// With `branch: None` and no pin, forcing the repo sends it to the
    /// remote's default branch: the plan found it on a branch cactup put it
    /// on, or could not learn the default (see [`Plan::force_where`]).
    pub follows_default: bool,
    /// As [`GitRepoPlan::named_branch`], for when the repo is forced.
    pub named_branch: bool,
    /// The `!REPO_COMMIT` pin, if any: forcing this repo pins it.
    pub commit: Option<gix::ObjectId>,
    pub reason: DirtyReason,
    pub untracked: Vec<String>,
    pub checkouts: Vec<String>,
}

impl Plan {
    /// Move every skipped repo into the fetchable set. See [`Plan::force_where`].
    pub fn force(&mut self) -> Vec<String> {
        self.force_where(|_| true)
    }

    /// Move the skipped repos `select` accepts into the fetchable set (after
    /// the caller has backed up their modified files), leaving the rest
    /// skipped. Returns the names moved, in plan order.
    pub fn force_where(&mut self, select: impl Fn(&SkippedRepo) -> bool) -> Vec<String> {
        let mut moved = Vec::new();
        // `drain` with a filter would leave the remaining `skipped` reordered
        // (retain-while-draining semantics); take the whole vec instead and
        // rebuild both parts in their original relative order.
        let taken = std::mem::take(&mut self.skipped);
        for s in taken {
            if !select(&s) {
                self.skipped.push(s);
                continue;
            }
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
            // A skipped repo with no !REPO_BRANCH has `branch: None`; unlike
            // `plan()`'s clean-repo path, nothing has resolved it yet. The
            // plan marks one `follows_default` when it sits on a branch
            // cactup put it on, or when the remote would not name its
            // default: forced, it goes to the remote's default (`execute`
            // asks), as an unforced fetch of the clean repo would — staying
            // put would record a branch no later plan could vouch for, and
            // skip it again. On any other branch, the user's, it stays there
            // (the next plan asks for the default), unless origin has no
            // such branch (one made locally): aligning to it would fail
            // every time, so it goes to the default too. Detached — a
            // branchless pin the list dropped, say — it has no branch to
            // stay on, and goes to the default. A HEAD that cannot be read
            // (not a repo, an unborn branch: a probe that failed) is
            // neither: forcing it onto the default would be a guess about a
            // repo nothing could inspect, so it goes with no branch and no
            // default, and `execute` fails it, naming why. A pinned repo
            // needs no branch: it is checked out by commit.
            let (branch, follows_default, named_branch) = match (s.commit, s.branch) {
                (Some(_), branch) => (branch, false, s.named_branch),
                (None, Some(branch)) => (Some(branch), s.follows_default, s.named_branch),
                (None, None) if s.follows_default => (None, true, false),
                (None, None) => match git::head_of(&s.dir) {
                    Ok((head, _)) if head == git::DETACHED => (None, true, false),
                    Ok((head, _)) if !git::tracks_origin(&s.dir, &head) => (None, true, false),
                    Ok((head, _)) => (Some(head), false, false),
                    Err(_) => (None, false, false),
                },
            };
            moved.push(s.repo.clone());
            self.git.push(GitRepoPlan {
                repo: s.repo,
                dir: s.dir,
                url: s.url,
                branch,
                follows_default,
                named_branch,
                commit: s.commit,
                action,
                checkouts: s.checkouts,
                forced: Some(s.reason.describe()),
                retarget,
            });
        }
        moved
    }
}

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
    /// The branch the repo is on after the action.
    pub branch: Option<String>,
    /// `branch` is the remote's default, followed because the thornlist
    /// names none (see [`RepoRecord::follows_default`]).
    pub follows_default: bool,
    /// `branch` is the one the thornlist names (see
    /// [`RepoRecord::named_branch`]).
    pub named_branch: bool,
    /// The commit HEAD resolved to after the action (hex).
    pub head: String,
    /// The pin this repo was fetched to, if any (see [`RepoRecord::commit`]).
    pub commit: Option<gix::ObjectId>,
    /// Whether the action moved HEAD (false = already up to date).
    pub changed: bool,
    /// The branch a pin took HEAD off, if it was on one: a change to the
    /// checkout even when the commit is the same, so it is said (and counted
    /// in the summary) though the sources did not move.
    pub detached: Option<String>,
    /// Carried through from the plan: why this repo needed forcing, if it did.
    pub forced: Option<String>,
    /// `Some(url)` when this fetch re-pointed the repo's `origin` at the
    /// thornlist URL — a persistent change to the user's repo, so it is
    /// reported unconditionally, not only under `--verbose`.
    pub retargeted: Option<String>,
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
                        follows_default: r.follows_default,
                        named_branch: r.named_branch,
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
/// `branch` is the one the repo went to: the item's, or the default branch
/// `execute` asked the remote for. `off` is the branch HEAD left, said even
/// when the commit is the same: one a pin took HEAD off (a pin always
/// detaches, and leaving a branch HEAD was on is a change to the user's
/// checkout, said in yellow), or the one an align switched away from.
fn announce_repo(
    line: &crate::progress::Line,
    item: &GitRepoPlan,
    branch: Option<&str>,
    head: gix::ObjectId,
    changed: bool,
    off: Option<&str>,
) {
    match announce_text(item, branch, head, changed, off) {
        Some((what, true)) => line.warned(what),
        Some((what, false)) => line.succeeded(what),
        None => {}
    }
}

/// [`announce_repo`]'s line, and whether it is a warning; `None` for a repo
/// that leaves no line.
fn announce_text(
    item: &GitRepoPlan,
    branch: Option<&str>,
    head: gix::ObjectId,
    changed: bool,
    off: Option<&str>,
) -> Option<(String, bool)> {
    let head = head.to_string();
    let short = &head[..12.min(head.len())];
    let mut what = match (&item.action, branch) {
        (GitAction::Clone, _) if item.commit.is_some() => format!("cloned, pinned at {short}"),
        (GitAction::Pin, _) if changed => format!("pinned at {short}"),
        (GitAction::Pin, _) if off.is_some() => format!("detached at the pin {short}"),
        (GitAction::Clone, Some(branch)) => format!("cloned {branch} at {short}"),
        (GitAction::Clone, None) => format!("cloned at {short}"),
        // A branch switch is news even when the commit is the same.
        (_, Some(branch)) if changed || off.is_some() => format!("{branch} now at {short}"),
        (_, None) if changed => format!("now at {short}"),
        // Unchanged, but something else about the repo was: say what, since
        // the fetch itself has nothing to report.
        _ if item.forced.is_some() || item.retarget.is_some() => {
            format!("already at {short}")
        }
        _ => return None,
    };
    if let Some(off) = off {
        what.push_str(&format!(", off {off}"));
    }
    Some(match (&item.forced, &item.retarget) {
        (Some(reason), _) => (format!("{what}; overwrote {reason}"), true),
        (None, Some(url)) => (format!("{what}; origin re-pointed at {url}"), true),
        (None, None) => (what, matches!(item.action, GitAction::Pin) && off.is_some()),
    })
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
                            _ => git::head_of(&item.dir).ok(),
                        };
                        // An existing repo with neither a branch nor a pin
                        // (forced off a detached HEAD, or detached by a
                        // branchless pin the list dropped): it goes to the
                        // remote's default branch, which only the remote
                        // knows. A clone without a branch lands there on its
                        // own. Not following the default either, it is one
                        // forced with a HEAD nothing could read (see
                        // `Plan::force_where`): failed, with that reason.
                        let branch = match (&item.action, &item.branch, item.commit) {
                            (GitAction::Update | GitAction::Align, None, None)
                                if !item.follows_default =>
                            {
                                Err(match git::head_of(&item.dir) {
                                    Err(e) => {
                                        e.context("Its HEAD cannot be read: no branch to fetch it on")
                                    }
                                    Ok(_) => anyhow::anyhow!("No branch resolved for existing repo"),
                                })
                            }
                            (GitAction::Update | GitAction::Align, None, None) => {
                                line.phase("asking for the default branch".to_string());
                                let asked = git::ask_remote_default_branch(Some(&item.dir), &item.url);
                                asked.map(|default| {
                                    line.phase(format!("switching to {default}"));
                                    Some(default)
                                })
                            }
                            _ => Ok(item.branch.clone()),
                        };
                        let (branch, asked) = match branch {
                            Ok(branch) => (branch, Ok(())),
                            Err(e) => (None, Err(e)),
                        };
                        // Forced (backed-up local edits) or repointed: only
                        // a full overwrite is right there.
                        let force = item.forced.is_some() || item.retarget.is_some();
                        let wanted = (&item.action, branch.as_deref(), item.commit);
                        let outcome = asked.and_then(|()| match wanted {
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
                                // Resolved just above for every existing
                                // repo; this is a bug guard, not a reachable
                                // path.
                                Err(anyhow::anyhow!("No branch resolved for existing repo"))
                            }
                        });
                        // Assert the repo actually landed on its expected
                        // branch: an independent read of HEAD on disk, taken
                        // here (outside the report lock) so it runs in
                        // parallel with the other workers.
                        // A pinned repo has no branch to be on: it must sit at
                        // its commit instead.
                        let wrong_branch = match (&outcome, &branch, item.commit) {
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
                        // A clone without a branch lands on the remote's
                        // default: record which branch that is.
                        let landed = match (&outcome, &branch) {
                            (Ok(_), None) if item.commit.is_none() => {
                                git::head_of(&item.dir).ok().map(|(b, _)| b)
                            }
                            _ => branch.clone(),
                        };
                        let mut report = report.lock().expect("fetch report poisoned");
                        match outcome {
                            Ok(head) => {
                                let changed = before.as_ref().map(|(_, id)| *id) != Some(head);
                                // The branch HEAD left, if it left one: a pin
                                // leaves HEAD detached, wherever it was; an
                                // align may switch branches.
                                let off = match (&item.action, &before) {
                                    (_, Some((was, _))) if was == git::DETACHED => None,
                                    (GitAction::Pin, Some((was, _))) => Some(was.clone()),
                                    (_, Some((was, _))) if branch.as_ref() != Some(was) => {
                                        Some(was.clone())
                                    }
                                    _ => None,
                                };
                                let detached =
                                    off.clone().filter(|_| matches!(item.action, GitAction::Pin));
                                let off = off.as_deref();
                                announce_repo(&line, item, branch.as_deref(), head, changed, off);
                                report.repos.push(RepoResult {
                                    repo: item.repo.clone(),
                                    url: item.url.clone(),
                                    branch: landed,
                                    follows_default: item.follows_default,
                                    named_branch: item.named_branch,
                                    head: head.to_string(),
                                    commit: item.commit,
                                    changed,
                                    detached,
                                    forced: item.forced.clone(),
                                    retargeted: item.retarget.clone(),
                                });
                                if let Some(error) = wrong_branch {
                                    report.failures.push(Failure { what: item.repo.clone(), error });
                                }
                            }
                            Err(e) => {
                                line.failed(format!("{e:#}"));
                                report.failures.push(Failure {
                                    what: item.repo.clone(),
                                    error: format!("{e:#}"),
                                });
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

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RepoRecord {
    pub url: String,
    pub branch: Option<String>,
    /// The thornlist named no branch and `branch` was the remote's default.
    /// Only a plan that trusts the record ([`DefaultBranches::Recorded`])
    /// reads it, to spare asking the remote again; `refetch` always asks.
    /// Missing in records written before it existed: such a plan then asks
    /// the remote once. Additive (spec §2.5): an older binary ignores it, and
    /// a record it rewrites without it costs one question to the remote.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub follows_default: bool,
    /// `branch` is the one the thornlist named (`!REPO_BRANCH`). With
    /// `follows_default`, how a plan knows cactup put the repo on `branch`
    /// and may move it on when the list changes. Missing in records written
    /// before it existed, which cannot tell a branch an earlier list named
    /// from one the user checked out (an older cactup recorded whatever
    /// branch a repo with no `!REPO_BRANCH` was on): such a record is read
    /// as not saying, and the branch as the user's. Additive (spec §2.5):
    /// an older binary ignores it, and a record it rewrites without it costs
    /// the repo its claim: a later list that moves it off that branch (drops
    /// `!REPO_BRANCH`, or names another branch the repo has locally) skips
    /// it once as a branch switch, until `--overwrite` moves it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub named_branch: bool,
    /// The commit HEAD resolved to when cactup last fetched this repo.
    pub head: String,
    /// The `!REPO_COMMIT` that fetch pinned the repo to, if any. A pinned
    /// repo sits detached at it, so `branch` is only the label it was taken
    /// from — not a branch HEAD is expected to be on.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
}

impl RepoRecord {
    /// Whether cactup put the repo on `head_branch`: its last fetch left it
    /// there unpinned (a pinned repo sits detached; a branch HEAD is on
    /// after a pin is the user's, whatever branch the pin's record names),
    /// on the branch the thornlist named or on the remote's default. A
    /// record that cannot say (an older cactup's) is read as "no".
    fn put_there(&self, head_branch: &str) -> bool {
        self.commit.is_none()
            && (self.named_branch || self.follows_default)
            && self.branch.as_deref() == Some(head_branch)
    }
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
                    follows_default: record.follows_default,
                    named_branch: record.named_branch,
                    head: record.head.clone(),
                    commit: record.commit.clone(),
                },
            );
        }
        // The records made anew above: put-back must not reach into them. A
        // repo name with a dot cannot be named in a dotted pattern, so a
        // write that touches one replaces every record whole.
        let patterns: Vec<String> = if records.repos.iter().any(|(repo, _)| repo.contains('.')) {
            vec!["repos.*".to_owned()]
        } else {
            records.repos.iter().map(|(repo, _)| format!("repos.{repo}")).collect()
        };
        let whole: Vec<&str> = patterns.iter().map(String::as_str).collect();
        format::write_replacing(&Self::path(install_root), &state, &FETCH_STATE, &whole)
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
fn is_flesh(root: &str, c: &Component) -> bool {
    c.target.trim_end_matches('/') == root.trim_end_matches('/')
        && matches!(c.checkout.as_str(), "Makefile" | "lib" | "src")
}

/// How a repo's source reads in a conflict message: its branch, and its pin.
fn source_label(branch: Option<&str>, commit: Option<&str>) -> String {
    let branch = branch.unwrap_or("<default>");
    match commit {
        Some(commit) => format!("{branch} pinned at {commit}"),
        None => branch.to_owned(),
    }
}

/// How [`plan`] learns the default branch of a repo whose thornlist entry
/// names no `!REPO_BRANCH`: only its remote knows it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultBranches {
    /// Ask every such remote. What `refetch` does, so a remote that moved its
    /// default branch — or renamed, or deleted, the one the repo is on — is
    /// followed by the next refetch.
    Ask,
    /// Take the fetch record's word for a repo it found on its remote's
    /// default branch, and ask only about the rest: for commands that fetch
    /// a few repos of the list (`add`) or clone it (`install`), where a
    /// round trip per repo they do not fetch would be all cost.
    Recorded,
}

/// What asking a remote for its default branch came to, in [`plan`].
enum Asked {
    /// Nothing to ask: the list names a branch or a pin, or the repo is not
    /// cactup's to move.
    Nothing,
    /// The remote's answer (or the record's, when the plan trusts it).
    Default(String),
    /// The remote did not answer; why.
    Failed(String),
}

/// Classify every component of `list` against the tree at `install_root`
/// (the directory that contains `<root>/`, i.e. the installation root).
/// Read-only. The network is touched only to ask a remote for its default
/// branch, for a repo whose thornlist entry names no branch and no pin and
/// that cactup may move: a clean one, or one detached where cactup's last
/// fetch left it. `defaults` says whether the fetch record can spare that
/// question. A clean repo whose remote does not answer stays where it is,
/// with a warning on its line of `progress`. `progress` is init'ed to the
/// repo count once the group map is built, then incremented as each repo's
/// probe — a plain gix status walk, plus that question where there is one —
/// finishes. The probes run on the
/// [`crate::par`] pool: even so, on ~80 repos they take long enough that
/// without progress the command looks hung before anything appears.
pub fn plan(
    list: &Thornlist,
    install_root: &Path,
    defaults: DefaultBranches,
    progress: &mut prodash::tree::Item,
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

    for c in list.components() {
        match c.ty {
            ComponentType::Git => {
                let url = c
                    .url
                    .clone()
                    .with_context(|| format!("git component {} has no !URL", c.checkout))?;
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
                links.push(c.clone());
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

    let mut plan = Plan {
        git: Vec::new(),
        skipped: Vec::new(),
        downloads,
        external,
        links,
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
        let mut current = progress.lock().expect("plan progress poisoned").add_child(repo.clone());
        let dir = repos_dir.join(repo);
        // A pinned repo is checked out by commit, so the branch it happens to
        // be on is no reason to skip it — only edits and local commits are.
        // Probing against "no branch" checks exactly those.
        let wanted_branch = if group.commit.is_some() { "" } else { group.branch.as_deref().unwrap_or("") };
        let mut probe = git::probe(&dir, &group.url, wanted_branch);
        if let RepoState::Dirty(DirtyReason::BranchSwitched { head, .. }) = &probe.state
            && fetch_state.as_ref().and_then(|s| s.repos.get(repo)).is_some_and(|r| r.put_there(head))
        {
            // cactup's own doing (not a branch checked out after a pin, nor
            // one a record cannot vouch for: see `put_there`). Re-probe
            // against the current branch to check the repo is otherwise
            // clean; if so, plan the align.
            let reprobe = git::probe(&dir, &group.url, head);
            if let RepoState::Clean { .. } = reprobe.state {
                probe = git::Probe {
                    state: RepoState::Clean { head_branch: head.clone() },
                    untracked: reprobe.untracked,
                };
            }
        }
        // No !REPO_BRANCH means the remote's default branch, which only the
        // remote knows: ask it, from the clone, whose `origin` is the
        // thornlist's URL (the probe classifies any other as changed). A
        // pinned repo sits at its commit, with no branch to follow; a repo
        // detached anywhere but where cactup's last fetch left it is the
        // user's, skipped whatever the default is. Asked here on the probe
        // pool, not on `execute`'s fetch workers, because the plan needs the
        // answer to choose the repo's action; each question is one ref
        // advertisement, abandoned on Ctrl-C like the probes around it.
        let record = fetch_state.as_ref().and_then(|s| s.repos.get(repo));
        let ask = || match git::ask_remote_default_branch(Some(&dir), &group.url) {
            Ok(default) => Asked::Default(default),
            Err(e) => Asked::Failed(format!("{e:#}")),
        };
        let default = match (&probe.state, &group.branch) {
            _ if group.commit.is_some() => Asked::Nothing,
            (RepoState::Clean { head_branch }, None) => {
                let recorded = record.is_some_and(|r| r.follows_default && r.put_there(head_branch));
                if defaults == DefaultBranches::Recorded && recorded {
                    Asked::Default(head_branch.clone())
                } else {
                    let asked = ask();
                    // Said here, on the repo's own line, since nothing else
                    // would, when the plan keeps the repo where it is and
                    // updates it (below). One on a branch cactup cannot
                    // vouch for is skipped instead, and its skip says why.
                    if let Asked::Failed(why) = &asked
                        && !gix::interrupt::is_triggered()
                        && record.is_some_and(|r| r.put_there(head_branch))
                    {
                        use colored::Colorize;
                        let warning = format!(
                            "could not ask for its default branch, so it stays on {head_branch}: {why}"
                        );
                        current.info(warning.yellow().to_string());
                    }
                    asked
                }
            }
            // No warning on a failure here: `execute` asks again, and fails
            // the repo with its own error if the remote still does not say.
            (RepoState::Detached { head }, None)
                if record.is_some_and(|r| r.head == head.to_string()) =>
            {
                ask()
            }
            _ => Asked::Nothing,
        };
        drop(current);
        progress.lock().expect("plan progress poisoned").inc();
        (probe, default)
    })?;

    for ((repo, group), (probe, default)) in groups.into_iter().zip(probes) {
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
                follows_default: group.branch.is_none() && commit.is_none(),
                named_branch: group.branch.is_some(),
                branch: group.branch,
                commit,
                action: GitAction::Clone,
                checkouts: group.checkouts,
                forced: None,
                retarget: None,
            }),
            RepoState::Clean { .. } if commit.is_some() => plan.git.push(GitRepoPlan {
                repo,
                dir,
                url: group.url,
                named_branch: group.branch.is_some(),
                branch: group.branch,
                follows_default: false,
                commit,
                action: GitAction::Pin,
                checkouts: group.checkouts,
                forced: None,
                retarget: None,
            }),
            RepoState::Clean { head_branch } => {
                let (wanted, follows_default, named_branch) = match (group.branch, default) {
                    (Some(branch), _) => (branch, false, true),
                    (None, Asked::Default(default)) => (default, true, false),
                    // The remote did not say. On a branch cactup put it on
                    // (the probe warned): stay there, following what the
                    // record says it follows, so that one unanswered
                    // question changes nothing. The fetch goes to the same
                    // remote and reports the failure; the next plan asks
                    // again. On any other branch, the user's, an update is
                    // not known to be what the list asks (were the default
                    // elsewhere, the repo would be skipped as a branch
                    // switch): skipped, the branch left alone. Forced, it
                    // goes to the default, which `execute` asks again.
                    (None, asked) => match record.filter(|r| r.put_there(&head_branch)) {
                        Some(r) => (head_branch.clone(), r.follows_default, r.named_branch),
                        None => {
                            let why = match asked {
                                Asked::Failed(why) => why,
                                _ => "nothing was asked".to_owned(),
                            };
                            plan.skipped.push(SkippedRepo {
                                repo,
                                dir,
                                url: group.url,
                                branch: None,
                                follows_default: true,
                                named_branch: false,
                                commit: None,
                                reason: DirtyReason::DefaultUnknown { head: head_branch, why },
                                untracked: probe.untracked,
                                checkouts: group.checkouts,
                            });
                            continue;
                        }
                    },
                };
                // Off the default branch with no !REPO_BRANCH: switch only if
                // cactup put the repo there (an earlier thornlist named that
                // branch, or it was the default). A branch the user checked
                // out is theirs to keep, as with a named branch — and so is
                // one a record cannot vouch for (see `put_there`).
                let ours = record.is_some_and(|r| r.put_there(&head_branch));
                if follows_default && head_branch != wanted && !ours {
                    plan.skipped.push(SkippedRepo {
                        repo,
                        dir,
                        url: group.url,
                        branch: Some(wanted.clone()),
                        follows_default,
                        named_branch,
                        commit: None,
                        reason: DirtyReason::BranchSwitched { head: head_branch, expected: wanted },
                        untracked: probe.untracked,
                        checkouts: group.checkouts,
                    });
                    continue;
                }
                let action =
                    if head_branch == wanted { GitAction::Update } else { GitAction::Align };
                plan.git.push(GitRepoPlan {
                    repo,
                    dir,
                    url: group.url,
                    branch: Some(wanted),
                    follows_default,
                    named_branch,
                    commit: None,
                    action,
                    checkouts: group.checkouts,
                    forced: None,
                    retarget: None,
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
                let action = match commit {
                    Some(pin) if pin == head || ours => {
                        Some((GitAction::Pin, group.branch.clone(), false))
                    }
                    // Off the pin, onto the branch the list names; with none,
                    // the remote's default. When the remote did not say,
                    // `execute` asks it again, and fails the repo if it
                    // still cannot: the branch the pin was taken from is not
                    // what the list asks for.
                    None if ours => Some(match (group.branch.clone(), default) {
                        (Some(branch), _) => (GitAction::Align, Some(branch), false),
                        (None, Asked::Default(default)) => (GitAction::Align, Some(default), true),
                        (None, _) => (GitAction::Align, None, true),
                    }),
                    _ => None,
                };
                match action {
                    Some((action, branch, follows_default)) => plan.git.push(GitRepoPlan {
                        repo,
                        dir,
                        url: group.url,
                        named_branch: group.branch.is_some(),
                        branch,
                        follows_default,
                        commit,
                        action,
                        checkouts: group.checkouts,
                        forced: None,
                        retarget: None,
                    }),
                    None => plan.skipped.push(SkippedRepo {
                        repo,
                        dir,
                        url: group.url,
                        named_branch: group.branch.is_some(),
                        branch: group.branch,
                        follows_default: false,
                        commit,
                        reason: DirtyReason::DetachedHead(head),
                        untracked: probe.untracked,
                        checkouts: group.checkouts,
                    }),
                }
            }
            RepoState::Dirty(reason) => {
                // BranchSwitched against "" happens only when no !REPO_BRANCH
                // is set; that cannot occur (probe compares against the head
                // branch then), but guard the invariant anyway. With no
                // !REPO_BRANCH and no pin, a repo on a branch cactup put it
                // on (local edits, say, or a re-pointed `origin`) is still
                // cactup's: forcing it goes to the remote's default, as the
                // list asks. Any other branch is the user's: forced, it stays
                // there, unless origin lacks it; detached, it goes to the
                // default; a HEAD that cannot be read fails (see
                // `force_where`). The next plan asks for the default.
                let vouched = |r: &RepoRecord| git::head_of(&dir).is_ok_and(|(head, _)| r.put_there(&head));
                let ours = group.branch.is_none() && commit.is_none() && record.is_some_and(vouched);
                plan.skipped.push(SkippedRepo {
                    repo,
                    dir,
                    url: group.url,
                    named_branch: group.branch.is_some(),
                    branch: group.branch,
                    follows_default: ours,
                    commit,
                    reason,
                    untracked: probe.untracked,
                    checkouts: group.checkouts,
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
            follows_default: false,
            named_branch: branch.is_some(),
            commit: None,
            reason: DirtyReason::WorktreeModified(vec!["src/foo.c".to_string()]),
            untracked: Vec::new(),
            checkouts: vec![repo.to_string()],
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
        // A clone has origin's branch: plant it in A, whose branch a forced
        // refetch then keeps (one origin lacks goes to the default instead).
        let (a_branch, a_head) = git::head_of(&a_dir).unwrap();
        let a_tracking = format!("refs/remotes/origin/{a_branch}");
        gix::open(&a_dir)
            .unwrap()
            .reference(a_tracking.as_str(), a_head, gix::refs::transaction::PreviousValue::Any, "test")
            .unwrap();

        let mut plan = Plan {
            git: Vec::new(),
            skipped: vec![skipped_repo(a_dir.clone(), "A", None), skipped_repo(b_dir.clone(), "B", Some("main"))],
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            root: "Cactus".to_string(),
        };

        let moved = plan.force_where(|s| s.repo == "A");
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
            follows_default: false,
            named_branch: true,
            commit: None,
            reason: DirtyReason::RemoteUrlChanged {
                on_disk: "https://old.example.invalid/repo.git".to_string(),
                wanted: wanted_url.clone(),
                modified: Vec::new(),
            },
            untracked: Vec::new(),
            checkouts: vec!["R".to_string()],
        };
        let mut plan = Plan {
            git: Vec::new(),
            skipped: vec![skipped],
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            root: "Cactus".to_string(),
        };

        let moved = plan.force_where(|_| true);
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
            git: Vec::new(),
            skipped: vec![skipped_repo(dir.clone(), "R", Some("main"))],
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            root: "Cactus".to_string(),
        };
        plan.force_where(|_| true);
        assert!(matches!(plan.git[0].action, GitAction::Update));
        assert_eq!(plan.git[0].retarget, None);
    }

    /// Forced with no branch named and a HEAD nothing can read (here, an
    /// unborn branch): never guessed onto the remote's default, the repo
    /// fails, saying why.
    #[test]
    fn a_forced_repo_whose_head_cannot_be_read_fails_saying_why() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("Cactus/repos/R");
        std::fs::create_dir_all(&dir).unwrap();
        gix::init(&dir).unwrap();
        let mut skipped = skipped_repo(dir.clone(), "R", None);
        skipped.reason = DirtyReason::Unknown("HEAD points at no commit".to_owned());
        let mut plan = Plan {
            git: Vec::new(),
            skipped: vec![skipped],
            downloads: Vec::new(),
            external: Vec::new(),
            links: Vec::new(),
            root: "Cactus".to_string(),
        };

        plan.force();
        assert_eq!(plan.git[0].branch, None);
        assert!(!plan.git[0].follows_default);
        let report = execute(&plan, tmp.path()).unwrap();
        assert!(report.repos.is_empty(), "{:?}", report.repos);
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        assert_eq!(report.failures[0].what, "R");
        let error = &report.failures[0].error;
        assert!(error.starts_with("Its HEAD cannot be read: no branch to fetch it on: "), "{error}");
    }

    /// An upstream at `<tmp>/upstream` whose default branch is one commit
    /// past a second branch `rel`. Returns the upstream, `rel`'s commit, the
    /// default branch and its tip.
    fn upstream_with_a_release_branch(tmp: &Path) -> (PathBuf, gix::ObjectId, String, gix::ObjectId) {
        let upstream = tmp.join("upstream");
        git::testrepo::init(&upstream);
        let release = git::testrepo::commit(&upstream, "release");
        gix::open(&upstream)
            .unwrap()
            .reference("refs/heads/rel", release, gix::refs::transaction::PreviousValue::Any, "test")
            .unwrap();
        let tip = git::testrepo::commit(&upstream, "after the release");
        let (default, _) = git::head_of(&upstream).unwrap();
        assert_ne!(default, "rel");
        (upstream, release, default, tip)
    }

    /// An [`upstream_with_a_release_branch`], and an installation whose repo
    /// `R` was cloned on `rel` — what a thornlist naming `!REPO_BRANCH = rel`
    /// leaves behind. Returns the installation root, the upstream, its
    /// default branch and its tip.
    fn cloned_on_a_release_branch(tmp: &Path) -> (PathBuf, PathBuf, String, gix::ObjectId) {
        let (upstream, _, default, tip) = upstream_with_a_release_branch(tmp);

        let root = tmp.join("install");
        let mut progress = prodash::tree::Root::new().add_child("test clone");
        git::clone(&upstream.to_string_lossy(), Some("rel"), &root.join("Cactus/repos/R"), &mut progress)
            .unwrap();
        (root, upstream, default, tip)
    }

    /// `R` with no `!REPO_BRANCH`: the remote's default branch.
    fn list_without_a_branch(upstream: &Path) -> String {
        list_naming(upstream, None, None)
    }

    /// `R` on `branch` (none: the remote's default), pinned at `pin` if any.
    fn list_naming(upstream: &Path, branch: Option<&str>, pin: Option<gix::ObjectId>) -> String {
        let branch = branch.map(|b| format!("!REPO_BRANCH = {b}\n")).unwrap_or_default();
        let pin = pin.map(|c| format!("!REPO_COMMIT = {c}\n")).unwrap_or_default();
        format!(
            "!CRL_VERSION = 1.0\n\
             !DEFINE ROOT = Cactus\n\n\
             !TARGET   = $ROOT\n\
             !TYPE     = git\n\
             !URL      = {}\n\
             {branch}{pin}\
             !NAME     = R\n\
             !CHECKOUT = Makefile\n",
            upstream.to_string_lossy()
        )
    }

    /// Plan `list_src` as `refetch` does, run it, insist nothing failed, and
    /// record it: one refetch.
    fn refetch_once(list_src: &str, root: &Path) -> (Plan, ExecReport) {
        let plan = plan_for(list_src, root);
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
        let report = execute(&plan, root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        FetchState::record(root, &report.records()).unwrap();
        (plan, report)
    }

    /// Point `dir`'s HEAD at its branch `branch`, as a checkout would,
    /// without touching the worktree or the index.
    fn point_head_at(dir: &Path, branch: &str) {
        use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit};
        let target = gix::refs::Target::Symbolic(format!("refs/heads/{branch}").try_into().unwrap());
        gix::open(dir)
            .unwrap()
            .edit_reference(RefEdit {
                change: Change::Update {
                    log: LogChange::default(),
                    expected: PreviousValue::Any,
                    new: target,
                },
                name: "HEAD".try_into().unwrap(),
                deref: false,
            })
            .unwrap();
    }

    /// An [`upstream_with_a_release_branch`], and an installation whose repo
    /// `R` cactup cloned pinned at `rel`'s commit with no `!REPO_BRANCH` —
    /// what `cactup add owner/name#<sha>` and `sim export` write — and
    /// recorded. Returns the installation root, the upstream, its default
    /// branch, the pin and the default's tip.
    fn cloned_on_a_branchless_pin(
        tmp: &Path,
    ) -> (PathBuf, PathBuf, String, gix::ObjectId, gix::ObjectId) {
        let (upstream, release, default, tip) = upstream_with_a_release_branch(tmp);
        let root = tmp.join("install");
        refetch_once(&list_naming(&upstream, None, Some(release)), &root);
        (root, upstream, default, release, tip)
    }

    /// A pin with no `!REPO_BRANCH` is cloned detached at its commit, and its
    /// record names no branch: there is none to follow while it is pinned.
    #[test]
    fn a_branchless_pin_is_cloned_with_no_branch_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        git::testrepo::init(&upstream);
        let pin = git::testrepo::commit(&upstream, "pinned");
        git::testrepo::commit(&upstream, "after the pin");
        let root = tmp.path().join("install");

        let (plan, _) = refetch_once(&list_naming(&upstream, None, Some(pin)), &root);
        assert!(matches!(plan.git[0].action, GitAction::Clone));
        assert!(!plan.git[0].follows_default);
        assert_eq!(git::head_of(&root.join("Cactus/repos/R")).unwrap(), (git::DETACHED.to_owned(), pin));
        let record = &FetchState::read(&root).unwrap().unwrap().repos["R"];
        assert_eq!(record.branch, None);
        assert!(!record.follows_default);
        assert_eq!(record.commit.as_deref(), Some(pin.to_string().as_str()));
    }

    /// Dropping a branchless pin puts the repo on the remote's default
    /// branch, in one refetch: it is cactup's detached HEAD to move, and the
    /// default is the branch the list now asks for.
    #[test]
    fn a_branchless_pin_dropped_goes_to_the_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, default, _, tip) = cloned_on_a_branchless_pin(tmp.path());

        let (plan, _) = refetch_once(&list_without_a_branch(&upstream), &root);
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(plan.git[0].branch.as_deref(), Some(default.as_str()));
        assert!(plan.git[0].follows_default);
        assert_eq!(git::head_of(&root.join("Cactus/repos/R")).unwrap(), (default.clone(), tip));
        let record = &FetchState::read(&root).unwrap().unwrap().repos["R"];
        assert_eq!((record.branch.as_deref(), record.follows_default), (Some(default.as_str()), true));
        assert_eq!(record.commit, None);
    }

    /// The same repo moved off its pin by hand is the user's: skipped. Forced
    /// (`refetch --overwrite R`), with no branch to stay on, it goes to the
    /// remote's default — `execute` asks — never to an error.
    #[test]
    fn a_forced_branchless_repo_off_its_pin_goes_to_the_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, default, _, tip) = cloned_on_a_branchless_pin(tmp.path());
        let dir = root.join("Cactus/repos/R");
        let mut progress = prodash::tree::Root::new().add_child("test checkout");
        git::pin(&dir, None, tip, false, &mut progress).unwrap();

        let mut plan = plan_for(&list_without_a_branch(&upstream), &root);
        assert!(matches!(plan.skipped[0].reason, DirtyReason::DetachedHead(id) if id == tip));
        plan.force_where(|s| s.repo == "R");
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(plan.git[0].branch, None);
        assert!(plan.git[0].follows_default);
        let report = execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(git::head_of(&dir).unwrap(), (default.clone(), tip));
        let records = report.records();
        assert_eq!(records.repos[0].1.branch.as_deref(), Some(default.as_str()));
        assert!(records.repos[0].1.follows_default);
    }

    /// Dropping a pin and its `!REPO_BRANCH` together reaches the remote's
    /// default in one refetch, not the branch the pin was taken from.
    #[test]
    fn a_pin_and_its_branch_dropped_together_reach_the_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, release, default, tip) = upstream_with_a_release_branch(tmp.path());
        let root = tmp.path().join("install");
        refetch_once(&list_naming(&upstream, Some("rel"), Some(release)), &root);

        let (plan, _) = refetch_once(&list_without_a_branch(&upstream), &root);
        assert_eq!(plan.git[0].branch.as_deref(), Some(default.as_str()));
        assert!(plan.git[0].follows_default);
        assert_eq!(git::head_of(&root.join("Cactus/repos/R")).unwrap(), (default.clone(), tip));
    }

    /// A clean repo on a branch, newly pinned with no `!REPO_BRANCH`: pinned,
    /// following no branch.
    #[test]
    fn a_clean_repo_newly_pinned_without_a_branch_is_pinned() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, _, tip) = cloned_on_a_release_branch(tmp.path());
        record_fetch(&root, &upstream, "rel", false, true);

        let plan = plan_for(&list_naming(&upstream, None, Some(tip)), &root);
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
        assert!(matches!(plan.git[0].action, GitAction::Pin));
        assert_eq!(plan.git[0].branch, None);
        assert!(!plan.git[0].follows_default);
    }

    /// A list names a branch, then pins the commit the repo already sits at:
    /// the pin moves nothing, but leaves HEAD detached, as every pin does.
    /// Dropping both later is then cactup's detached HEAD to move, to the
    /// remote's default, not a branch switch the user has to force.
    #[test]
    fn a_pin_at_the_head_detaches_it_and_dropping_it_reaches_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, release, default, tip) = upstream_with_a_release_branch(tmp.path());
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        refetch_once(&list_naming(&upstream, Some("rel"), None), &root);
        assert_eq!(git::head_of(&dir).unwrap(), ("rel".to_owned(), release));

        let (plan, report) = refetch_once(&list_naming(&upstream, Some("rel"), Some(release)), &root);
        assert!(matches!(plan.git[0].action, GitAction::Pin));
        assert!(!report.repos[0].changed, "the pin moved nothing");
        assert_eq!(report.repos[0].detached.as_deref(), Some("rel"), "but HEAD left rel");
        assert_eq!(git::head_of(&dir).unwrap(), (git::DETACHED.to_owned(), release));

        let (plan, _) = refetch_once(&list_without_a_branch(&upstream), &root);
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(git::head_of(&dir).unwrap(), (default, tip));
    }

    /// A branch the user checked out after a branchless pin is theirs: the
    /// pin's record names no branch, and that must not read as "cactup's".
    #[test]
    fn a_branch_checked_out_after_a_branchless_pin_is_skipped() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, default, release, _) = cloned_on_a_branchless_pin(tmp.path());
        let dir = root.join("Cactus/repos/R");
        // `git checkout -b rel --track origin/rel`, at the pin's own commit.
        let repo = gix::open(&dir).unwrap();
        for name in ["refs/heads/rel", "refs/remotes/origin/rel"] {
            repo.reference(name, release, gix::refs::transaction::PreviousValue::Any, "test").unwrap();
        }
        point_head_at(&dir, "rel");

        let plan = plan_for(&list_without_a_branch(&upstream), &root);
        assert!(plan.git.is_empty(), "{:?}", plan.git);
        match &plan.skipped[0].reason {
            DirtyReason::BranchSwitched { head, expected } => {
                assert_eq!((head.as_str(), expected.as_str()), ("rel", default.as_str()));
            }
            other => panic!("expected BranchSwitched, got {other:?}"),
        }
    }

    /// A repo that follows its remote's default keeps following it: when the
    /// remote moves its default to another branch, and when it then renames
    /// that branch away, the next refetch moves the repo along. A plan that
    /// trusts the record (`add`) keeps it where the record says, offline.
    #[test]
    fn a_followed_default_branch_that_moves_is_followed_by_the_next_refetch() {
        let tmp = tempfile::tempdir().unwrap();
        let upstream = tmp.path().join("upstream");
        git::testrepo::init(&upstream);
        git::testrepo::commit(&upstream, "first");
        let (first, base) = git::head_of(&upstream).unwrap();
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        let list = list_without_a_branch(&upstream);
        let (plan, _) = refetch_once(&list, &root);
        assert!(plan.git[0].follows_default);
        assert_eq!(git::head_of(&dir).unwrap().0, first);

        // The remote's default moves to a new branch; the old one stays.
        let upstream_repo = gix::open(&upstream).unwrap();
        use gix::refs::transaction::PreviousValue::Any;
        upstream_repo.reference("refs/heads/next", base, Any, "test").unwrap();
        point_head_at(&upstream, "next");
        let next_tip = git::testrepo::commit(&upstream, "on next");
        let recorded = plan_with(&list, &root, DefaultBranches::Recorded);
        assert!(matches!(recorded.git[0].action, GitAction::Update));
        assert_eq!(recorded.git[0].branch.as_deref(), Some(first.as_str()));
        let (plan, _) = refetch_once(&list, &root);
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(git::head_of(&dir).unwrap(), ("next".to_owned(), next_tip));

        // The remote renames that branch: the old name is gone.
        upstream_repo.reference("refs/heads/trunk", next_tip, Any, "test").unwrap();
        point_head_at(&upstream, "trunk");
        upstream_repo.find_reference("refs/heads/next").unwrap().delete().unwrap();
        let (plan, _) = refetch_once(&list, &root);
        assert_eq!(plan.git[0].branch.as_deref(), Some("trunk"));
        assert_eq!(git::head_of(&dir).unwrap(), ("trunk".to_owned(), next_tip));
        assert!(FetchState::read(&root).unwrap().unwrap().repos["R"].follows_default);
    }

    /// Record a fetch of `R` that left it on `branch`, unpinned: the
    /// remote's default when `follows_default`, the list's `!REPO_BRANCH`
    /// when `named_branch`, and when neither, a record an older cactup
    /// wrote, which cannot say which.
    fn record_fetch(
        root: &Path,
        upstream: &Path,
        branch: &str,
        follows_default: bool,
        named_branch: bool,
    ) {
        let record = RepoRecord {
            url: upstream.to_string_lossy().into_owned(),
            branch: Some(branch.to_string()),
            follows_default,
            named_branch,
            head: String::new(),
            commit: None,
        };
        FetchState::record(root, &FetchRecords { repos: vec![("R".to_string(), record)], downloads: Vec::new() })
            .unwrap();
    }

    /// Dropping `!REPO_BRANCH` from a thornlist moves the repo back to the
    /// remote's default branch, when an earlier fetch put it on the old one:
    /// its record says the list named that branch.
    #[test]
    fn a_dropped_repo_branch_moves_the_repo_to_the_default_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, release, default, tip) = upstream_with_a_release_branch(tmp.path());
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        refetch_once(&list_naming(&upstream, Some("rel"), None), &root);
        assert_eq!(git::head_of(&dir).unwrap(), ("rel".to_owned(), release));
        let record = &FetchState::read(&root).unwrap().unwrap().repos["R"];
        assert_eq!((record.branch.as_deref(), record.named_branch), (Some("rel"), true));
        assert!(!record.follows_default);

        let (plan, _) = refetch_once(&list_without_a_branch(&upstream), &root);
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(plan.git[0].branch.as_deref(), Some(default.as_str()));
        assert!(plan.git[0].follows_default);
        assert_eq!(git::head_of(&dir).unwrap(), (default.clone(), tip));
        let record = &FetchState::read(&root).unwrap().unwrap().repos["R"];
        assert_eq!((record.branch.as_deref(), record.follows_default), (Some(default.as_str()), true));
        assert!(!record.named_branch);
    }

    /// A record an older cactup wrote names the branch the repo is on, but
    /// cannot say whether an earlier list named it or the user checked it
    /// out (`git checkout develop`): such a branch is the user's. Skipped as
    /// a branch switch, never moved silently; forced, it goes to the
    /// remote's default.
    #[test]
    fn a_branch_an_old_record_cannot_vouch_for_is_skipped_until_forced() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, default, tip) = cloned_on_a_release_branch(tmp.path());
        record_fetch(&root, &upstream, "rel", false, false);

        let mut plan = plan_for(&list_without_a_branch(&upstream), &root);
        assert!(plan.git.is_empty(), "{:?}", plan.git);
        match &plan.skipped[0].reason {
            DirtyReason::BranchSwitched { head, expected } => {
                assert_eq!((head.as_str(), expected.as_str()), ("rel", default.as_str()));
            }
            other => panic!("expected BranchSwitched, got {other:?}"),
        }

        plan.force_where(|s| s.repo == "R");
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(plan.git[0].branch.as_deref(), Some(default.as_str()));
        assert!(plan.git[0].follows_default);
        let report = execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(git::head_of(&root.join("Cactus/repos/R")).unwrap(), (default.clone(), tip));
        let records = report.records();
        assert_eq!(records.repos[0].1.branch.as_deref(), Some(default.as_str()));
        assert!(records.repos[0].1.follows_default);
    }

    /// A branch the user checked out after a pin is theirs, even when it is
    /// the branch the pin was taken from: a list that then names another
    /// branch, which the repo has locally, skips it as a branch switch.
    #[test]
    fn a_branch_checked_out_after_a_pin_is_not_switched_by_a_named_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, release, default, _) = upstream_with_a_release_branch(tmp.path());
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        refetch_once(&list_without_a_branch(&upstream), &root);
        refetch_once(&list_naming(&upstream, Some("rel"), Some(release)), &root);
        assert_eq!(git::head_of(&dir).unwrap(), (git::DETACHED.to_owned(), release));
        // `git checkout -b rel --track origin/rel`, at the pin's own commit.
        let repo = gix::open(&dir).unwrap();
        for name in ["refs/heads/rel", "refs/remotes/origin/rel"] {
            repo.reference(name, release, gix::refs::transaction::PreviousValue::Any, "test").unwrap();
        }
        point_head_at(&dir, "rel");

        let plan = plan_for(&list_naming(&upstream, Some(&default), None), &root);
        assert!(plan.git.is_empty(), "{:?}", plan.git);
        match &plan.skipped[0].reason {
            DirtyReason::BranchSwitched { head, expected } => {
                assert_eq!((head.as_str(), expected.as_str()), ("rel", default.as_str()));
            }
            other => panic!("expected BranchSwitched, got {other:?}"),
        }
    }

    /// A branch the user checked out by hand is theirs, as with a named
    /// branch: skipped, and a forced refetch switches it to the default.
    #[test]
    fn a_branch_the_user_checked_out_is_skipped_not_switched() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, default, _) = cloned_on_a_release_branch(tmp.path());
        record_fetch(&root, &upstream, &default, true, false);

        let mut plan = plan_for(&list_without_a_branch(&upstream), &root);
        assert!(plan.git.is_empty());
        assert_eq!(plan.skipped.len(), 1);
        match &plan.skipped[0].reason {
            DirtyReason::BranchSwitched { head, expected } => {
                assert_eq!((head.as_str(), expected.as_str()), ("rel", default.as_str()));
            }
            other => panic!("expected BranchSwitched, got {other:?}"),
        }
        plan.force();
        assert_eq!(plan.git[0].branch.as_deref(), Some(default.as_str()));
        assert!(plan.git[0].follows_default);
    }

    /// A plan that trusts the record plans a repo the last fetch found on
    /// the default branch from the record alone: with the upstream gone,
    /// asking it would have failed.
    #[test]
    fn a_repo_already_on_the_default_branch_is_not_asked_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, _, _) = cloned_on_a_release_branch(tmp.path());
        record_fetch(&root, &upstream, "rel", true, false);
        std::fs::remove_dir_all(&upstream).unwrap();

        let plan = plan_with(&list_without_a_branch(&upstream), &root, DefaultBranches::Recorded);
        assert_eq!(plan.git.len(), 1);
        assert!(matches!(plan.git[0].action, GitAction::Update));
        assert_eq!(plan.git[0].branch.as_deref(), Some("rel"));
        assert!(plan.git[0].follows_default);
    }

    /// When the remote cannot say, a repo on a branch cactup put it on
    /// stays where it is, following what its record says it follows (one
    /// unanswered question changes nothing; the next plan asks again), and
    /// its line says why.
    #[test]
    fn an_unanswered_lookup_leaves_the_repo_on_its_branch() {
        // Followed as the default, or named by the list.
        for (followed, named) in [(true, false), (false, true)] {
            let tmp = tempfile::tempdir().unwrap();
            let (root, upstream, _, _) = cloned_on_a_release_branch(tmp.path());
            record_fetch(&root, &upstream, "rel", followed, named);
            std::fs::remove_dir_all(&upstream).unwrap();

            let (plan, messages) = plan_with_messages(&list_without_a_branch(&upstream), &root);
            assert_eq!(plan.git.len(), 1);
            assert!(matches!(plan.git[0].action, GitAction::Update));
            assert_eq!(plan.git[0].branch.as_deref(), Some("rel"));
            assert_eq!((plan.git[0].follows_default, plan.git[0].named_branch), (followed, named));
            let said = "could not ask for its default branch, so it stays on rel";
            assert!(matches!(messages.as_slice(), [m] if m.contains(said)), "{messages:?}");
        }
    }

    /// When the remote cannot say, a branch cactup cannot vouch for — the
    /// user's (the record names another), or one an older record names —
    /// is skipped, not updated: were the default elsewhere it would be a
    /// branch switch, so nothing moves it, and no fast-forward lands on it
    /// in the remote's stead. Its skip says why. Forced, it goes to the
    /// remote's default, which `execute` asks again: still unanswered, the
    /// repo fails, left where it was.
    #[test]
    fn an_unanswered_lookup_skips_a_branch_cactup_cannot_vouch_for() {
        for old_record in [false, true] {
            let tmp = tempfile::tempdir().unwrap();
            let (root, upstream, default, _) = cloned_on_a_release_branch(tmp.path());
            let dir = root.join("Cactus/repos/R");
            let before = git::head_of(&dir).unwrap();
            match old_record {
                true => record_fetch(&root, &upstream, "rel", false, false),
                false => record_fetch(&root, &upstream, &default, true, false),
            }
            std::fs::remove_dir_all(&upstream).unwrap();

            let (mut plan, messages) = plan_with_messages(&list_without_a_branch(&upstream), &root);
            assert!(plan.git.is_empty(), "{:?}", plan.git);
            assert!(messages.is_empty(), "the skip says it: {messages:?}");
            match &plan.skipped[0].reason {
                DirtyReason::DefaultUnknown { head, why } => {
                    assert_eq!(head, "rel");
                    assert!(why.contains(&*upstream.to_string_lossy()), "{why}");
                }
                other => panic!("expected DefaultUnknown, got {other:?}"),
            }
            let said = plan.skipped[0].reason.describe();
            assert!(said.contains("so rel is left alone"), "{said}");

            plan.force_where(|s| s.repo == "R");
            assert!(matches!(plan.git[0].action, GitAction::Align));
            assert_eq!(plan.git[0].branch, None);
            assert!(plan.git[0].follows_default);
            let report = execute(&plan, &root).unwrap();
            assert!(report.repos.is_empty(), "{:?}", report.repos);
            assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
            assert_eq!(git::head_of(&dir).unwrap(), before);
        }
    }

    /// Dropping a branchless pin while its remote is out of reach: the plan
    /// still moves cactup's detached HEAD (it does not blame the user, nor
    /// guess a branch), and the fetch fails the repo with the remote's own
    /// error, leaving it where it was.
    #[test]
    fn an_unreachable_remote_fails_a_dropped_branchless_pin_at_execute() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, _, pin, _) = cloned_on_a_branchless_pin(tmp.path());
        std::fs::remove_dir_all(&upstream).unwrap();

        let plan = plan_for(&list_without_a_branch(&upstream), &root);
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(plan.git[0].branch, None);
        assert!(plan.git[0].follows_default);
        let report = execute(&plan, &root).unwrap();
        assert!(report.repos.is_empty(), "{:?}", report.repos);
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        assert_eq!(report.failures[0].what, "R");
        let error = &report.failures[0].error;
        assert!(error.contains(&*upstream.to_string_lossy()), "{error}");
        let dir = root.join("Cactus/repos/R");
        assert_eq!(git::head_of(&dir).unwrap(), (git::DETACHED.to_owned(), pin));
    }

    /// The same, against a remote that answers but names no default branch
    /// (its HEAD is detached, so it advertises no symbolic HEAD): the fetch
    /// fails the repo saying so, and leaves it where it was.
    #[test]
    fn a_remote_that_names_no_default_fails_a_dropped_branchless_pin_at_execute() {
        let tmp = tempfile::tempdir().unwrap();
        let (root, upstream, _, pin, tip) = cloned_on_a_branchless_pin(tmp.path());
        let mut progress = prodash::tree::Root::new().add_child("test detach");
        git::pin(&upstream, None, tip, false, &mut progress).unwrap();
        assert_eq!(git::head_of(&upstream).unwrap().0, git::DETACHED);

        let plan = plan_for(&list_without_a_branch(&upstream), &root);
        assert!(plan.skipped.is_empty(), "{:?}", plan.skipped);
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(plan.git[0].branch, None);
        let report = execute(&plan, &root).unwrap();
        assert!(report.repos.is_empty(), "{:?}", report.repos);
        assert_eq!(report.failures.len(), 1, "{:?}", report.failures);
        let error = &report.failures[0].error;
        assert!(error.contains("does not say which branch is its default"), "{error}");
        let dir = root.join("Cactus/repos/R");
        assert_eq!(git::head_of(&dir).unwrap(), (git::DETACHED.to_owned(), pin));
    }

    /// An upstream at `<tmp>/upstream` like [`upstream_with_a_release_branch`]
    /// whose commits hold a tracked `Makefile`, so a clone can carry local
    /// edits. Returns the upstream, `rel`'s commit, the default branch and
    /// its tip.
    fn upstream_with_a_tracked_file(tmp: &Path) -> (PathBuf, gix::ObjectId, String, gix::ObjectId) {
        let upstream = tmp.join("upstream");
        git::testrepo::init(&upstream);
        let release = git::testrepo::commit_file(&upstream, "Makefile", "release\n");
        gix::open(&upstream)
            .unwrap()
            .reference("refs/heads/rel", release, gix::refs::transaction::PreviousValue::Any, "test")
            .unwrap();
        let tip = git::testrepo::commit_file(&upstream, "Makefile", "after the release\n");
        let (default, _) = git::head_of(&upstream).unwrap();
        assert_ne!(default, "rel");
        (upstream, release, default, tip)
    }

    /// A list names `rel`, then drops `!REPO_BRANCH` while the repo has
    /// local edits: skipped for them, and still cactup's branch. Forced, it
    /// goes to the remote's default in that one refetch — not left on `rel`
    /// under a record no later plan could vouch for, to be skipped again.
    #[test]
    fn a_dirty_repo_cactup_put_on_a_branch_is_forced_to_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, _, default, tip) = upstream_with_a_tracked_file(tmp.path());
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        refetch_once(&list_naming(&upstream, Some("rel"), None), &root);
        std::fs::write(dir.join("Makefile"), "edited\n").unwrap();

        let list = list_without_a_branch(&upstream);
        let mut plan = plan_for(&list, &root);
        assert!(matches!(plan.skipped[0].reason, DirtyReason::WorktreeModified(_)));
        assert!(plan.skipped[0].follows_default);
        plan.force_where(|s| s.repo == "R");
        assert!(matches!(plan.git[0].action, GitAction::Align));
        assert_eq!(plan.git[0].branch, None);
        assert!(plan.git[0].follows_default);
        let report = execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        FetchState::record(&root, &report.records()).unwrap();
        assert_eq!(git::head_of(&dir).unwrap(), (default.clone(), tip));
        let record = &FetchState::read(&root).unwrap().unwrap().repos["R"];
        assert_eq!((record.branch.as_deref(), record.follows_default), (Some(default.as_str()), true));

        let (plan, _) = refetch_once(&list, &root);
        assert!(matches!(plan.git[0].action, GitAction::Update));
    }

    /// The same edits on a branch the user checked out (the record names
    /// another): forced, the repo stays on the user's branch, and its record
    /// does not claim the branch for cactup.
    #[test]
    fn a_dirty_repo_on_the_users_branch_is_forced_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, release, default, _) = upstream_with_a_tracked_file(tmp.path());
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        let mut progress = prodash::tree::Root::new().add_child("test clone");
        git::clone(&upstream.to_string_lossy(), Some("rel"), &dir, &mut progress).unwrap();
        record_fetch(&root, &upstream, &default, true, false);
        std::fs::write(dir.join("Makefile"), "edited\n").unwrap();

        let mut plan = plan_for(&list_without_a_branch(&upstream), &root);
        assert!(matches!(plan.skipped[0].reason, DirtyReason::WorktreeModified(_)));
        assert!(!plan.skipped[0].follows_default);
        plan.force_where(|s| s.repo == "R");
        assert_eq!(plan.git[0].branch.as_deref(), Some("rel"));
        assert_eq!((plan.git[0].follows_default, plan.git[0].named_branch), (false, false));
        let report = execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(git::head_of(&dir).unwrap(), ("rel".to_owned(), release));
        let record = &report.records().repos[0].1;
        assert_eq!((record.follows_default, record.named_branch), (false, false));
    }

    /// A list names `rel`, then drops `!REPO_BRANCH` and is pointed at
    /// another repository: skipped for the new URL, and still cactup's
    /// branch. Forced, `origin` is re-pointed and the repo goes to the new
    /// remote's default, following it.
    #[test]
    fn a_retargeted_repo_cactup_put_on_a_branch_is_forced_to_the_new_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, _, default, _) = upstream_with_a_release_branch(tmp.path());
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        refetch_once(&list_naming(&upstream, Some("rel"), None), &root);
        let fork = tmp.path().join("fork");
        git::testrepo::init(&fork);
        let fork_tip = git::testrepo::commit(&fork, "fork");
        assert_eq!(git::head_of(&fork).unwrap().0, default);

        let mut plan = plan_for(&list_without_a_branch(&fork), &root);
        assert!(matches!(plan.skipped[0].reason, DirtyReason::RemoteUrlChanged { .. }));
        assert!(plan.skipped[0].follows_default);
        plan.force_where(|s| s.repo == "R");
        assert!(plan.git[0].retarget.is_some());
        assert_eq!(plan.git[0].branch, None);
        assert!(plan.git[0].follows_default);
        let report = execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(git::head_of(&dir).unwrap(), (default.clone(), fork_tip));
        let record = &report.records().repos[0].1;
        assert_eq!((record.branch.as_deref(), record.follows_default), (Some(default.as_str()), true));
    }

    /// A branch made locally, with no remote-tracking ref, in a repo whose
    /// list names no branch: the probe cannot inspect it against origin, and
    /// forced, it goes to the remote's default (aligning to a branch with no
    /// remote-tracking ref would fail every time). The local branch itself
    /// stays, with the work on it.
    #[test]
    fn a_forced_local_only_branch_goes_to_the_default() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, _, default, tip) = upstream_with_a_release_branch(tmp.path());
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        let list = list_without_a_branch(&upstream);
        refetch_once(&list, &root);
        // `git checkout -b topic`, and a commit on it.
        use gix::refs::transaction::PreviousValue::Any;
        gix::open(&dir).unwrap().reference("refs/heads/topic", tip, Any, "test").unwrap();
        point_head_at(&dir, "topic");
        let work = git::testrepo::commit(&dir, "work on topic");
        assert_ne!(work, tip);

        let mut plan = plan_for(&list, &root);
        assert!(matches!(plan.skipped[0].reason, DirtyReason::Unknown(_)), "{:?}", plan.skipped);
        plan.force_where(|s| s.repo == "R");
        assert_eq!(plan.git[0].branch, None);
        assert!(plan.git[0].follows_default);
        let report = execute(&plan, &root).unwrap();
        assert!(report.failures.is_empty(), "{:?}", report.failures);
        assert_eq!(git::head_of(&dir).unwrap(), (default, tip));
        let topic = gix::open(&dir).unwrap().find_reference("refs/heads/topic").unwrap().id().detach();
        assert_eq!(topic, work, "the work on topic is still there");
    }

    /// A forced align that switches branches says so even when the commit
    /// stays the same, naming the branch HEAD left, rather than "already at".
    #[test]
    fn a_branch_switch_at_the_same_commit_names_both_branches() {
        let head = gix::ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        let mut item = GitRepoPlan {
            repo: "R".to_owned(),
            dir: PathBuf::from("/nonexistent/R"),
            url: "https://example.invalid/R.git".to_owned(),
            branch: None,
            follows_default: true,
            named_branch: false,
            commit: None,
            action: GitAction::Align,
            checkouts: vec!["R/T".to_owned()],
            forced: Some("local modifications (Makefile)".to_owned()),
            retarget: None,
        };
        let line = |item: &GitRepoPlan, changed: bool, off: Option<&str>| {
            announce_text(item, Some("main"), head, changed, off)
        };
        let (what, warned) = line(&item, false, Some("topic")).unwrap();
        assert_eq!(what, "main now at 0123456789ab, off topic; overwrote local modifications (Makefile)");
        assert!(warned);
        // Moved too: the same, said once.
        assert_eq!(line(&item, true, Some("topic")).unwrap().0, what);
        // Same branch, same commit, forced: "already at", as before.
        let (what, _) = line(&item, false, None).unwrap();
        assert_eq!(what, "already at 0123456789ab; overwrote local modifications (Makefile)");
        // Unforced, a switch at the same commit still says so, in green.
        item.forced = None;
        let switched = ("main now at 0123456789ab, off topic".to_owned(), false);
        assert_eq!(line(&item, false, Some("topic")).unwrap(), switched);
        assert_eq!(line(&item, false, None), None);
    }

    /// A pin always detaches HEAD; off a branch, it says so, even when HEAD
    /// was already at the commit (here, the user's `git checkout` of the
    /// default branch, which happens to be at the pin): the repo's result
    /// names the branch it left, and the summary counts it.
    #[test]
    fn a_pin_that_detaches_a_branch_says_so() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, release, default, tip) = upstream_with_a_release_branch(tmp.path());
        let root = tmp.path().join("install");
        let dir = root.join("Cactus/repos/R");
        refetch_once(&list_without_a_branch(&upstream), &root);

        // At the pin already: nothing moves but HEAD, off the branch.
        let (plan, report) = refetch_once(&list_naming(&upstream, None, Some(tip)), &root);
        assert!(matches!(plan.git[0].action, GitAction::Pin));
        assert!(!report.repos[0].changed);
        assert_eq!(report.repos[0].detached.as_deref(), Some(default.as_str()));
        assert_eq!(git::head_of(&dir).unwrap(), (git::DETACHED.to_owned(), tip));

        // Already detached: a new pin leaves no branch.
        let (_, report) = refetch_once(&list_naming(&upstream, None, Some(release)), &root);
        assert!(report.repos[0].changed);
        assert_eq!(report.repos[0].detached, None);

        // Back on the branch, pinned elsewhere: moved, and off the branch.
        refetch_once(&list_without_a_branch(&upstream), &root);
        let (_, report) = refetch_once(&list_naming(&upstream, None, Some(release)), &root);
        assert!(report.repos[0].changed);
        assert_eq!(report.repos[0].detached.as_deref(), Some(default.as_str()));
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
        let mut plan = plan(&list, &root, DefaultBranches::Ask, &mut classify).unwrap();
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
        let moved = plan.force_where(|_| true);
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

    /// Plan `list_src` as `refetch` does: [`plan_with`], asking every remote.
    /// The shared first half of every placement case below.
    fn plan_for(list_src: &str, install_root: &Path) -> Plan {
        plan_with(list_src, install_root, DefaultBranches::Ask)
    }

    /// Parse `list_src` and run [`plan`] against it, asking the remotes as
    /// `defaults` says, with a throwaway progress sink.
    fn plan_with(list_src: &str, install_root: &Path, defaults: DefaultBranches) -> Plan {
        let list = crate::thornlist::parse(list_src).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        plan(&list, install_root, defaults, &mut progress).unwrap()
    }

    /// [`plan_for`], and the messages the plan left on its progress lines.
    fn plan_with_messages(list_src: &str, install_root: &Path) -> (Plan, Vec<String>) {
        let list = crate::thornlist::parse(list_src).unwrap();
        let root = prodash::tree::Root::new();
        let mut progress = root.add_child("test plan");
        let plan = plan(&list, install_root, DefaultBranches::Ask, &mut progress).unwrap();
        let mut messages = Vec::new();
        root.copy_messages(&mut messages);
        (plan, messages.into_iter().map(|m| m.message).collect())
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
            let planned = plan(list, &root, DefaultBranches::Ask, &mut progress).unwrap();
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
        let tip = RepoRecord {
            url: url.clone(),
            branch: Some(branch.clone()),
            follows_default: false,
            named_branch: true,
            head: c3.to_string(),
            commit: None,
        };
        FetchState::record(&root, &FetchRecords { repos: vec![("R".to_owned(), tip)], downloads: Vec::new() }).unwrap();
        let mut progress = prodash::tree::Root::new().add_child("test plan");
        let mut skipped = plan(&list(Some(&c3)), &root, DefaultBranches::Ask, &mut progress).unwrap();
        assert!(skipped.git.is_empty());
        assert!(matches!(skipped.skipped[0].reason, DirtyReason::DetachedHead(id) if id == c1));
        // Forcing it pins it all the same.
        skipped.force();
        assert!(matches!(skipped.git[0].action, GitAction::Pin));
    }

    fn exhaustive_fetch_state() -> FetchState {
        let mut repos = indexmap::IndexMap::new();
        // CarpetX is pinned on the branch the list names; Cactus names no
        // branch and follows the default.
        for (name, branch, pinned) in [("CarpetX", "main", true), ("Cactus", "master", false)] {
            repos.insert(
                name.to_owned(),
                RepoRecord {
                    url: format!("https://github.com/example/{name}.git"),
                    branch: Some(branch.to_owned()),
                    follows_default: !pinned,
                    named_branch: pinned,
                    head: "3f2c9e1a00112233445566778899aabbccddeeff".to_owned(),
                    commit: pinned.then(|| "3f2c9e1a00112233445566778899aabbccddeeff".to_owned()),
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
            follows_default: false,
            named_branch: true,
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
