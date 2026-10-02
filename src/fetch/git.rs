//! gix-backed git operations for the native fetcher (spec §3.2): the per-repo
//! dirtiness probe, depth-1 single-branch clone, and branch alignment.
//!
//! The fetcher never merges or rebases. Dirty repos are skipped by the planner
//! by definition, so [`align`] only ever has to make a *clean* worktree match
//! `origin/<branch>` — that property is what makes the gix-only (no external
//! `git`) choice viable.

use crate::Res;
use anyhow::{anyhow, bail, Context};
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
    /// A checkout cactup itself started and Ctrl-C cut short: tracked files
    /// differ from HEAD, but every one holds either HEAD's version or the one
    /// from before cactup's align moved HEAD — nothing of the user's.
    UnfinishedCheckout(Vec<String>),
    /// Commits on HEAD that origin/<head-branch> does not have.
    LocalCommits(usize),
    DetachedHead(ObjectId),
    /// Mid-rebase/merge/cherry-pick/… (`gix::state::InProgress`).
    MidOperation(&'static str),
    /// The wanted branch exists locally but HEAD is deliberately elsewhere.
    BranchSwitched { head: String, expected: String },
    /// `modified` is the worktree's tracked-file changes (repo-relative
    /// paths), gathered eagerly here rather than left for a later dirty
    /// classification: this is the one `DirtyReason` a forced refetch can
    /// override (via [`set_origin_url`] + a normal align), so it is also the
    /// one whose backup pass needs to know what to save *before* that happens.
    RemoteUrlChanged { on_disk: String, wanted: String, modified: Vec<String> },
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
            DirtyReason::UnfinishedCheckout(paths) => {
                format!("an interrupted checkout ({} file(s) behind, no local edits)", paths.len())
            }
            DirtyReason::LocalCommits(n) => format!("{n} local commit(s) not on origin"),
            DirtyReason::DetachedHead(id) => {
                format!("detached HEAD at {}", &id.to_string()[..12.min(id.to_string().len())])
            }
            DirtyReason::MidOperation(op) => format!("a {op} is in progress"),
            DirtyReason::BranchSwitched { head, expected } => {
                format!("checked out on branch {head} instead of {expected}")
            }
            DirtyReason::RemoteUrlChanged { on_disk, wanted, modified } => {
                let mut s = format!("remote URL is {on_disk}, thornlist wants {wanted}");
                if !modified.is_empty() {
                    s.push_str(&format!(" ({} local modification(s))", modified.len()));
                }
                s
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
    let _span = crate::timing::span("git probe");
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
        // A retargeted repo can *also* carry local edits — e.g. the fork
        // adoption workflow above is exactly "edit a thorn, then repoint
        // origin at your fork" — and the refetch backup pass (src/fetch/mod.rs)
        // needs `modified` to know what to save before a forced refetch
        // clobbers the worktree. So pay for the status walk here too, even
        // though a bare URL mismatch is the rare case: skipping it would
        // silently drop edits that were never backed up.
        // Interrupted, the walk saw only some files: never report its partial
        // (or empty) list as everything there is to back up.
        let (modified, untracked) = match status_paths(&repo, Untracked::List) {
            Ok(paths) => paths,
            Err(e) if gix::interrupt::is_triggered() => return Err(e),
            Err(_) => Default::default(),
        };
        return Ok(Probe {
            state: RepoState::Dirty(DirtyReason::RemoteUrlChanged {
                on_disk: on_disk_url,
                wanted: wanted_url.to_owned(),
                modified,
            }),
            untracked,
        });
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
    let (modified, untracked) = status_paths(&repo, Untracked::List)?;
    if !modified.is_empty() {
        let reason = if interrupted_align(&repo).is_some() && local_edits(repo_dir, &modified).is_empty() {
            DirtyReason::UnfinishedCheckout(modified)
        } else {
            DirtyReason::WorktreeModified(modified)
        };
        return Ok(dirty(reason, untracked));
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

/// Canonical comparison key for a remote URL, so that the fork-adoption
/// workflow — a thornlist re-pointing a repo at an `ssh://` fork of the same
/// project — doesn't read as a different repo just because of URL spelling.
/// `git@host:user/repo.git`, `https://host/user/repo`, and
/// `ssh://git@host:22/user/repo/` must all compare equal, or the probe
/// reports `RemoteUrlChanged` for a repo that is, upstream-identity-wise,
/// unchanged.
///
/// Rules: strip a leading scheme (`ssh://`, `git://`, `git+ssh://`,
/// `ssh+git://`, `https://`, `http://`, case-insensitive) or recognize
/// scp-style `[user@]host:path` (no `://`, first `:` before first `/`); drop
/// `user[:password]@` userinfo and a trailing `:<port>` from the authority;
/// lowercase the host (DNS is case-insensitive; hosting providers don't
/// distinguish `GitHub.com` from `github.com`); leave path case alone (POSIX
/// paths are case-sensitive, and repo/owner names on some forges are too, so
/// lowercasing here would conflate genuinely different repos); strip the
/// path's leading `/`, trailing `/`, and trailing `.git`. `file://` URLs and
/// bare local paths (`/…`, `./…`, `../…`, `~…`) have no authority: the result
/// is just the cleaned path. Empty input stays empty (the planner passes
/// `""` for "no wanted URL" in some code paths).
pub(crate) fn normalize_url(url: &str) -> String {
    let url = url.trim();
    if url.is_empty() {
        return String::new();
    }

    // Local path forms have no authority to split off. A leading char alone
    // tells absolute from relative from home-relative apart, so — unlike the
    // host+path case below — the leading character is never stripped here.
    if let Some(rest) = url.strip_prefix("file://") {
        return clean_path(rest);
    }
    if url.starts_with('/') || url.starts_with("./") || url.starts_with("../") || url.starts_with('~') {
        return clean_path(url);
    }

    const SCHEMES: [&str; 6] = ["ssh://", "git://", "git+ssh://", "ssh+git://", "https://", "http://"];
    let schemeless = SCHEMES.iter().find_map(|scheme| {
        (url.len() >= scheme.len() && url[..scheme.len()].eq_ignore_ascii_case(scheme))
            .then(|| &url[scheme.len()..])
    });

    let (authority, path) = if let Some(rest) = schemeless {
        // URL-style: `[user[:password]@]host[:port][/path]`.
        rest.split_once('/').unwrap_or((rest, ""))
    } else {
        let colon = url.find(':');
        let slash = url.find('/');
        match colon {
            // scp-style `[user@]host:path` — no scheme, and the `:` precedes
            // any `/` (so e.g. a Windows path with a slash before its drive
            // colon, which can't happen, or any path-then-colon shape, isn't
            // misread as scp-style).
            Some(c) if slash.is_none_or(|s| c < s) => (&url[..c], &url[c + 1..]),
            // No recognized scheme and no scp form: opaque, treat as a path.
            _ => return clean_path(url),
        }
    };

    // Userinfo (`user[:password]@`) is login material, not repo identity.
    let host_port = authority.rsplit_once('@').map_or(authority, |(_userinfo, host)| host);
    // A trailing `:<port>` only exists in URL-style authorities — scp-style's
    // only `:` was already consumed as the host/path separator above, so
    // this is a no-op for that branch (no further `:` remains to match).
    let host = host_port
        .rsplit_once(':')
        .filter(|(_, port)| !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()))
        .map_or(host_port, |(host, _port)| host)
        .to_ascii_lowercase();

    // Host+path form: unlike a bare local path, a leading `/` here is just
    // URL syntax (separating authority from path), not part of repo
    // identity, so it's stripped along with the trailing `/` and `.git`.
    // Case is left alone past this point — some forges have case-sensitive
    // owner/repo names, so lowercasing the path could conflate two distinct
    // repos (`/repo` vs `/Repo`) that the host treats as different.
    let path = path.strip_prefix('/').unwrap_or(path);
    let path = path.strip_suffix('/').unwrap_or(path);
    let path = path.strip_suffix(".git").unwrap_or(path);

    if path.is_empty() {
        host
    } else {
        format!("{host}/{path}")
    }
}

/// Trailing-slash/`.git` cleanup for a bare local path (no host). See
/// [`normalize_url`] for why the leading character is untouched here.
fn clean_path(path: &str) -> String {
    let path = path.strip_suffix('/').unwrap_or(path);
    path.strip_suffix(".git").unwrap_or(path).to_owned()
}

/// Depth-1 single-branch clone of `url` at `branch` into `dest`, with a full
/// worktree checkout — matching what GetComponents produced ($SHALLOW_CLONE).
/// `branch: None` clones the remote's default branch (no `!REPO_BRANCH`).
pub fn clone(
    url: &str,
    branch: Option<&str>,
    dest: &Path,
    progress: &mut (impl prodash::NestedProgress<SubProgress: 'static> + 'static),
) -> Res<()> {
    let _span = crate::timing::span("git clone");
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
    // gix's checkout returns `Ok` when interrupted, with only part of the
    // tree written: that clone must not be reported as done.
    if gix::interrupt::is_triggered() {
        let name = dest.file_name().unwrap_or(dest.as_os_str()).to_string_lossy();
        bail!(
            "interrupted during the clone's checkout: {} is incomplete; delete it, then fetch {name} again",
            dest.display()
        );
    }
    Ok(())
}

/// Make a *clean* repo match `origin/<branch>`: fetch the branch by explicit
/// refspec at depth 1 (this is what makes a brand-new release branch reachable
/// in an existing depth-1 single-branch clone — the case the Perl script
/// silently no-ops), point `refs/heads/<branch>` and `HEAD` at it, then check
/// out the tree, deleting files the old checkout tracked that the new one
/// doesn't. Returns the commit the repo ends on.
///
/// Only what changed is written: a repo already at the fetched tip is left
/// alone entirely, and otherwise a file whose blob and mode are the same in
/// both trees keeps its bytes, mtime and index stat (a full rewrite costs a
/// create, write and close per file on NFS, and moves every mtime, so `make`
/// rebuilds what never changed). `force_overwrite` — a forced refetch of a
/// modified repo, or one whose `origin` was just repointed — opts out of
/// both: there the full checkout is exactly what discards the local edits.
pub fn align(
    repo_dir: &Path,
    branch: &str,
    force_overwrite: bool,
    progress: &mut (impl prodash::NestedProgress<SubProgress: 'static> + 'static),
) -> Res<ObjectId> {
    let _span = crate::timing::span("git align");
    let mut repo = gix::open(repo_dir)
        .with_context(|| format!("Failed to open {}", repo_dir.display()))?;
    // Every ref the fetch below moves gets a reflog entry, and gix refuses
    // to write one without a committer identity — which an account with no
    // ~/.gitconfig doesn't have. Fall back to gix's generic in-memory
    // identity (never written to disk; a configured identity always wins).
    let _ = repo.committer_or_set_generic_fallback();

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

    let branch_ref: gix::refs::FullName = format!("refs/heads/{branch}")
        .try_into()
        .map_err(|e| anyhow!("invalid branch name {branch}: {e}"))?;

    // Already there: HEAD names the branch and sits on the fetched tip, and
    // the probe found the worktree clean, so there is nothing to write.
    if !force_overwrite
        && repo.head_name().ok().flatten().as_ref() == Some(&branch_ref)
        && repo.head_id().ok().map(|id| id.detach()) == Some(target_id)
    {
        clear_align_marker(&repo);
        return Ok(target_id);
    }

    // The old index tells us which tracked files may need deleting after the
    // switch, and which ones are already right on disk. Read it before
    // touching anything.
    let old_index = repo.open_index().ok();
    let old_paths: Vec<BString> = old_index
        .as_ref()
        .map(|idx| idx.entries().iter().map(|e| e.path(idx).to_owned()).collect())
        .unwrap_or_default();

    // Move refs/heads/<branch> (create or force-update — the probe guaranteed
    // no local commits) and point HEAD at it.
    let log = |msg: &str| LogChange {
        mode: RefLog::AndReference,
        force_create_reflog: false,
        message: msg.into(),
    };
    // From here until its index is written, an interruption leaves the repo
    // between two trees; the marker is how a later probe tells that apart
    // from a user's edits (see `interrupted_align`).
    let before = repo.head_id().ok().map(|id| id.detach());
    write_align_marker(&repo, before, target_id)?;
    repo.edit_references([
        RefEdit {
            change: Change::Update {
                log: log(ALIGN_REFLOG_MESSAGE),
                expected: PreviousValue::Any,
                new: Target::Object(target_id),
            },
            name: branch_ref.clone(),
            deref: false,
        },
        RefEdit {
            change: Change::Update {
                log: log(ALIGN_REFLOG_MESSAGE),
                expected: PreviousValue::Any,
                new: Target::Symbolic(branch_ref),
            },
            name: "HEAD".try_into().expect("HEAD is a valid ref name"),
            deref: false,
        },
    ])
    .with_context(|| format!("failed to update refs for branch {branch}"))?;

    // Check out the new tree over the existing worktree.
    let tree_id = repo
        .find_object(target_id)
        .with_context(|| "fetched commit missing from odb")?
        .peel_to_tree()
        .with_context(|| "fetched commit has no tree")?
        .id;
    let old = if force_overwrite { None } else { old_index.as_ref() };
    check_out_tree(&repo, tree_id, old, &old_paths, progress, &gix::interrupt::IS_INTERRUPTED)?;
    clear_align_marker(&repo);
    Ok(target_id)
}

/// The reflog message [`align`] writes when it moves a branch and HEAD.
const ALIGN_REFLOG_MESSAGE: &str = "cactup refetch: align";

/// `<git_dir>/cactup-align`: present only while an [`align`] that moves HEAD
/// has not yet written its index — so, left behind, it means that align was
/// cut short. It holds `<commit moved from> <commit moved to>`.
fn align_marker(repo: &gix::Repository) -> std::path::PathBuf {
    repo.git_dir().join("cactup-align")
}

fn write_align_marker(repo: &gix::Repository, from: Option<ObjectId>, to: ObjectId) -> Res<()> {
    let from = from.unwrap_or_else(|| ObjectId::null(to.kind()));
    let path = align_marker(repo);
    std::fs::write(&path, format!("{from} {to}\n")).with_context(|| format!("Failed to write {}", path.display()))
}

fn clear_align_marker(repo: &gix::Repository) {
    let _ = std::fs::remove_file(align_marker(repo));
}

/// When an [`align`] was cut short on its way to the current HEAD, the commit
/// it moved from — whose tree the interrupted checkout left in the index and
/// partly on disk. `None` when no align was interrupted here (the marker is
/// absent, or names a different HEAD), or when it moved from nothing.
fn interrupted_align(repo: &gix::Repository) -> Option<ObjectId> {
    let marker = std::fs::read_to_string(align_marker(repo)).ok()?;
    let (from, to) = marker.trim().split_once(' ')?;
    let (from, to) = (ObjectId::from_hex(from.as_bytes()).ok()?, ObjectId::from_hex(to.as_bytes()).ok()?);
    (Some(to) == repo.head_id().ok().map(|id| id.detach()) && !from.is_null()).then_some(from)
}

/// The paths among `paths` that hold something of the user's: a file (or
/// symlink) whose content and kind (file, executable, symlink) match neither
/// HEAD's entry nor — only when an align was cut short here — the entry from
/// before that align. Those are the only files a forced refetch can lose; a
/// deleted file has nothing to lose. When a path cannot be checked it counts
/// as an edit, so the answer only ever errs toward backing up too much.
pub fn local_edits(repo_dir: &Path, paths: &[String]) -> Vec<String> {
    let Ok(repo) = gix::open(repo_dir) else { return paths.to_vec() };
    let tree_of = |commit: ObjectId| repo.find_object(commit).ok()?.peel_to_tree().ok();
    let head_tree = repo.head_id().ok().and_then(|id| tree_of(id.detach()));
    let before_tree = interrupted_align(&repo).and_then(tree_of);
    let entry_at = |tree: &Option<gix::Tree<'_>>, rel: &str| {
        let entry = tree.as_ref()?.lookup_entry_by_path(rel).ok()??;
        Some((entry.object_id(), entry.mode().kind()))
    };
    use gix::objs::tree::EntryKind;
    let hash_kind = repo.object_hash();
    paths
        .iter()
        .filter(|rel| {
            let path = repo_dir.join(rel);
            let Ok(meta) = std::fs::symlink_metadata(&path) else { return false }; // deleted
            let kind = if meta.file_type().is_symlink() {
                EntryKind::Link
            } else if std::os::unix::fs::PermissionsExt::mode(&meta.permissions()) & 0o111 != 0 {
                EntryKind::BlobExecutable
            } else {
                EntryKind::Blob
            };
            let id = if meta.file_type().is_symlink() {
                use std::os::unix::ffi::OsStrExt;
                let Ok(target) = std::fs::read_link(&path) else { return true };
                gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, target.as_os_str().as_bytes()).ok()
            } else if meta.is_file() {
                let Ok(mut file) = std::fs::File::open(&path) else { return true };
                let discard = &mut gix::progress::Discard;
                let blob = gix::objs::Kind::Blob;
                let interrupt = &gix::interrupt::IS_INTERRUPTED;
                gix::objs::compute_stream_hash(hash_kind, blob, &mut file, meta.len(), discard, interrupt).ok()
            } else {
                return true; // a directory where a file was: not ours to judge
            };
            let Some(id) = id else { return true };
            let on_disk = Some((id, kind));
            on_disk != entry_at(&head_tree, rel) && on_disk != entry_at(&before_tree, rel)
        })
        .cloned()
        .collect()
}

