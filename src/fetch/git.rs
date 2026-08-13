//! gix-backed git operations for the native fetcher (spec §3.2): the per-repo
//! dirtiness probe, depth-1 single-branch clone, and branch alignment.
//!
//! The fetcher never merges or rebases. Dirty repos are skipped by the planner
//! by definition, so [`align`] only ever has to make a *clean* worktree match
//! `origin/<branch>` — that property is what makes the gix-only (no external
//! `git`) choice viable.

use crate::Res;
use anyhow::{anyhow, Context};
use gix::bstr::{BString, ByteSlice};
use gix::refs::transaction::{Change, LogChange, PreviousValue, RefEdit, RefLog};
use gix::refs::Target;
use gix::remote::fetch::Shallow;
use gix::remote::Direction;
use gix::ObjectId;
use std::path::Path;

/// What the probe concluded about a repo directory on disk.
#[derive(Debug)]
pub enum RepoState {
    /// No directory: clone it.
    Absent,
    /// Safe to fetch/align; `head_branch` is the currently checked-out branch.
    Clean { head_branch: String },
    /// Skipped unless forced; the reason is the user-facing contract.
    Dirty(DirtyReason),
}

#[derive(Debug)]
pub enum DirtyReason {
    /// Tracked files modified, staged, or deleted (repo-relative paths).
    WorktreeModified(Vec<String>),
    /// Commits on HEAD that origin/<head-branch> does not have.
    LocalCommits(usize),
    DetachedHead(ObjectId),
    /// Mid-rebase/merge/cherry-pick/… (`gix::state::InProgress`).
    MidOperation(&'static str),
    /// The wanted branch exists locally but HEAD is deliberately elsewhere.
    BranchSwitched { head: String, expected: String },
    RemoteUrlChanged { on_disk: String, wanted: String },
    /// Any probe failure. Never silently "clean".
    Unknown(String),
}

impl DirtyReason {
    /// One-line user-facing description.
    pub fn describe(&self) -> String {
        match self {
            DirtyReason::WorktreeModified(paths) => match paths.as_slice() {
                [one] => format!("local modifications ({one})"),
                many => format!("local modifications ({} files)", many.len()),
            },
            DirtyReason::LocalCommits(n) => format!("{n} local commit(s) not on origin"),
            DirtyReason::DetachedHead(id) => {
                format!("detached HEAD at {}", &id.to_string()[..12.min(id.to_string().len())])
            }
            DirtyReason::MidOperation(op) => format!("a {op} is in progress"),
            DirtyReason::BranchSwitched { head, expected } => {
                format!("checked out on branch {head} instead of {expected}")
            }
            DirtyReason::RemoteUrlChanged { on_disk, wanted } => {
                format!("remote URL is {on_disk}, thornlist wants {wanted}")
            }
            DirtyReason::Unknown(err) => format!("could not inspect the repo ({err})"),
        }
    }
}

/// Probe result: the classification plus untracked paths, which never block a
/// fetch (git never overwrites them) but do block `--prune` and are reported
/// under `--verbose`.
#[derive(Debug)]
pub struct Probe {
    pub state: RepoState,
    pub untracked: Vec<String>,
}

/// Classify `repo_dir` against the thornlist's `wanted_url`/`wanted_branch`.
/// Infallible by design: any inspection error becomes `Dirty(Unknown)`.
pub fn probe(repo_dir: &Path, wanted_url: &str, wanted_branch: &str) -> Probe {
    if !repo_dir.exists() {
        return Probe { state: RepoState::Absent, untracked: Vec::new() };
    }
    match probe_inner(repo_dir, wanted_url, wanted_branch) {
        Ok(probe) => probe,
        Err(e) => Probe {
            state: RepoState::Dirty(DirtyReason::Unknown(format!("{e:#}"))),
            untracked: Vec::new(),
        },
    }
}

fn probe_inner(repo_dir: &Path, wanted_url: &str, wanted_branch: &str) -> Res<Probe> {
    let repo = gix::open(repo_dir).with_context(|| "not an openable git repository")?;

    if let Some(op) = repo.state() {
        use gix::state::InProgress::*;
        let name = match op {
            ApplyMailbox | ApplyMailboxRebase => "mailbox apply",
            Bisect => "bisect",
            CherryPick | CherryPickSequence => "cherry-pick",
            Merge => "merge",
            Rebase | RebaseInteractive => "rebase",
            Revert | RevertSequence => "revert",
        };
        return Ok(dirty(DirtyReason::MidOperation(name), Vec::new()));
    }

    // Remote URL check before worktree state: a repo pointing somewhere else
    // entirely is a different upstream no matter how clean it is.
    let remote = repo
        .find_remote("origin")
        .map_err(|e| anyhow!("no origin remote: {e}"))?;
    let on_disk_url = remote
        .url(Direction::Fetch)
        .map(|u| u.to_bstring().to_string())
        .ok_or_else(|| anyhow!("origin has no fetch URL"))?;
    if normalize_url(&on_disk_url) != normalize_url(wanted_url) {
        return Ok(dirty(
            DirtyReason::RemoteUrlChanged { on_disk: on_disk_url, wanted: wanted_url.to_owned() },
            Vec::new(),
        ));
    }

    let head = repo.head().with_context(|| "failed to read HEAD")?;
    let Some(head_ref) = head.referent_name() else {
        let id = head
            .id()
            .map(|id| id.detach())
            .unwrap_or_else(|| ObjectId::null(gix::hash::Kind::Sha1));
        return Ok(dirty(DirtyReason::DetachedHead(id), Vec::new()));
    };
    let head_branch = head_ref.shorten().to_string();

    // Worktree + index state. `into_iter(..)` covers both HEAD↔index (staged)
    // and index↔worktree (unstaged + untracked) changes.
    let (modified, untracked) = status_paths(&repo)?;
    if !modified.is_empty() {
        return Ok(dirty(DirtyReason::WorktreeModified(modified), untracked));
    }

    // Branch placement. An empty wanted branch means "no !REPO_BRANCH":
    // whatever branch the clone is on is the wanted one.
    let wanted =
        if wanted_branch.is_empty() { head_branch.clone() } else { wanted_branch.to_owned() };
    if head_branch != wanted {
        let wanted_local = format!("refs/heads/{wanted}");
        if repo.find_reference(&wanted_local).is_ok() {
            // The user has the wanted branch locally and deliberately moved
            // off it — that is their state to keep.
            return Ok(dirty(
                DirtyReason::BranchSwitched { head: head_branch, expected: wanted },
                untracked,
            ));
        }
        if repo.find_reference(&format!("refs/remotes/origin/{head_branch}")).is_err() {
            // The current branch has no remote-tracking ref at all: a
            // locally-created branch (in-progress work). Truthfully a
            // branch switch, not an inspection failure.
            return Ok(dirty(
                DirtyReason::BranchSwitched { head: head_branch, expected: wanted },
                untracked,
            ));
        }
        // Wanted branch absent locally, current branch tracks origin (e.g. a
        // release bump): fall through to the local-commits check, then Clean
        // — the planner emits an align, which creates and checks it out.
        // (Local commits on the current branch survive an align — the old
        // branch ref is left in place — but silently switching the worktree
        // away from in-progress work is still not ours to do unforced.)
    }

    // Local commits: HEAD commits that origin/<head-branch> does not have.
    let head_id = repo
        .head_id()
        .with_context(|| "HEAD points at no commit")?
        .detach();
    let upstream_name = format!("refs/remotes/origin/{head_branch}");
    let upstream_id = match repo.find_reference(&upstream_name) {
        Ok(mut r) => r.peel_to_id().with_context(|| "failed to peel upstream")?.detach(),
        Err(_) => {
            // head == wanted but its tracking ref is missing: genuinely odd
            // (a clone cactup never made?); never silently "clean".
            return Ok(dirty(
                DirtyReason::Unknown(format!("no remote-tracking ref {upstream_name}")),
                untracked,
            ));
        }
    };
    if upstream_id != head_id {
        let ahead = repo
            .rev_walk(Some(head_id))
            .with_hidden(Some(upstream_id))
            .all()
            .with_context(|| "failed to walk history")?
            .filter_map(|c| c.ok())
            .count();
        if ahead > 0 {
            return Ok(dirty(DirtyReason::LocalCommits(ahead), untracked));
        }
        // Behind origin only (e.g. an interrupted earlier fetch): still clean —
        // align fast-forwards it.
    }

    Ok(Probe { state: RepoState::Clean { head_branch }, untracked })
}

fn dirty(reason: DirtyReason, untracked: Vec<String>) -> Probe {
    Probe { state: RepoState::Dirty(reason), untracked }
}

/// URL comparison tolerance: trailing `/` and `.git` are cosmetic.
fn normalize_url(url: &str) -> &str {
    let url = url.trim_end_matches('/');
    url.strip_suffix(".git").unwrap_or(url)
}

/// Depth-1 single-branch clone of `url` at `branch` into `dest`, with a full
/// worktree checkout — matching what GetComponents produced ($SHALLOW_CLONE).
/// `branch: None` clones the remote's default branch (no `!REPO_BRANCH`).
pub fn clone(
    url: &str,
    branch: Option<&str>,
    dest: &Path,
    progress: &mut prodash::tree::Item,
) -> Res<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("Failed to create {}", dest.display()))?;

    let branch_desc = branch.unwrap_or("<default>");
    let mut prepare = gix::prepare_clone(url, dest)
        .with_context(|| format!("Failed to prepare clone of {url}"))?
        .with_shallow(Shallow::DepthAtRemote(1.try_into().expect("nonzero")))
        .with_ref_name(branch)
        .with_context(|| format!("Invalid branch name {branch_desc}"))?;
    if let Some(branch) = branch {
        // Single-branch: narrow the remote's fetch refspec to just this
        // branch, exactly like `git clone --single-branch`.
        let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
        prepare = prepare.configure_remote(move |mut r| {
            r.replace_refspecs(Some(refspec.as_str()), Direction::Fetch)?;
            Ok(r)
        });
    }

    let (mut checkout, _outcome) = prepare
        .fetch_then_checkout(&mut *progress, &gix::interrupt::IS_INTERRUPTED)
        .with_context(|| format!("Failed to fetch {url} (branch {branch_desc})"))?;
    let (_repo, _outcome) = checkout
        .main_worktree(&mut *progress, &gix::interrupt::IS_INTERRUPTED)
        .with_context(|| format!("Failed to check out {url} (branch {branch_desc})"))?;
    Ok(())
}

