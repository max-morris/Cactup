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
    /// modified files up and moves them into `git` via [`Plan::force`].
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
    /// Move every skipped repo into the fetchable set (after the caller has
    /// backed up its modified files). Clean-vs-wanted state decides the
    /// action the same way `plan` does for clean repos.
    pub fn force(&mut self) {
        for s in self.skipped.drain(..) {
            let action = match &s.branch {
                Some(b) if git::at_local_origin_tip(&s.dir, b) => GitAction::Update,
                _ => GitAction::Align,
            };
            self.git.push(GitRepoPlan {
                repo: s.repo,
                dir: s.dir,
                url: s.url,
                branch: s.branch,
                action,
                checkouts: s.checkouts,
                forced: Some(s.reason.describe()),
            });
        }
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
                        let name = match (&item.action, item.branch.as_deref()) {
                            (GitAction::Clone, _) => format!("clone {}", item.repo),
                            (GitAction::Update, _) => format!("update {}", item.repo),
                            (GitAction::Align, Some(branch)) => {
                                format!("switch {} to {branch}", item.repo)
                            }
                            (GitAction::Align, None) => format!("switch {}", item.repo),
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

    // Externals sequentially: they spawn system tools that may talk to the
    // terminal, and none occur in the real ET list anyway.
    for c in &plan.external {
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

/// Probe every repo this thornlist names. `Ok(None)` only when the tree holds
/// no inspectable repo at all — callers must read that as "no information",
/// never as "nothing changed". A repo that fails to probe is left out rather
/// than guessed at.
pub fn source_heads(install_root: &Path, list: &Thornlist) -> Res<Option<SourceHeads>> {
    let repos_dir = install_root.join(list.root()).join("repos");
    let mut out = SourceHeads::default();
    for c in list.components() {
        if out.flesh.is_none() && is_flesh(list.root(), c) {
            out.flesh = Some(c.repo.clone());
        }
        if out.heads.contains_key(&c.repo) {
            continue;
        }
        let dir = repos_dir.join(&c.repo);
        if !dir.is_dir() {
            continue;
        }
        if let Ok(state) = git::source_state(&dir) {
            if state.contains("+") {
                out.dirty.insert(c.repo.clone());
            }
            out.heads.insert(c.repo.clone(), state);
        }
    }
    if out.heads.is_empty() {
        return Ok(None);
    }
    Ok(Some(out))
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
/// repo count once the group map is built, then incremented as each repo is
/// probed — a plain gix status walk per repo, which on ~80 repos takes long
/// enough that without this the command looks hung before anything appears.
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

    for (repo, group) in git_groups {
        let _current = progress.add_child(repo.clone());
        let dir = repos_dir.join(&repo);
        let mut probe = git::probe(&dir, &group.url, group.branch.as_deref().unwrap_or(""));
        if let RepoState::Dirty(DirtyReason::BranchSwitched { head, .. }) = &probe.state
            && fetch_state
                .as_ref()
                .and_then(|s| s.repos.get(&repo))
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
        match probe.state {
            RepoState::Absent => plan.git.push(GitRepoPlan {
                repo,
                dir,
                url: group.url,
                branch: group.branch,
                action: GitAction::Clone,
                checkouts: group.checkouts,
                forced: None,
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
        progress.inc();
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
        let got = source_heads(root, &list).unwrap().unwrap();

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
        let after = source_heads(root, &list).unwrap().unwrap();
        assert_ne!(after.heads["core"], before);
        assert_eq!(after.heads["cactusbase"], got.heads["cactusbase"], "untouched repos hold still");
    }

    #[test]
    fn source_heads_is_none_when_nothing_is_inspectable() {
        let tmp = tempfile::tempdir().unwrap();
        let list = crate::thornlist::parse(LIST).unwrap();
        // No repos on disk at all: no information, which callers must not read
        // as "nothing changed".
        assert!(source_heads(tmp.path(), &list).unwrap().is_none());
    }

    #[test]
    fn committed_splits_the_worktree_summary_off() {
        assert_eq!(committed("abc123"), "abc123");
        assert_eq!(committed("abc123+2mod@1700"), "abc123");
    }
}