/// How an [`align`] cut short by Ctrl-C mid-checkout begins its error, for
/// callers that want to add advice about such repos (a forced refetch finishes
/// it). An interrupted [`clone`] says something else: it is to be deleted.
pub const INTERRUPTED_CHECKOUT: &str = "interrupted during checkout";

/// Check `tree_id` out over `repo`'s worktree and write its index, keeping the
/// entries of `old` that are already right on disk (`None`: rewrite all), and
/// delete the files `old_paths` tracked that the new tree does not.
///
/// gix's checkout returns `Ok` when `should_interrupt` cuts it short, with
/// only part of the tree written; that is reported as an "interrupted" error
/// here, never as a finished checkout. The index is left as it was: under the
/// HEAD that already moved it reads as staged changes, so the repo is plainly
/// modified, and it still lists the old tree's paths — which the recovering
/// refetch needs to delete the files the new tree dropped.
fn check_out_tree(
    repo: &gix::Repository,
    tree_id: ObjectId,
    old: Option<&gix::index::File>,
    old_paths: &[BString],
    progress: &mut (impl prodash::NestedProgress<SubProgress: 'static> + 'static),
    should_interrupt: &std::sync::atomic::AtomicBool,
) -> Res<()> {
    let workdir = repo
        .workdir()
        .ok_or_else(|| anyhow!("{} is bare", repo.git_dir().display()))?
        .to_owned();
    let mut index = repo
        .index_from_tree(&tree_id)
        .with_context(|| "failed to build index from tree")?;
    if let Some(old) = old {
        keep_unchanged_entries(&mut index, old);
    }
    let mut opts = repo
        .checkout_options(gix::worktree::stack::state::attributes::Source::IdMapping)
        .with_context(|| "failed to read checkout options")?;
    opts.destination_is_initially_empty = false;
    opts.overwrite_existing = true;

    // Named exactly as gix names its own checkout progress, so both paths
    // collapse onto a component's one line the same way (see
    // `progress::role_of`): the bounded file count draws the bar, the
    // unbounded byte count beside it is dropped.
    let files = progress.add_child("checkout");
    let bytes = progress.add_child("writing");
    gix::worktree::state::checkout(
        &mut index,
        &workdir,
        repo.objects.clone().into_arc().with_context(|| "failed to reopen odb")?,
        &files,
        &bytes,
        should_interrupt,
        opts,
    )
    .with_context(|| "worktree checkout failed")?;
    // The way to finish it depends on the command (a refetch can force it; an
    // install that was never registered cannot be refetched), so the callers
    // say how: see `INTERRUPTED_CHECKOUT`.
    if should_interrupt.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("{INTERRUPTED_CHECKOUT}: {} is only partly updated", workdir.display());
    }

    reconcile_exec_bits(&mut index, &workdir);
    for entry in index.entries_mut() {
        entry.flags.remove(gix::index::entry::Flags::SKIP_WORKTREE);
    }
    index.write(Default::default()).with_context(|| "failed to write index")?;

    // Delete tracked files that existed under the old checkout but not the
    // new one (gix's checkout only writes the new entries; it does not sweep).
    let new_paths: std::collections::HashSet<BString> =
        index.entries().iter().map(|e| e.path(&index).to_owned()).collect();
    let mut dirs = std::collections::BTreeSet::new();
    for old in old_paths {
        if !new_paths.contains(old) {
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
    Ok(())
}

/// gix's checkout adds the executable bit to a file it overwrites but never
/// removes one, so a 755 → 644 change between branches would linger and read
/// as a local modification on the next probe. Reconcile the bit for every
/// entry checkout wrote (`SKIP_WORKTREE` marks the kept ones, whose mode is
/// right by construction), and re-record the stat of each one changed: chmod
/// moves its ctime, which the index compares.
fn reconcile_exec_bits(index: &mut gix::index::File, workdir: &Path) {
    use gix::index::entry::{Flags, Mode};
    use std::os::unix::fs::PermissionsExt;
    for (entry, rela_path) in index.entries_mut_with_paths() {
        let want_exec = entry.mode == Mode::FILE_EXECUTABLE;
        if entry.flags.contains(Flags::SKIP_WORKTREE) || (!want_exec && entry.mode != Mode::FILE) {
            continue;
        }
        let path = workdir.join(rela_path.to_str_lossy().as_ref());
        let Ok(meta) = std::fs::symlink_metadata(&path) else { continue };
        if !meta.file_type().is_file() {
            continue;
        }
        let mode = meta.permissions().mode();
        if (mode & 0o111 != 0) == want_exec {
            continue;
        }
        let new_mode = if want_exec { mode | 0o111 } else { mode & !0o111 };
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(new_mode));
        if let Ok(stat) = gix::index::fs::Metadata::from_path_no_follow(&path)
            .map_err(|_| ())
            .and_then(|m| gix::index::entry::Stat::from_fs(&m).map_err(|_| ()))
        {
            entry.stat = stat;
        }
    }
}