/// Make a *clean* repo match `origin/<branch>`: fetch the branch by explicit
/// refspec at depth 1 (this is what makes a brand-new release branch reachable
/// in an existing depth-1 single-branch clone — the case the Perl script
/// silently no-ops), point `refs/heads/<branch>` and `HEAD` at it, then check
/// out the tree, deleting files the old checkout tracked that the new one
/// doesn't. Returns the commit the repo ends on.
pub fn align(repo_dir: &Path, branch: &str, progress: &mut prodash::tree::Item) -> Res<ObjectId> {
    let repo = gix::open(repo_dir)
        .with_context(|| format!("Failed to open {}", repo_dir.display()))?;

    // Fetch the wanted branch explicitly.
    let refspec = format!("+refs/heads/{branch}:refs/remotes/origin/{branch}");
    let mut remote = repo
        .find_remote("origin")
        .with_context(|| "no origin remote")?;
    remote
        .replace_refspecs(Some(refspec.as_str()), Direction::Fetch)
        .with_context(|| "failed to set fetch refspec")?;
    remote
        .connect(Direction::Fetch)
        .with_context(|| "failed to connect to origin")?
        .prepare_fetch(&mut *progress, Default::default())
        .with_context(|| "failed to prepare fetch")?
        .with_shallow(Shallow::DepthAtRemote(1.try_into().expect("nonzero")))
        .receive(&mut *progress, &gix::interrupt::IS_INTERRUPTED)
        .with_context(|| format!("failed to fetch branch {branch}"))?;

    let target_id = repo
        .find_reference(&format!("refs/remotes/origin/{branch}"))
        .with_context(|| format!("origin/{branch} missing after fetch — does the branch exist upstream?"))?
        .peel_to_id()
        .with_context(|| format!("failed to resolve origin/{branch}"))?
        .detach();

    // The old index tells us which tracked files may need deleting after the
    // switch. Read it before touching anything.
    let old_paths: Vec<BString> = repo
        .open_index()
        .map(|idx| idx.entries().iter().map(|e| e.path(&idx).to_owned()).collect())
        .unwrap_or_default();

    // Move refs/heads/<branch> (create or force-update — the probe guaranteed
    // no local commits) and point HEAD at it.
    let log = |msg: &str| LogChange {
        mode: RefLog::AndReference,
        force_create_reflog: false,
        message: msg.into(),
    };
    let branch_ref: gix::refs::FullName = format!("refs/heads/{branch}")
        .try_into()
        .map_err(|e| anyhow!("invalid branch name {branch}: {e}"))?;
    repo.edit_references([
        RefEdit {
            change: Change::Update {
                log: log("cactup refetch: align"),
                expected: PreviousValue::Any,
                new: Target::Object(target_id),
            },
            name: branch_ref.clone(),
            deref: false,
        },
        RefEdit {
            change: Change::Update {
                log: log("cactup refetch: align"),
                expected: PreviousValue::Any,
                new: Target::Symbolic(branch_ref),
            },
            name: "HEAD".try_into().expect("HEAD is a valid ref name"),
            deref: false,
        },
    ])
    .with_context(|| format!("failed to update refs for branch {branch}"))?;

    // Check out the new tree over the existing worktree.
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("{} is bare", repo_dir.display()))?
        .to_owned();
    let tree_id = repo
        .find_object(target_id)
        .with_context(|| "fetched commit missing from odb")?
        .peel_to_tree()
        .with_context(|| "fetched commit has no tree")?
        .id;
    let mut index = repo
        .index_from_tree(&tree_id)
        .with_context(|| "failed to build index from tree")?;
    let mut opts = repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .with_context(|| "failed to read checkout options")?;
    opts.destination_is_initially_empty = false;
    opts.overwrite_existing = true;

    let files = progress.add_child("checkout");
    let bytes = progress.add_child("writing");
    gix::worktree::state::checkout(
        &mut index,
        &workdir,
        repo.objects.clone().into_arc().with_context(|| "failed to reopen odb")?,
        &files,
        &bytes,
        &gix::interrupt::IS_INTERRUPTED,
        opts,
    )
    .with_context(|| "worktree checkout failed")?;
    index.write(Default::default()).with_context(|| "failed to write index")?;

    // gix's checkout leaves files whose *content* already matches untouched,
    // so a mode-only difference between branches (644 ↔ 755) would linger
    // and read as a local modification on the next probe. Reconcile the
    // executable bit with the index explicitly.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for entry in index.entries() {
            let want_exec = entry.mode == gix::index::entry::Mode::FILE_EXECUTABLE;
            if !want_exec && entry.mode != gix::index::entry::Mode::FILE {
                continue;
            }
            let path = workdir.join(entry.path(&index).to_str_lossy().as_ref());
            if let Ok(meta) = std::fs::symlink_metadata(&path) {
                if !meta.file_type().is_file() {
                    continue;
                }
                let mode = meta.permissions().mode();
                let has_exec = mode & 0o111 != 0;
                if has_exec != want_exec {
                    let new_mode = if want_exec { mode | 0o111 } else { mode & !0o111 };
                    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(new_mode));
                }
            }
        }
    }

    // Delete tracked files that existed under the old checkout but not the
    // new one (gix's checkout only writes the new entries; it does not sweep).
    let new_paths: std::collections::HashSet<BString> =
        index.entries().iter().map(|e| e.path(&index).to_owned()).collect();
    let mut dirs = std::collections::BTreeSet::new();
    for old in old_paths {
        if !new_paths.contains(&old) {
            let path = workdir.join(old.to_str_lossy().as_ref());
            let _ = std::fs::remove_file(&path);
            let mut parent = path.parent().map(Path::to_path_buf);
            while let Some(p) = parent {
                if p == workdir {
                    break;
                }
                dirs.insert(p.clone());
                parent = p.parent().map(Path::to_path_buf);
            }
        }
    }
    // Deepest-first so nested empty dirs collapse; remove_dir fails (and is
    // ignored) on non-empty dirs.
    for dir in dirs.iter().rev() {
        let _ = std::fs::remove_dir(dir);
    }

    Ok(target_id)
}

