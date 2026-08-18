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
///  2. our own per-component headline ("clone foo", "download bar", ...)
///  3. gix's phase status line ("negotiate (round N)", "receiving pack", ...)
///  4. gix's per-phase bars (remote, read pack, create index file, checkout,
///     writing) — the actually-informative detail (byte counts, server
///     "remote" counts)
///  5+. per-thread delta-resolution/decoding noise — not useful, hidden
const PROGRESS_MAX_LEVEL: prodash::progress::key::Level = 4;

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
}

#[derive(Debug)]
pub struct GitRepoPlan {
    pub repo: String,
    /// Absolute repo directory: `<install_root>/<root>/repos/<repo>`.
    pub dir: PathBuf,
    pub url: String,
    /// `None` = the remote's default branch (no `!REPO_BRANCH`).
    pub branch: Option<String>,
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
            let action = if retarget.is_some() {
                GitAction::Align
            } else {
                match &s.branch {
                    Some(b) if git::at_local_origin_tip(&s.dir, b) => GitAction::Update,
                    _ => GitAction::Align,
                }
            };
            // A skipped repo with no !REPO_BRANCH has `branch: None`; unlike
            // `plan()`'s clean-repo path, nothing has resolved it to the
            // repo's actual head branch yet. Do that here too, or `execute`
            // errors out with "no branch resolved for existing repo" for any
            // dirty repo that was never given an explicit branch.
            let branch = s.branch.or_else(|| git::head_of(&s.dir).ok().map(|(b, _)| b));
            moved.push(s.repo.clone());
            self.git.push(GitRepoPlan {
                repo: s.repo,
                dir: s.dir,
                url: s.url,
                branch,
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
    pub failures: Vec<Failure>,
}

#[derive(Debug)]
pub struct RepoResult {
    pub repo: String,
    pub url: String,
    pub branch: Option<String>,
    /// The commit HEAD resolved to after the action (hex).
    pub head: String,
    /// Whether the action moved HEAD (false = already up to date).
    pub changed: bool,
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

impl ExecReport {
    /// The `fetch-state.toml` records for everything successfully fetched.
    pub fn records(&self) -> Vec<(String, RepoRecord)> {
        self.repos
            .iter()
            .map(|r| {
                (
                    r.repo.clone(),
                    RepoRecord { url: r.url.clone(), branch: r.branch.clone(), head: r.head.clone() },
                )
            })
            .collect()
    }
}

/// Run the plan: git repos and downloads on a [`WORKERS`]-wide pool keyed by
/// repo (one repo is only ever touched by one worker), externals
/// sequentially, then the symlink pass. Progress renders via the crate's
/// prodash line renderer, four levels deep (see [`PROGRESS_MAX_LEVEL`]): an
/// overall "fetch components" bar (level 1) counts finished items; each
/// in-flight git work item gets a stable headline naming it (level 2, e.g.
/// "clone foo") plus a child gix actually writes to (level 3/4 — see the
/// worker loop for why those are split); downloads get one child (level 2)
/// with bytes progress. Errors are collected per item, never fatal to the
/// rest of the fetch, but a failing item's headline is left as a permanent
/// red line before it's dropped, so a failure is visible live and not just
/// in the final report.
pub fn execute(plan: &Plan, install_root: &Path) -> Res<ExecReport> {
    enum Work<'p> {
        Git(&'p GitRepoPlan),
        Download(&'p Component),
    }

    let (progress, renderer) = crate::manifest::setup_prodash_with(
        Some(crate::manifest::progress_level_filter(PROGRESS_MAX_LEVEL)),
        true,
    );

    let top = progress.add_child("fetch components");
    top.init(Some(plan.git.len() + plan.downloads.len()), Some(prodash::unit::label("components")));
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
                        let name = if let Some(url) = &item.retarget {
                            // Distinct headline: this item's `origin` is about
                            // to be rewritten before anything is fetched, not
                            // a normal clone/update/switch.
                            format!("repoint {} at {url}", item.repo)
                        } else {
                            match (&item.action, item.branch.as_deref()) {
                                (GitAction::Clone, _) => format!("clone {}", item.repo),
                                (GitAction::Update, _) => format!("update {}", item.repo),
                                (GitAction::Align, Some(branch)) => {
                                    format!("switch {} to {branch}", item.repo)
                                }
                                (GitAction::Align, None) => format!("switch {}", item.repo),
                            }
                        };
                        let mut header =
                            top.lock().expect("fetch progress poisoned").add_child(name);
                        // gix renames whatever item it's given as the fetch
                        // moves through phases ("negotiate (round N)",
                        // "receiving pack", ...) — it cannot carry our own
                        // "clone/update/switch <repo>" headline, so that
                        // headline lives one level above the item we hand
                        // gix, which is never displayed directly.
                        let mut gix_item = header.add_child("connecting");

                        // Rewrite `origin` before anything else touches the
                        // repo: `align` (below) re-opens it and must see the
                        // thornlist's URL, not the stale one the probe
                        // flagged. A failure here is reported and dropped
                        // exactly like an align/clone failure — no fetch is
                        // attempted for an item whose remote we could not fix.
                        if let Some(url) = &item.retarget {
                            if let Err(e) = git::set_origin_url(&item.dir, url) {
                                header.fail(format!("{}: {e:#}", item.repo));
                                report.lock().expect("fetch report poisoned").failures.push(Failure {
                                    what: item.repo.clone(),
                                    error: format!("{e:#}"),
                                });
                                drop(gix_item);
                                drop(header);
                                top.lock().expect("fetch progress poisoned").inc();
                                continue;
                            }
                        }
                        let before = match item.action {
                            GitAction::Clone => None,
                            _ => git::head_of(&item.dir).ok().map(|(_, id)| id),
                        };
                        let outcome = match (&item.action, item.branch.as_deref()) {
                            (GitAction::Clone, branch) => {
                                git::clone(&item.url, branch, &item.dir, &mut gix_item)
                                    .and_then(|()| git::head_of(&item.dir).map(|(_, id)| id))
                            }
                            (_, Some(branch)) => git::align(&item.dir, branch, &mut gix_item),
                            (_, None) => {
                                // plan() always fills in the probed head
                                // branch for existing repos; this is a bug
                                // guard, not a reachable path.
                                Err(anyhow::anyhow!("no branch resolved for existing repo"))
                            }
                        };
                        let mut report = report.lock().expect("fetch report poisoned");
                        match outcome {
                            Ok(head) => report.repos.push(RepoResult {
                                repo: item.repo.clone(),
                                url: item.url.clone(),
                                branch: item.branch.clone(),
                                head: head.to_string(),
                                changed: before != Some(head),
                                forced: item.forced.clone(),
                                retargeted: item.retarget.clone(),
                            }),
                            Err(e) => {
                                header.fail(format!("{}: {e:#}", item.repo));
                                report.failures.push(Failure {
                                    what: item.repo.clone(),
                                    error: format!("{e:#}"),
                                });
                            }
                        }
                        drop(report);
                        drop(gix_item);
                        drop(header);
                        top.lock().expect("fetch progress poisoned").inc();
                    }
                    Work::Download(c) => {
                        let mut child = top
                            .lock()
                            .expect("fetch progress poisoned")
                            .add_child(format!("download {}", c.checkout));
                        let outcome = download::download_component(install_root, c, &mut child);
                        let mut report = report.lock().expect("fetch report poisoned");
                        match outcome {
                            Ok(path) => report.downloads.push(path),
                            Err(e) => {
                                child.fail(format!("{}: {e:#}", c.checkout));
                                report.failures.push(Failure {
                                    what: c.checkout.clone(),
                                    error: format!("{e:#}"),
                                });
                            }
                        }
                        drop(report);
                        drop(child);
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
                .push(Failure { what, error: "interrupted before this component was fetched".into() });
        }
        return Ok(report);
    }

    // Externals sequentially: they spawn system tools that may talk to the
    // terminal, and none occur in the real ET list anyway.
    for c in &plan.external {
        if gix::interrupt::is_triggered() {
            report
                .failures
                .push(Failure { what: c.checkout.clone(), error: "interrupted".into() });
            continue;
        }
        if let Err(e) = external::fetch_external(install_root, c) {
            report.failures.push(Failure { what: c.checkout.clone(), error: format!("{e:#}") });
        }
    }

    // Symlink pass, after every fetch: repoint-or-create per checkout, and
    // assert each fetched repo actually landed on its expected branch.
    // Plain targets first: a target containing `..` may deliberately route
    // *through* an arrangement symlink another component creates (the ET
    // list's Fuka section does exactly this), so those resolve last.
    let mut links: Vec<&Component> = plan.links.iter().collect();
    links.sort_by_key(|c| c.target.contains("..") || c.checkout.contains(".."));
    for c in links {
        match link::link_component(install_root, &plan.root, c) {
            Ok(outcome) => report.links.push((c.checkout.clone(), outcome)),
            Err(e) => {
                report.failures.push(Failure { what: c.checkout.clone(), error: format!("{e:#}") })
            }
        }
    }
    for r in &report.repos {
        if let Some(expected) = &r.branch
            && let Ok((actual, _)) = git::head_of(&install_root.join(&plan.root).join("repos").join(&r.repo))
            && &actual != expected
        {
            report.failures.push(Failure {
                what: r.repo.clone(),
                error: format!("post-fetch HEAD is on {actual}, expected {expected}"),
            });
        }
    }

    Ok(report)
}

/// `<install_root>/.cactup/fetch-state.toml` — the per-repo record of what
/// cactup itself fetched (URL, branch, resolved HEAD). Prune only ever
/// removes repos this file (or the current thornlist) accounts for, and it
/// is the input a future rebuild-decision hookup will diff (§7.5).
#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct FetchState {
    pub schema: u32,
    #[serde(default)]
    pub repos: indexmap::IndexMap<String, RepoRecord>,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct RepoRecord {
    pub url: String,
    pub branch: Option<String>,
    /// The commit HEAD resolved to when cactup last fetched this repo.
    pub head: String,
}

impl FetchState {
    pub const SCHEMA: u32 = 1;

    fn path(install_root: &Path) -> PathBuf {
        install_root.join(".cactup").join("fetch-state.toml")
    }

    /// `Ok(None)` when no fetch has ever recorded state (pre-existing
    /// installations) — callers must treat that as "no record", never as
    /// "nothing was fetched by cactup".
    pub fn read(install_root: &Path) -> Res<Option<FetchState>> {
        let path = Self::path(install_root);
        let text = match std::fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            other => other.with_context(|| format!("Failed to read {}", path.display()))?,
        };
        let state: FetchState = toml::from_str(&text)
            .with_context(|| format!("Failed to parse {}", path.display()))?;
        Ok(Some(state))
    }

    /// Merge `records` over the existing state (repos not touched by this
    /// fetch keep their previous record) and write atomically.
    pub fn record(install_root: &Path, records: &[(String, RepoRecord)]) -> Res<()> {
        let mut state = Self::read(install_root)?
            .unwrap_or_else(|| FetchState { schema: Self::SCHEMA, repos: Default::default() });
        for (repo, record) in records {
            state.repos.insert(
                repo.clone(),
                RepoRecord {
                    url: record.url.clone(),
                    branch: record.branch.clone(),
                    head: record.head.clone(),
                },
            );
        }
        let path = Self::path(install_root);
        let dir = path.parent().expect("fetch-state path has a parent");
        std::fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create {}", dir.display()))?;
        let text = toml::to_string_pretty(&state).with_context(|| "Failed to render fetch state")?;
        let tmp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("Failed to create temp file in {}", dir.display()))?;
        std::fs::write(tmp.path(), text)
            .with_context(|| "Failed to write fetch state temp file")?;
        tmp.persist(&path)
            .with_context(|| format!("Failed to replace {}", path.display()))?;
        Ok(())
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
/// finish. `Ok(None)` only when the tree holds no readable repo at all —
/// callers must read that as "no information", never as "nothing changed".
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
) -> Res<Option<SourceHeads>> {
    let repos_dir = install_root.join(list.root()).join("repos");
    let mut out = SourceHeads::default();
    let mut seen = std::collections::BTreeSet::new();
    let mut repos: Vec<String> = Vec::new();
    for c in list.components() {
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
        let state = git::source_state(&repos_dir.join(repo)).ok();
        drop(current);
        progress.lock().expect("source_heads progress poisoned").inc();
        state
    })?;
    for (repo, state) in repos.into_iter().zip(states) {
        let Some(state) = state else {
            out.unreadable.insert(repo);
            continue;
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
        return Ok(None);
    }
    Ok(Some(out))
}

/// [`source_heads`] behind its own phase-scoped line renderer, for callers
/// that carry no renderer of their own (the submit/run divergence warning,
/// `config delta`, the build's rebuild decision). The renderer's 500ms
/// initial delay means a tree that probes quickly never flashes a bar, and
/// on a non-tty stderr (job logs, pipes) there is no renderer at all.
pub fn source_heads_with_progress(
    install_root: &Path,
    list: &Thornlist,
) -> Res<Option<SourceHeads>> {
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

/// Classify every component of `list` against the tree at `install_root`
/// (the directory that contains `<root>/`, i.e. the installation root).
/// Read-only; the network is never touched. `progress` is init'ed to the
/// repo count once the group map is built, then incremented as each repo's
/// probe — a plain gix status walk — finishes. The probes run on the
/// [`crate::par`] pool: even so, on ~80 repos they take long enough that
/// without progress the command looks hung before anything appears.
pub fn plan(list: &Thornlist, install_root: &Path, progress: &mut prodash::tree::Item) -> Res<Plan> {
    let root = list.root().to_owned();
    let repos_dir = install_root.join(&root).join("repos");

    // Group git components by repo directory; the repo, not the thorn, is
    // the unit of fetching and of dirtiness.
    struct Group<'c> {
        url: String,
        branch: Option<String>,
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
                                checkouts: vec![c.checkout.clone()],
                                first: c,
                            },
                        );
                    }
                    Some(group) => {
                        // Two sections naming one repo dir must agree, or the
                        // fetch would be order-dependent.
                        if group.url != url || group.branch != c.branch {
                            bail!(
                                "thornlist names repo {} twice with conflicting \
                                 sources: {} @ {} (for {}) vs {} @ {} (for {})",
                                c.repo,
                                group.url,
                                group.branch.as_deref().unwrap_or("<default>"),
                                group.first.checkout,
                                url,
                                c.branch.as_deref().unwrap_or("<default>"),
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
        let current = progress.lock().expect("plan progress poisoned").add_child(repo.clone());
        let dir = repos_dir.join(repo);
        let mut probe = git::probe(&dir, &group.url, group.branch.as_deref().unwrap_or(""));
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

    for ((repo, group), probe) in groups.into_iter().zip(probes) {
        let dir = repos_dir.join(&repo);
        match probe.state {
            RepoState::Absent => plan.git.push(GitRepoPlan {
                repo,
                dir,
                url: group.url,
                branch: group.branch,
                action: GitAction::Clone,
                checkouts: group.checkouts,
                forced: None,
                retarget: None,
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
                    action,
                    checkouts: group.checkouts,
                    forced: None,
                    retarget: None,
                });
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
mod tests {
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
        assert!(source_heads(tmp.path(), &list, &mut progress).unwrap().is_none());
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
}