/// What [`settle_index`] did to one repo's index.
#[derive(Debug, PartialEq, Eq)]
pub enum Settled {
    /// No entry was racy: nothing to do.
    Clean,
    /// Rewritten with the same entries, so its mtime now post-dates them.
    /// (Serialized by gix like any index it writes — the same as `align`'s own
    /// write: optional extensions it does not carry, such as the untracked
    /// cache, are dropped, which git simply rebuilds.)
    Rewritten,
    /// Left as it was: `index.lock` was held, an entry's content did not
    /// verify, the only racy entries were future-dated, the second never
    /// passed, Ctrl-C, or an I/O error. The index is still valid — only slower
    /// to status.
    Left,
}

/// Rewrite `repo_dir`'s index unchanged when any entry is "racy" against it,
/// so later status walks match those entries by stat instead of hashing them.
///
/// git (and gix) cannot trust the stat of a file modified in the same second
/// the index was written — a same-size edit later in that second would look
/// identical — so status reads and hashes every such entry, every time. A
/// checkout writes its files and then its index within the same second(s), so
/// a fresh clone of a small repo is racy throughout, and gix's status never
/// writes the refreshed index back the way `git status` does. Rewriting the
/// same index once the fileserver's clock has moved past the newest entry
/// fixes that at the source.
///
/// Safe by construction: the rewrite happens under git's own `index.lock`
/// (a concurrent `git add` fails fast, or we skip), it re-reads the index
/// under that lock, and it vouches only for entries whose content it has
/// just hashed against the index. The remaining exposure — a same-size edit
/// in the same second, after that hash — is the one git's own refresh has.
/// Best effort throughout: any doubt leaves the index alone.
pub fn settle_index(repo_dir: &Path) -> Res<Settled> {
    use std::time::{Duration, Instant, UNIX_EPOCH};
    let repo = gix::open(repo_dir).with_context(|| format!("Failed to open {}", repo_dir.display()))?;
    // Unlocked first look: most indexes are not racy, and this costs one
    // read. (It cannot tell future-dated entries apart — there is no "now"
    // yet — so a repo with any takes the lock below and lets go each time.)
    let index = repo.open_index().with_context(|| "failed to read index")?;
    if !index.entries().iter().any(|e| is_racy_in(e, &index)) {
        return Ok(Settled::Clean);
    }
    drop(index);

    let index_path = repo.index_path();
    let Ok(mut lock) = gix::lock::File::acquire_to_update_resource(
        &index_path,
        gix::lock::acquire::Fail::Immediately,
        None,
    ) else {
        return Ok(Settled::Left);
    };
    // Dropping `lock` from here on rolls back: the index stays as it was.
    let index = repo.open_index().with_context(|| "failed to re-read index under its lock")?;
    // The lock file was just created: its mtime is the fileserver's "now", in
    // exactly the clock gix later compares entry mtimes against. Entries more
    // than a second ahead of it are future-dated (a skewed writer, an
    // unpacked archive) and no rewrite can ever settle them.
    let now = lock.with_mut(|f| f.metadata()).with_context(|| "failed to stat index.lock")?;
    let now_secs = file_secs(&now);
    let mut newest = None;
    // The earliest future-dated entry, which is never verified: if hashing
    // ran long enough that the write lands in its second, the rewrite would
    // vouch for it unchecked, so that write is rolled back instead.
    let mut first_unverified = None;
    for entry in index.entries() {
        if gix::interrupt::is_triggered() {
            return Ok(Settled::Left);
        }
        let secs = i64::from(entry.stat.mtime.secs);
        if !is_racy_in(entry, &index) {
            continue;
        }
        if secs > now_secs + 1 {
            first_unverified = Some(first_unverified.map_or(secs, |first: i64| first.min(secs)));
            continue;
        }
        if !content_matches(&repo, repo_dir, entry, entry.path(&index))? {
            return Ok(Settled::Left);
        }
        newest = newest.max(Some(secs));
    }
    let Some(newest) = newest else { return Ok(Settled::Left) };

    let mut bytes = Vec::new();
    let skip_hash = index.checksum().is_none_or(|c| c.is_null());
    index
        .write_to(&mut bytes, gix::index::write::Options { extensions: Default::default(), skip_hash })
        .with_context(|| "failed to serialize index")?;
    // Write, then see what second the server stamped; while that is not past
    // the newest racy entry, wait out the rest of the second and write again
    // (the same bytes in place). At most two waits: `newest` is at most one
    // second ahead of `now`.
    for attempt in 0..3 {
        let written = lock
            .with_mut(|f| {
                use std::io::{Seek, Write};
                f.seek(std::io::SeekFrom::Start(0))?;
                f.write_all(&bytes)?;
                f.set_len(bytes.len() as u64)?;
                f.metadata()
            })
            .with_context(|| "failed to write index.lock")?;
        if first_unverified.is_some_and(|first| file_secs(&written) >= first) {
            return Ok(Settled::Left);
        }
        if file_secs(&written) > newest {
            lock.commit().map_err(|e| e.error).with_context(|| "failed to commit index.lock")?;
            return Ok(Settled::Rewritten);
        }
        if attempt == 2 || gix::interrupt::is_triggered() {
            break;
        }
        let stamped = written.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok());
        let nanos = stamped.map_or(0, |d| d.subsec_nanos());
        let wait = Duration::from_nanos(1_000_000_000 - u64::from(nanos)) + Duration::from_millis(50);
        // Interrupt-aware: Ctrl-C leaves the index as it was.
        let until = Instant::now() + wait;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            if gix::interrupt::is_triggered() {
                return Ok(Settled::Left);
            }
            std::thread::sleep(left.min(Duration::from_millis(25)));
        }
    }
    Ok(Settled::Left)
}