/// The branch and commit a repo is currently on, for post-pass assertions and
/// `fetch-state.toml`.
/// `(modified, untracked)` for a repo. Modified = tracked files differing from
/// HEAD or the index (staged *and* unstaged); untracked = files git does not
/// know about. Shared by [`probe`] and [`source_state`], which need the same
/// walk but draw different conclusions from it.
fn status_paths(repo: &gix::Repository) -> Res<(Vec<String>, Vec<String>)> {
    let mut modified = Vec::new();
    let mut untracked = Vec::new();
    let iter = repo
        .status(gix::progress::Discard)
        .with_context(|| "failed to prepare status")?
        .into_iter(Vec::<BString>::new())
        .with_context(|| "failed to run status")?;
    for item in iter {
        let item = item.with_context(|| "status iteration failed")?;
        match item {
            gix::status::Item::TreeIndex(change) => {
                modified.push(change.location().to_string());
            }
            gix::status::Item::IndexWorktree(item) => {
                use gix::status::index_worktree::Item::*;
                let path = match &item {
                    Modification { rela_path, .. } => rela_path.to_string(),
                    DirectoryContents { entry, .. } => entry.rela_path.to_string(),
                    Rewrite { dirwalk_entry, .. } => dirwalk_entry.rela_path.to_string(),
                };
                let is_untracked = matches!(
                    &item,
                    DirectoryContents { entry, .. }
                        if matches!(entry.status, gix::dir::entry::Status::Untracked)
                );
                if is_untracked {
                    untracked.push(path);
                } else {
                    modified.push(path);
                }
            }
        }
    }
    modified.sort();
    modified.dedup();
    Ok((modified, untracked))
}