/// gix's racy test with its default (seconds-only) stat options: an entry
/// modified at or after the second its index was written.
fn is_racy_in(entry: &gix::index::Entry, index: &gix::index::File) -> bool {
    entry.stat.is_racy(index.timestamp(), gix::index::entry::stat::Options::default())
}

fn file_secs(meta: &std::fs::Metadata) -> i64 {
    use std::os::unix::fs::MetadataExt;
    meta.mtime()
}

/// Whether the worktree file behind `entry` still hashes to the entry's blob.
/// Only plain files, executables and symlinks can be checked; anything else
/// (a submodule's commit entry) counts as not verified. A file under a
/// clean/smudge filter also fails to match — such a repo is just not settled.
fn content_matches(
    repo: &gix::Repository,
    repo_dir: &Path,
    entry: &gix::index::Entry,
    rela_path: &gix::bstr::BStr,
) -> Res<bool> {
    use gix::index::entry::Mode;
    let path = repo_dir.join(rela_path.to_str_lossy().as_ref());
    let hash_kind = repo.object_hash();
    let id = if entry.mode == Mode::FILE || entry.mode == Mode::FILE_EXECUTABLE {
        // Streamed (and interrupt-aware): a racy entry can be a large file.
        let Ok(mut file) = std::fs::File::open(&path) else { return Ok(false) };
        let Ok(len) = file.metadata().map(|m| m.len()) else { return Ok(false) };
        let discard = &mut gix::progress::Discard;
        match gix::objs::compute_stream_hash(
            hash_kind,
            gix::objs::Kind::Blob,
            &mut file,
            len,
            discard,
            &gix::interrupt::IS_INTERRUPTED,
        ) {
            Ok(id) => id,
            Err(_) => return Ok(false), // unreadable, or Ctrl-C
        }
    } else if entry.mode == Mode::SYMLINK {
        use std::os::unix::ffi::OsStrExt;
        let Ok(target) = std::fs::read_link(&path) else { return Ok(false) };
        gix::objs::compute_hash(hash_kind, gix::objs::Kind::Blob, target.as_os_str().as_bytes())
            .with_context(|| "failed to hash symlink target")?
    } else {
        return Ok(false);
    };
    Ok(id == entry.id)
}

/// Mark every entry of `new` that is already right on disk — same path,
/// blob and mode in `old`, an index the probe just found clean — so the
/// checkout skips it (`SKIP_WORKTREE`, honored by gix's checkout), and carry
/// its recorded stat over so status keeps matching it by stat alone. An entry
/// that was racy against `old` (modified in the same second `old` was written,
/// so its stat cannot vouch for its content) is rewritten instead: copying
/// that stat into a newer index would let a same-second, same-size edit hide.
fn keep_unchanged_entries(new: &mut gix::index::File, old: &gix::index::File) {
    use gix::index::entry::Flags;
    let old_written = old.timestamp();
    let stat_options = gix::index::entry::stat::Options::default();
    for (entry, path) in new.entries_mut_with_paths() {
        let Some(previous) = old.entry_by_path(path) else { continue };
        // An entry whose flags make its stat say nothing about the file on
        // disk (sparse, assume-unchanged, intent-to-add) is checked out
        // normally instead.
        let unreliable = Flags::SKIP_WORKTREE | Flags::ASSUME_VALID | Flags::INTENT_TO_ADD;
        if previous.id == entry.id
            && previous.mode == entry.mode
            && previous.stage_raw() == 0
            && !previous.flags.intersects(unreliable)
            && !previous.stat.is_racy(old_written, stat_options)
        {
            entry.stat = previous.stat;
            entry.flags.insert(Flags::SKIP_WORKTREE);
        }
    }
}

/// Rewrite `origin`'s fetch URL to `url`, persistently, in `<git_dir>/config`.
///
/// This is the heal path for `DirtyReason::RemoteUrlChanged` under a forced
/// refetch: [`align`] fetches from whatever `origin` names, and the plan
/// item's wanted URL is otherwise used only for [`clone`]. Without this, a
/// forced refetch of a re-pointed repo fetches the *old* upstream and dies
/// the moment the wanted branch is missing there. Persisting the change
/// (rather than fetching from an anonymous, one-off remote) is what stops
/// the very next probe from flagging the repo again.
///
/// `gix`'s `Repository::config_snapshot_mut` is in-memory only and never
/// reaches disk, so this edits `config` directly and writes it back with the
/// same temp-file-then-persist pattern as `FetchState::record`
/// (src/fetch/mod.rs) — a git config is a file every other git-aware tool
/// reads too, so a crash here must never leave a half-written one behind.
pub fn set_origin_url(repo_dir: &Path, url: &str) -> Res<()> {
    let repo = gix::open(repo_dir)
        .with_context(|| format!("Failed to open {}", repo_dir.display()))?;
    let git_dir = repo.git_dir().to_owned();
    let config_path = git_dir.join("config");

    let mut file =
        gix::config::File::from_path_no_includes(config_path.clone(), gix::config::Source::Local)
            .with_context(|| format!("Failed to read {}", config_path.display()))?;
    file.set_raw_value_by("remote", Some("origin".into()), "url", gix::bstr::BStr::new(url.as_bytes()))
        .with_context(|| format!("Failed to set remote.origin.url in {}", config_path.display()))?;

    let tmp = tempfile::NamedTempFile::new_in(&git_dir)
        .with_context(|| format!("Failed to create temp file in {}", git_dir.display()))?;
    {
        let mut writer = std::io::BufWriter::new(tmp.as_file());
        file.write_to(&mut writer)
            .with_context(|| "Failed to render updated git config")?;
        std::io::Write::flush(&mut writer).with_context(|| "Failed to flush git config temp file")?;
    }
    tmp.persist(&config_path)
        .with_context(|| format!("Failed to replace {}", config_path.display()))?;
    Ok(())
}

/// gix status threads per walk inside a [`crate::par::parallel_map`] fan-out.
const FAN_OUT_STATUS_THREADS: usize = 4;

/// Whether a status walk also lists untracked files.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Untracked {
    /// Walk every directory of the worktree for files git does not know.
    List,
    /// Skip that walk: the caller would throw the list away.
    Skip,
}

/// `(modified, untracked)` for a repo. Modified = tracked files differing from
/// HEAD or the index (staged *and* unstaged); untracked = files git does not
/// know about (always empty with [`Untracked::Skip`]). Shared by [`probe`] and
/// [`source_diff`], which need the same walk but draw different conclusions
/// from it.
fn status_paths(repo: &gix::Repository, untracked_files: Untracked) -> Res<(Vec<String>, Vec<String>)> {
    status_paths_until(repo, untracked_files, &gix::interrupt::IS_INTERRUPTED)
}