/// What a build actually compiles out of this repo, as a compact string:
/// the HEAD commit, plus a summary of the tracked files that differ from it.
///
/// This is deliberately a *live* reading rather than a replay of
/// `fetch-state.toml`: editing a thorn in place and rebuilding is a normal
/// workflow, and so is `git checkout` inside a repo, and neither moves
/// anything cactup recorded at fetch time.
///
/// The worktree part is `<n>mod@<newest-mtime-nanos>`, which is mtime-based
/// exactly like the `make` that consumes these sources — editing a file, or
/// editing a second one, or reverting one, all change it. Untracked files are
/// excluded on purpose: the Einstein Toolkit test harness leaves output inside
/// the source tree, and that must not read as a source change.
pub fn source_state(repo_dir: &Path) -> Res<String> {
    Ok(source_diff(repo_dir)?.state())
}

/// The full reading behind [`source_state`], for `cactup inst delta` /
/// `cactup config delta`, which report *what* diverged rather than just that
/// something did.
#[derive(Debug)]
pub struct SourceDiff {
    pub head: ObjectId,
    /// `None` when HEAD is detached.
    pub branch: Option<String>,
    /// Tracked files differing from HEAD or the index.
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
    /// Newest mtime among `modified`, in nanoseconds since the epoch. Part of
    /// the state string so that a *second* edit is distinguishable from the
    /// first — mtime-based exactly like the `make` that consumes these files.
    newest_mtime: u128,
}