/// [`status_paths`], stopping when `should_interrupt` is set. A walk cut short
/// has not seen every file, so it fails rather than return a list that would
/// read as "nothing else changed".
fn status_paths_until(
    repo: &gix::Repository,
    untracked_files: Untracked,
    should_interrupt: &'static std::sync::atomic::AtomicBool,
) -> Res<(Vec<String>, Vec<String>)> {
    let walk_span = crate::timing::span("gix status walk");
    let mut modified = Vec::new();
    let mut untracked = Vec::new();
    let mut platform = repo
        .status(gix::progress::Discard)
        .with_context(|| "failed to prepare status")?
        .should_interrupt_shared(should_interrupt);
    if untracked_files == Untracked::Skip {
        platform = platform.untracked_files(gix::status::UntrackedFiles::None);
    }
    // Inside a fan-out over repos the walks already run side by side, so
    // gix's own per-walk pool (as wide as the machine: ~100 threads per walk
    // on a big node, times the fan-out) only adds threads. A few are kept so
    // that one large repo finishing last still overlaps its lstats.
    if crate::par::in_fan_out() {
        platform = platform
            .index_worktree_options_mut(|o| o.thread_limit = Some(FAN_OUT_STATUS_THREADS));
    }
    let mut iter =
        platform.into_iter(Vec::<BString>::new()).with_context(|| "failed to run status")?;
    for item in iter.by_ref() {
        let item = match item {
            Ok(item) => item,
            // gix ends an interrupted walk with an error item; say what it is.
            Err(_) if should_interrupt.load(std::sync::atomic::Ordering::Relaxed) => bail!("interrupted"),
            Err(e) => return Err(e).with_context(|| "status iteration failed"),
        };
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
    if should_interrupt.load(std::sync::atomic::Ordering::Relaxed) {
        bail!("interrupted");
    }
    drop(walk_span);
    if crate::timing::enabled()
        && let Some(outcome) = iter.outcome_mut()
    {
        use crate::timing::count;
        let tracked = &outcome.index_worktree.tracked_file_modification;
        count("status: entries", tracked.entries_processed as u64);
        count("status: lstat calls", tracked.symlink_metadata_calls as u64);
        count("status: racy-clean entries", tracked.racy_clean as u64);
        count("status: files read (hashed)", tracked.worktree_files_read as u64);
        count("status: bytes read", tracked.worktree_bytes);
        if let Some(dirwalk) = &outcome.index_worktree.dirwalk {
            count("status: dirwalk read_dir calls", dirwalk.read_dir_calls as u64);
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
    // The untracked list plays no part in the state, so skip the directory
    // walk that would find it.
    Ok(source_diff_walking(repo_dir, Untracked::Skip)?.state())
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
    source_diff_walking(repo_dir, Untracked::List)
}

fn source_diff_walking(repo_dir: &Path, untracked_files: Untracked) -> Res<SourceDiff> {
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
    let (modified, untracked) = status_paths(&repo, untracked_files)?;
    let newest_mtime = modified
        .iter()
        .filter_map(|rel| std::fs::metadata(repo_dir.join(rel)).ok()?.modified().ok())
        .filter_map(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos())
        .max()
        .unwrap_or(0);
    Ok(SourceDiff { head, branch, modified, untracked, newest_mtime })
}

/// The branch and commit a repo is currently on, for post-pass assertions and
/// `fetch-state.toml`.
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

    /// The fork-adoption workflow: a thornlist re-pointing a repo at an
    /// `ssh://` fork of the same project must not read as a different repo
    /// just because of URL spelling (scheme, userinfo, port, `.git`, trailing
    /// slash, host case all vary across the forms people paste).
    #[test]
    fn url_normalization_treats_scp_and_url_forms_as_the_same_repo() {
        let scp = normalize_url("git@github.com:max-morris/SpacetimeX.git");
        let https = normalize_url("https://github.com/max-morris/SpacetimeX");
        let ssh_with_port = normalize_url("ssh://git@github.com:22/max-morris/SpacetimeX/");
        let https_upper_host = normalize_url("https://GitHub.com/max-morris/SpacetimeX.git");
        assert_eq!(scp, https);
        assert_eq!(https, ssh_with_port);
        assert_eq!(ssh_with_port, https_upper_host);

        // Different owner is a genuinely different repo, not a spelling
        // variant — the path is compared verbatim past the host.
        assert_ne!(
            normalize_url("https://github.com/max-morris/SpacetimeX"),
            normalize_url("https://github.com/EinsteinToolkit/SpacetimeX"),
        );

        // Path case is left alone (only the host is lowercased): some forges
        // have case-sensitive repo/owner names, so folding case here could
        // conflate two distinct repos.
        assert_ne!(normalize_url("https://x.org/repo"), normalize_url("https://x.org/Repo"));

        // Local paths compare by path alone, with no host to normalize.
        assert_eq!(normalize_url("/srv/mirrors/repo.git"), normalize_url("/srv/mirrors/repo"));

        assert_eq!(normalize_url(""), "");
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

    /// A repo that is both retargeted (thornlist wants a different origin)
    /// and locally edited must report the edit — the refetch backup pass
    /// needs `modified` to save it before a forced refetch clobbers the
    /// worktree. Before this fix `probe_inner` returned on the URL mismatch
    /// before ever walking the worktree, so `modified` was always empty.
    #[test]
    fn remote_url_changed_carries_modified_files() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        testrepo::init(&dir);
        testrepo::commit_file(&dir, "thorn.cc", "int a;\n");
        set_origin_url(&dir, "https://old.example.com/owner/repo.git").unwrap();
        std::fs::write(dir.join("thorn.cc"), "int a; int b;\n").unwrap();

        let probe = probe(&dir, "https://new.example.com/owner/repo.git", "");
        match probe.state {
            RepoState::Dirty(DirtyReason::RemoteUrlChanged { on_disk, wanted, modified }) => {
                assert_eq!(on_disk, "https://old.example.com/owner/repo.git");
                assert_eq!(wanted, "https://new.example.com/owner/repo.git");
                assert_eq!(modified, vec!["thorn.cc".to_owned()]);
            }
            other => panic!("expected RemoteUrlChanged with a modification, got {other:?}"),
        }
    }

    /// The heal path: rewriting `origin` persists, is visible to a fresh
    /// `gix::open`, and setting it twice never leaves a duplicate `url` key
    /// behind (which would otherwise make the "current" URL ambiguous to
    /// every other git-aware tool reading the same config).
    #[test]
    fn set_origin_url_persists_and_does_not_duplicate() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("repo");
        testrepo::init(&dir);

        set_origin_url(&dir, "https://first.example.com/a/b.git").unwrap();
        let repo = gix::open(&dir).expect("reopen after first set_origin_url");
        let url = repo
            .find_remote("origin")
            .expect("origin exists after set_origin_url")
            .url(Direction::Fetch)
            .expect("origin has a fetch url")
            .to_bstring()
            .to_string();
        assert_eq!(url, "https://first.example.com/a/b.git");

        set_origin_url(&dir, "https://second.example.com/c/d.git").unwrap();
        let repo = gix::open(&dir).expect("reopen after second set_origin_url");
        let url = repo
            .find_remote("origin")
            .expect("origin exists after second set_origin_url")
            .url(Direction::Fetch)
            .expect("origin has a fetch url")
            .to_bstring()
            .to_string();
        assert_eq!(url, "https://second.example.com/c/d.git");

        // No duplicate `url` line left behind by the first write.
        let config_text = std::fs::read_to_string(dir.join(".git/config")).unwrap();
        assert_eq!(
            config_text.matches("url =").count(),
            1,
            "expected exactly one url entry, got:\n{config_text}"
        );
        assert!(!config_text.contains("first.example.com"));
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

#[cfg(test)]
mod align_tests {
    use super::*;
    use std::path::PathBuf;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::time::{Duration, SystemTime};

    /// An upstream with `files`, cloned into `<tmp>/clone`; returns the
    /// upstream dir, the clone dir and the branch.
    fn upstream_and_clone(tmp: &Path, files: &[(&str, &str, bool)]) -> (PathBuf, PathBuf, String) {
        let upstream = tmp.join("upstream");
        testrepo::init(&upstream);
        testrepo::commit_tree(&upstream, files);
        let (branch, _) = head_of(&upstream).unwrap();
        let clone_dir = tmp.join("clone");
        let mut progress = prodash::tree::Root::new().add_child("test clone");
        clone(&upstream.to_string_lossy(), Some(&branch), &clone_dir, &mut progress).unwrap();
        (upstream, clone_dir, branch)
    }

    fn align_now(dir: &Path, branch: &str, force: bool) -> ObjectId {
        let mut progress = prodash::tree::Root::new().add_child("test align");
        align(dir, branch, force, &mut progress).unwrap()
    }

    /// (mtime, inode) — a rewrite moves the first, a replace the second.
    fn identity(path: &Path) -> (SystemTime, u64) {
        let meta = std::fs::symlink_metadata(path).unwrap();
        (meta.modified().unwrap(), meta.ino())
    }

    fn set_mtime(path: &Path, when: SystemTime) {
        std::fs::File::options().write(true).open(path).unwrap().set_modified(when).unwrap();
    }

    /// Back-date every worktree file a few seconds (and re-record the index),
    /// so a later rewrite is visible in the mtime regardless of timestamp
    /// granularity.
    fn backdate(dir: &Path, names: &[&str]) {
        let past = SystemTime::now() - Duration::from_secs(30);
        for name in names {
            set_mtime(&dir.join(name), past);
        }
        let repo = gix::open(dir).unwrap();
        let mut index = repo.open_index().unwrap();
        for (entry, path) in index.entries_mut_with_paths() {
            let file = dir.join(path.to_str_lossy().as_ref());
            let meta = gix::index::fs::Metadata::from_path_no_follow(&file).unwrap();
            entry.stat = gix::index::entry::Stat::from_fs(&meta).unwrap();
        }
        index.write(Default::default()).unwrap();
    }

    #[test]
    fn align_at_the_tip_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let files = [("a", "alpha\n", false), ("b", "beta\n", false)];
        let (_, dir, branch) = upstream_and_clone(tmp.path(), &files);
        backdate(&dir, &["a", "b"]);
        let identities = || ["a", "b", ".git/index"].map(|name| identity(&dir.join(name)));
        let before = identities();
        let (_, head) = head_of(&dir).unwrap();
        assert_eq!(align_now(&dir, &branch, false), head);
        let after = identities();
        assert_eq!(before, after, "an up-to-date repo must not be touched");
    }

    #[test]
    fn align_rewrites_only_what_changed() {
        let tmp = tempfile::tempdir().unwrap();
        let (upstream, dir, branch) = upstream_and_clone(
            tmp.path(),
            &[("a", "alpha\n", false), ("b", "beta\n", false), ("c", "gamma\n", false)],
        );
        backdate(&dir, &["a", "b", "c"]);
        let a_before = identity(&dir.join("a"));
        // Upstream: `b`'s content changes, `c` becomes executable, `a` stays.
        let changed = [("a", "alpha\n", false), ("b", "BETA\n", false), ("c", "gamma\n", true)];
        let tip = testrepo::commit_tree(&upstream, &changed);
        assert_eq!(align_now(&dir, &branch, false), tip);
        assert_eq!(identity(&dir.join("a")), a_before, "an unchanged file must be left alone");
        assert_eq!(std::fs::read_to_string(dir.join("b")).unwrap(), "BETA\n");
        assert_ne!(std::fs::metadata(dir.join("c")).unwrap().permissions().mode() & 0o111, 0);
        assert_clean_by_stat(&dir, tip);

        // And back: gix never clears an executable bit itself, so 755 → 644
        // is align's own chmod — after which the recorded stat (ctime
        // included) must still match the file.
        let back = [("a", "alpha\n", false), ("b", "BETA\n", false), ("c", "gamma\n", false)];
        let tip = testrepo::commit_tree(&upstream, &back);
        assert_eq!(align_now(&dir, &branch, false), tip);
        assert_eq!(std::fs::metadata(dir.join("c")).unwrap().permissions().mode() & 0o111, 0);
        assert_clean_by_stat(&dir, tip);
    }

    /// Status walks are read-only (the index is never written back) and list
    /// untracked files only when asked; skipping that walk changes nothing
    /// about the tracked changes reported.
    #[test]
    fn status_walks_are_read_only_and_skip_the_untracked_walk_on_request() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, dir, _) = upstream_and_clone(tmp.path(), &[("a", "alpha\n", false)]);
        std::fs::write(dir.join("output.log"), "test output\n").unwrap();
        let index_before = identity(&dir.join(".git/index"));
        let repo = gix::open(&dir).unwrap();

        let (modified, untracked) = status_paths(&repo, Untracked::List).unwrap();
        assert!(modified.is_empty());
        assert_eq!(untracked, ["output.log"]);
        let (modified, untracked) = status_paths(&repo, Untracked::Skip).unwrap();
        assert!(modified.is_empty() && untracked.is_empty());
        // `delta` still sees untracked files; the state string never did.
        assert_eq!(source_diff(&dir).unwrap().untracked, ["output.log"]);
        let (_, head) = head_of(&dir).unwrap();
        assert_eq!(source_state(&dir).unwrap(), head.to_string());
        // Inside a fan-out (two items: one would not be one), too.
        let dirs = [dir.clone(), dir.clone()];
        let states = crate::par::parallel_map(&dirs, |d| source_state(d).unwrap()).unwrap();
        assert_eq!(states, [head.to_string(), head.to_string()]);

        assert_eq!(identity(&dir.join(".git/index")), index_before, "status must not write the index");

        // A dirty repo: an edited file, a deleted one, a new untracked one.
        // Both walks report the same tracked changes.
        let files = [("a", "alpha\n", false), ("b", "beta\n", false), ("c", "gamma\n", false)];
        let tmp2 = tempfile::tempdir().unwrap();
        let (_, dirty, _) = upstream_and_clone(tmp2.path(), &files);
        std::fs::write(dirty.join("a"), "edited\n").unwrap();
        std::fs::remove_file(dirty.join("b")).unwrap();
        std::fs::write(dirty.join("b-renamed"), "beta\n").unwrap();
        let repo = gix::open(&dirty).unwrap();
        let (listed, untracked) = status_paths(&repo, Untracked::List).unwrap();
        let (skipped, _) = status_paths(&repo, Untracked::Skip).unwrap();
        assert_eq!(listed, ["a", "b"]);
        assert_eq!(untracked, ["b-renamed"]);
        assert_eq!(skipped, listed);
    }

    /// Set from the start, so every interruptible step sees Ctrl-C at once —
    /// without touching the process-wide flag the other tests share.
    static INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

    /// A status walk under a set interrupt flag fails instead of returning a
    /// list that would read as "nothing else changed". (Whether gix itself
    /// stops early is gix's business; this pins cactup's verdict.)
    #[test]
    fn an_interrupted_status_walk_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, dir, _) = upstream_and_clone(tmp.path(), &[("a", "alpha\n", false)]);
        std::fs::write(dir.join("a"), "edited\n").unwrap();
        let repo = gix::open(&dir).unwrap();
        let err = status_paths_until(&repo, Untracked::List, &INTERRUPTED).unwrap_err();
        assert_eq!(err.to_string(), "interrupted");
    }

    /// A checkout cut short by Ctrl-C (gix returns `Ok` for it) is reported
    /// as interrupted and leaves the old index, so under the moved HEAD the
    /// repo reads as modified; the forced refetch the message suggests then
    /// finishes it — including deleting the file the new tree dropped, which
    /// it can only find in that old index.
    #[test]
    fn an_interrupted_checkout_fails_and_a_forced_one_finishes_it() {
        static NOT_INTERRUPTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        let tmp = tempfile::tempdir().unwrap();
        let files = [("a", "alpha\n", false), ("gone", "x\n", false)];
        let (_, dir, _) = upstream_and_clone(tmp.path(), &files);
        let repo = gix::open(&dir).unwrap();
        // A new commit straight into the clone, moving HEAD the way align's
        // ref edit does before it checks out: `a` changes, `gone` is dropped,
        // `b` is new.
        let mut tree = gix::objs::Tree::empty();
        for (name, content) in [("a", "ALPHA\n"), ("b", "beta\n")] {
            let oid = repo.write_blob(content.as_bytes()).unwrap().detach();
            let mode = gix::objs::tree::EntryKind::Blob.into();
            tree.entries.push(gix::objs::tree::Entry { mode, filename: name.into(), oid });
        }
        let tree_id = repo.write_object(&tree).unwrap().detach();
        let parent = repo.head_id().unwrap().detach();
        let tip = repo.commit("HEAD", "new tree", tree_id, [parent]).unwrap().detach();
        let paths_of = |index: &gix::index::File| -> Vec<BString> {
            index.entries().iter().map(|e| e.path(index).to_owned()).collect()
        };
        let old = repo.open_index().unwrap();
        let old_paths = paths_of(&old);
        let index_before = identity(&dir.join(".git/index"));
        let mut progress = prodash::tree::Root::new().add_child("test checkout");

        let err = check_out_tree(&repo, tree_id, Some(&old), &old_paths, &mut progress, &INTERRUPTED)
            .unwrap_err();
        assert!(err.to_string().starts_with(INTERRUPTED_CHECKOUT), "{err}");
        // Nothing checked out, nothing swept, the index untouched…
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "alpha\n");
        assert!(dir.join("gone").exists() && !dir.join("b").exists());
        assert_eq!(identity(&dir.join(".git/index")), index_before);
        // …so under the new HEAD the repo reads as modified.
        assert!(source_state(&dir).unwrap().contains("mod@"));

        // The forced recovery: full checkout, sweeping by the old index.
        let repo = gix::open(&dir).unwrap();
        let old_paths = paths_of(&repo.open_index().unwrap());
        check_out_tree(&repo, tree_id, None, &old_paths, &mut progress, &NOT_INTERRUPTED).unwrap();
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "ALPHA\n");
        assert!(dir.join("b").exists());
        assert!(!dir.join("gone").exists(), "the dropped file must be swept");
        assert_eq!(source_state(&dir).unwrap(), tip.to_string());
    }

    /// What an align cut short by Ctrl-C leaves behind — the branch moved
    /// with align's reflog message, the checkout only partly done, the old
    /// index — probes as an unfinished checkout with nothing to back up. A
    /// real edit on top makes it ordinary local modifications again, and only
    /// that edit needs backing up.
    #[test]
    fn an_interrupted_align_probes_as_an_unfinished_checkout() {
        let tmp = tempfile::tempdir().unwrap();
        let files = [("a", "alpha\n", false), ("b", "beta\n", false)];
        let (upstream, dir, branch) = upstream_and_clone(tmp.path(), &files);
        let url = upstream.to_string_lossy().into_owned();
        let mut repo = gix::open(&dir).unwrap();
        let _ = repo.committer_or_set_generic_fallback();
        // The new tip: `a` and `b` both change.
        let mut tree = gix::objs::Tree::empty();
        for (name, content) in [("a", "ALPHA\n"), ("b", "BETA\n")] {
            let oid = repo.write_blob(content.as_bytes()).unwrap().detach();
            let mode = gix::objs::tree::EntryKind::Blob.into();
            tree.entries.push(gix::objs::tree::Entry { mode, filename: name.into(), oid });
        }
        let tree_id = repo.write_object(&tree).unwrap().detach();
        let parent = repo.head_id().unwrap().detach();
        let tip = repo.new_commit("tip", tree_id, [parent]).unwrap().id;
        // What align does before its checkout: the marker, then the ref move.
        write_align_marker(&repo, Some(parent), tip).unwrap();
        let branch_ref = format!("refs/heads/{branch}");
        let any = gix::refs::transaction::PreviousValue::Any;
        repo.reference(branch_ref.as_str(), tip, any, ALIGN_REFLOG_MESSAGE).unwrap();
        // The checkout got as far as `a`, then Ctrl-C.
        std::fs::write(dir.join("a"), "ALPHA\n").unwrap();

        let probe = probe(&dir, &url, &branch);
        let RepoState::Dirty(DirtyReason::UnfinishedCheckout(paths)) = &probe.state else {
            panic!("expected an unfinished checkout, got {:?}", probe.state);
        };
        assert!(local_edits(&dir, paths).is_empty(), "nothing of the user's is there");

        // A real edit on top: ordinary modifications, and just that file.
        std::fs::write(dir.join("b"), "my edit\n").unwrap();
        let probe = super::probe(&dir, &url, &branch);
        let RepoState::Dirty(DirtyReason::WorktreeModified(paths)) = &probe.state else {
            panic!("expected local modifications, got {:?}", probe.state);
        };
        assert_eq!(local_edits(&dir, paths), ["b"]);
    }

    /// After an align that finished, nothing is "interrupted": a user who
    /// restores a file's pre-update version, or only changes its mode, has
    /// made a local edit that a forced refetch must back up first.
    #[test]
    fn after_a_completed_align_old_content_and_mode_changes_are_edits() {
        let tmp = tempfile::tempdir().unwrap();
        let files = [("a", "alpha\n", false), ("b", "beta\n", false)];
        let (upstream, dir, branch) = upstream_and_clone(tmp.path(), &files);
        let url = upstream.to_string_lossy().into_owned();
        testrepo::commit_tree(&upstream, &[("a", "ALPHA\n", false), ("b", "beta\n", false)]);
        align_now(&dir, &branch, false);
        assert!(!dir.join(".git/cactup-align").exists(), "a finished align leaves no marker");

        std::fs::write(dir.join("a"), "alpha\n").unwrap(); // back to the old version
        std::fs::set_permissions(dir.join("b"), std::fs::Permissions::from_mode(0o755)).unwrap();
        let probe = probe(&dir, &url, &branch);
        let RepoState::Dirty(DirtyReason::WorktreeModified(paths)) = &probe.state else {
            panic!("expected local modifications, got {:?}", probe.state);
        };
        assert_eq!(local_edits(&dir, paths), ["a", "b"]);
    }

    /// `source_state` really skips the untracked walk: an unreadable untracked
    /// directory breaks the walk (`source_diff` fails) but not the state.
    #[test]
    fn source_state_does_not_walk_untracked_directories() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, dir, _) = upstream_and_clone(tmp.path(), &[("a", "alpha\n", false)]);
        let locked = dir.join("unreadable");
        std::fs::create_dir(&locked).unwrap();
        std::fs::write(locked.join("x"), "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Running as root: permissions do not bind, nothing to show.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let diff = source_diff(&dir);
        let state = source_state(&dir);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(diff.is_err(), "the untracked walk should trip over the unreadable directory");
        let (_, head) = head_of(&dir).unwrap();
        assert_eq!(state.unwrap(), head.to_string());
    }

    /// The chmod's ctime is re-recorded. (Inside a real align the chmod
    /// usually lands in the same coarse kernel clock tick as the checkout's
    /// write, so only a pause makes a missing re-read visible.)
    #[test]
    fn reconcile_exec_bits_rerecords_the_stat_it_moves() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, dir, _) = upstream_and_clone(tmp.path(), &[("c", "gamma\n", false)]);
        let c = dir.join("c");
        std::fs::set_permissions(&c, std::fs::Permissions::from_mode(0o755)).unwrap();
        let repo = gix::open(&dir).unwrap();
        let mut index = repo.open_index().unwrap();
        let stat_of = |path: &Path| {
            let meta = gix::index::fs::Metadata::from_path_no_follow(path).unwrap();
            gix::index::entry::Stat::from_fs(&meta).unwrap()
        };
        index.entries_mut()[0].stat = stat_of(&c);
        std::thread::sleep(Duration::from_millis(50));
        reconcile_exec_bits(&mut index, &dir);
        assert_eq!(std::fs::metadata(&c).unwrap().permissions().mode() & 0o111, 0);
        assert_eq!(index.entries()[0].stat, stat_of(&c), "the chmod's ctime must be recorded");
    }

    /// The repo reads clean at `tip`, every entry's recorded stat matches its
    /// file exactly (so status never has to hash it), and no skip flag is
    /// left behind.
    fn assert_clean_by_stat(dir: &Path, tip: ObjectId) {
        assert_eq!(source_state(dir).unwrap(), tip.to_string());
        let index = gix::open(dir).unwrap().open_index().unwrap();
        for entry in index.entries() {
            let file = dir.join(entry.path(&index).to_str_lossy().as_ref());
            let meta = gix::index::fs::Metadata::from_path_no_follow(&file).unwrap();
            let on_disk = gix::index::entry::Stat::from_fs(&meta).unwrap();
            assert_eq!(entry.stat, on_disk, "{} has a stale stat", file.display());
            assert!(!entry.flags.contains(gix::index::entry::Flags::SKIP_WORKTREE));
        }
    }

    /// The forced path (a backed-up dirty repo, or a repointed one) keeps
    /// the full overwrite: it is what discards the local edit.
    #[test]
    fn forced_align_at_the_tip_restores_a_modified_file() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, dir, branch) = upstream_and_clone(tmp.path(), &[("a", "alpha\n", false)]);
        std::fs::write(dir.join("a"), "local edit\n").unwrap();
        align_now(&dir, &branch, true);
        assert_eq!(std::fs::read_to_string(dir.join("a")).unwrap(), "alpha\n");
    }

    /// Make `dir`'s index racy: its mtime set to the newest entry's second.
    fn make_racy(dir: &Path) {
        let index = gix::open(dir).unwrap().open_index().unwrap();
        let newest = index.entries().iter().map(|e| e.stat.mtime.secs).max().unwrap();
        set_mtime(&dir.join(".git/index"), SystemTime::UNIX_EPOCH + Duration::from_secs(newest.into()));
    }

    fn racy_entries(dir: &Path) -> usize {
        let index = gix::open(dir).unwrap().open_index().unwrap();
        index.entries().iter().filter(|e| is_racy_in(e, &index)).count()
    }

    #[test]
    fn settle_rewrites_a_racy_index_and_status_stops_hashing() {
        let tmp = tempfile::tempdir().unwrap();
        let files = [("a", "alpha\n", false), ("b", "beta\n", false)];
        let (_, dir, _) = upstream_and_clone(tmp.path(), &files);
        make_racy(&dir);
        assert_eq!(racy_entries(&dir), 2);
        assert_eq!(settle_index(&dir).unwrap(), Settled::Rewritten);
        assert_eq!(racy_entries(&dir), 0);
        let (_, head) = head_of(&dir).unwrap();
        assert_eq!(source_state(&dir).unwrap(), head.to_string());
        // Settled once, it stays settled.
        assert_eq!(settle_index(&dir).unwrap(), Settled::Clean);
    }

    #[test]
    fn settle_leaves_an_index_alone_unless_it_can_vouch_for_it() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, dir, _) = upstream_and_clone(tmp.path(), &[("a", "alpha\n", false)]);
        let index_path = dir.join(".git/index");

        // Not racy: nothing written.
        set_mtime(&index_path, SystemTime::now() + Duration::from_secs(60));
        let before = identity(&index_path);
        assert_eq!(settle_index(&dir).unwrap(), Settled::Clean);
        assert_eq!(identity(&index_path), before);

        // Racy, but the file's content no longer matches its entry.
        make_racy(&dir);
        std::fs::write(dir.join("a"), "ALPHA\n").unwrap();
        let before = identity(&index_path);
        assert_eq!(settle_index(&dir).unwrap(), Settled::Left);
        assert_eq!(identity(&index_path), before);
        std::fs::write(dir.join("a"), "alpha\n").unwrap();

        // Racy, but git holds the index lock.
        std::fs::write(dir.join(".git/index.lock"), "").unwrap();
        assert_eq!(settle_index(&dir).unwrap(), Settled::Left);
        assert_eq!(identity(&index_path), before);
        assert!(dir.join(".git/index.lock").exists(), "someone else's lock must survive");
        std::fs::remove_file(dir.join(".git/index.lock")).unwrap();
    }

    #[test]
    fn settle_ignores_future_dated_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let (_, dir, _) = upstream_and_clone(tmp.path(), &[("a", "alpha\n", false)]);
        let repo = gix::open(&dir).unwrap();
        let mut index = repo.open_index().unwrap();
        let future = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs() + 3600;
        for entry in index.entries_mut() {
            entry.stat.mtime.secs = future as u32;
        }
        index.write(Default::default()).unwrap();
        let before = identity(&dir.join(".git/index"));
        let started = std::time::Instant::now();
        assert_eq!(settle_index(&dir).unwrap(), Settled::Left);
        assert_eq!(identity(&dir.join(".git/index")), before, "no rewrite can settle a future entry");
        // Recognized up front, not after waiting out seconds for it.
        assert!(started.elapsed() < Duration::from_millis(500), "{:?}", started.elapsed());
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

    /// Commit a flat tree of `(name, content, executable)` files on HEAD —
    /// objects and refs only, no worktree checkout: an upstream for
    /// [`super::clone`] and [`super::align`] to fetch from.
    pub(crate) fn commit_tree(dir: &Path, files: &[(&str, &str, bool)]) -> ObjectId {
        let repo = gix::open(dir).expect("open repo");
        let mut tree = gix::objs::Tree::empty();
        for (name, content, executable) in files {
            let blob = repo.write_blob(content.as_bytes()).expect("write blob").detach();
            let kind = if *executable {
                gix::objs::tree::EntryKind::BlobExecutable
            } else {
                gix::objs::tree::EntryKind::Blob
            };
            let entry = gix::objs::tree::Entry { mode: kind.into(), filename: (*name).into(), oid: blob };
            tree.entries.push(entry);
        }
        tree.entries.sort();
        let tree_id = repo.write_object(&tree).expect("write tree").detach();
        let parents: Vec<ObjectId> = repo
            .head()
            .ok()
            .and_then(|mut h| h.peel_to_commit().ok())
            .map(|c| vec![c.id])
            .unwrap_or_default();
        repo.commit("HEAD", "tree", tree_id, parents).expect("commit").detach()
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