impl SourceDiff {
    /// The compact form recorded in config metadata. See [`source_state`].
    pub fn state(&self) -> String {
        if self.modified.is_empty() {
            return self.head.to_string();
        }
        format!("{}+{}mod@{}", self.head, self.modified.len(), self.newest_mtime)
    }
}

pub fn source_diff(repo_dir: &Path) -> Res<SourceDiff> {
    let repo = gix::open(repo_dir)
        .with_context(|| format!("Failed to open {}", repo_dir.display()))?;
    let head_ref = repo.head().with_context(|| "failed to read HEAD")?;
    let branch = head_ref.referent_name().map(|r| r.shorten().to_string());
    let head = repo
        .head()
        .with_context(|| "failed to read HEAD")?
        .id()
        .map(|id| id.detach())
        .unwrap_or_else(|| ObjectId::null(gix::hash::Kind::Sha1));
    let (modified, untracked) = status_paths(&repo)?;
    let newest_mtime = modified
        .iter()
        .filter_map(|rel| std::fs::metadata(repo_dir.join(rel)).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .max()
        .unwrap_or(0);
    Ok(SourceDiff { head, branch, modified, untracked, newest_mtime })
}

pub fn head_of(repo_dir: &Path) -> Res<(String, ObjectId)> {
    let repo = gix::open(repo_dir)
        .with_context(|| format!("Failed to open {}", repo_dir.display()))?;
    let head = repo.head().with_context(|| "failed to read HEAD")?;
    let branch = head
        .referent_name()
        .map(|n| n.shorten().to_string())
        .unwrap_or_else(|| "(detached)".to_owned());
    let id = repo.head_id().with_context(|| "HEAD points at no commit")?.detach();
    Ok((branch, id))
}

/// A cheap early-out for the planner: is the repo already exactly at
/// origin/<branch> according to local refs alone (no network)?
pub fn at_local_origin_tip(repo_dir: &Path, branch: &str) -> bool {
    let Ok(repo) = gix::open(repo_dir) else { return false };
    let head = match repo.head_id() {
        Ok(id) => id.detach(),
        Err(_) => return false,
    };
    let origin = match repo.find_reference(&format!("refs/remotes/origin/{branch}")) {
        Ok(mut r) => match r.peel_to_id() {
            Ok(id) => id.detach(),
            Err(_) => return false,
        },
        Err(_) => return false,
    };
    head == origin
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_normalization_tolerates_cosmetics() {
        assert_eq!(normalize_url("https://x.org/repo.git"), normalize_url("https://x.org/repo"));
        assert_eq!(normalize_url("https://x.org/repo/"), normalize_url("https://x.org/repo"));
        assert_ne!(normalize_url("https://x.org/repo"), normalize_url("https://x.org/other"));
    }

    #[test]
    fn probe_absent_and_non_repo() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(matches!(probe(&missing, "u", "b").state, RepoState::Absent));

        // An existing plain directory is not silently "clean".
        let plain = dir.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert!(matches!(
            probe(&plain, "u", "b").state,
            RepoState::Dirty(DirtyReason::Unknown(_))
        ));
    }
}


#[cfg(test)]
mod source_state_tests {
    use super::*;

    /// The user's edit-in-place workflow: change a thorn's source, rebuild.
    /// The commit never moves, so only the worktree half of the state string
    /// can carry the change.
    #[test]
    fn source_state_tracks_edits_without_a_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("cactusbase");
        testrepo::init(&dir);
        testrepo::commit_file(&dir, "thorn.cc", "int a;\n");

        let clean = source_state(&dir).unwrap();
        assert!(!clean.contains('+'), "a fresh checkout is clean: {clean}");
        assert_eq!(source_state(&dir).unwrap(), clean, "reading twice must not drift");

        // Edit the tracked file: same commit, different worktree.
        std::fs::write(dir.join("thorn.cc"), "int a; int b;\n").unwrap();
        let edited = source_state(&dir).unwrap();
        assert_ne!(edited, clean, "a local edit must change the state");
        assert!(edited.starts_with(&clean), "the commit half must be unchanged: {edited}");
        assert!(edited.contains("1mod@"), "one modified file: {edited}");

        // Reverting the content restores the clean state.
        std::fs::write(dir.join("thorn.cc"), "int a;\n").unwrap();
        assert_eq!(source_state(&dir).unwrap(), clean, "reverting must read as clean again");

        // An untracked file is NOT a source change: the Einstein Toolkit test
        // harness leaves output inside the source tree.
        std::fs::write(dir.join("test_output.log"), "noise").unwrap();
        assert_eq!(source_state(&dir).unwrap(), clean, "untracked output must not count");

        // A new commit moves the commit half.
        testrepo::commit(&dir, "more");
        assert_ne!(source_state(&dir).unwrap(), clean);
    }
}

/// Test-only repo builder: a real on-disk git repo, created with gix so the
/// suite never depends on a `git` binary. Commits carry an empty tree — HEAD
/// movement is what the source-tracking tests need, and an empty tree keeps
/// the helper small.
#[cfg(test)]
pub(crate) mod testrepo {
    use super::*;

    pub(crate) fn init(dir: &Path) -> gix::Repository {
        std::fs::create_dir_all(dir).unwrap();
        gix::init(dir).expect("init repo");
        // gix refuses to commit without a committer identity, and the ambient
        // user config must not leak into a test.
        std::fs::write(
            dir.join(".git/config"),
            "[user]\n\tname = cactup-test\n\temail = test@invalid\n",
        )
        .unwrap();
        gix::open(dir).expect("reopen repo")
    }

    /// Commit `name` with `content` and materialize it into the worktree and
    /// index, so it is a *tracked* file that editing on disk shows up as a
    /// modification. That is what the edit-in-place workflow needs to be
    /// testable without shelling out to `git`.
    pub(crate) fn commit_file(dir: &Path, name: &str, content: &str) -> ObjectId {
        let repo = gix::open(dir).expect("open repo");
        let blob = repo.write_blob(content.as_bytes()).expect("write blob").detach();
        let mut tree = gix::objs::Tree::empty();
        tree.entries.push(gix::objs::tree::Entry {
            mode: gix::objs::tree::EntryKind::Blob.into(),
            filename: name.into(),
            oid: blob,
        });
        let tree_id = repo.write_object(&tree).expect("write tree").detach();
        let parents: Vec<ObjectId> = repo
            .head()
            .ok()
            .and_then(|mut h| h.peel_to_commit().ok())
            .map(|c| vec![c.id])
            .unwrap_or_default();
        let id = repo.commit("HEAD", format!("add {name}"), tree_id, parents).expect("commit");

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

    /// Add one commit on HEAD and return the new commit id.
    pub(crate) fn commit(dir: &Path, message: &str) -> ObjectId {
        let repo = gix::open(dir).expect("open repo");
        let tree = repo
            .write_object(gix::objs::Tree::empty())
            .expect("write empty tree")
            .detach();
        let parents: Vec<ObjectId> = repo
            .head()
            .ok()
            .and_then(|mut h| h.peel_to_commit().ok())
            .map(|c| vec![c.id])
            .unwrap_or_default();
        repo.commit("HEAD", message, tree, parents).expect("commit").detach()
    }
}
