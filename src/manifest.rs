use std::fs;
use std::path::Path;
use std::sync::Arc;
use gix::{Reference, Repository};
use anyhow::{anyhow, Context};
use gix::protocol::fetch::Tags;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use gix::remote::Direction;
use prodash::render::line::{JoinHandle, StreamKind};
use prodash::Root;
use crate::Res;

type ProgressHandle = Arc<prodash::tree::Root>;

pub(crate) fn setup_prodash() -> (ProgressHandle, JoinHandle) {
    setup_prodash_with(None, false)
}

/// Like [`setup_prodash`], but with no renderer at all when stderr is not a
/// terminal: bars would not draw there anyway, and the render thread's
/// shutdown writes a clear-line escape even to a pipe (job logs, captured
/// test output). Only for phases whose items never call `info`/`fail` —
/// the renderer DOES print those on a non-tty, so they would be lost here.
pub(crate) fn setup_prodash_if_tty() -> (ProgressHandle, Option<JoinHandle>) {
    use std::io::IsTerminal;
    if std::io::stderr().is_terminal() {
        let (progress, renderer) = setup_prodash();
        (progress, Some(renderer))
    } else {
        (prodash::tree::Root::new(), None)
    }
}

/// Like [`setup_prodash`], but lets a caller cap which tree levels the
/// renderer draws (e.g. hiding gix's per-thread delta-resolution children)
/// and opt into throughput display (needed for byte-unit progress like
/// downloads). Every renderer gets a 500ms initial delay regardless — a run
/// that finishes (or finds nothing to do) inside that window never flashes a
/// bar at all.
pub(crate) fn setup_prodash_with(
    level_filter: Option<std::ops::RangeInclusive<prodash::progress::key::Level>>,
    throughput: bool,
) -> (ProgressHandle, JoinHandle) {
    let progress = prodash::tree::Root::new();

    let progress_renderer_options = prodash::render::line::Options {
        frames_per_second: 6.0,
        hide_cursor: true, // signal-hook feature restores cursor on exit
        level_filter,
        throughput,
        initial_delay: Some(std::time::Duration::from_millis(500)),
        ..Default::default()
    }.auto_configure(StreamKind::Stderr);

    let progress_renderer = prodash::render::line::render(
        std::io::stderr(),
        progress.downgrade(),
        progress_renderer_options
    );

    (progress, progress_renderer)
}

/// `1..=default_max`, overridable via `CACTUP_PROGRESS_DEPTH` (any `u8 >=
/// 1`) for progress-UX tuning/experiments — e.g. peeking at gix's per-thread
/// noise without recompiling. Env-only on purpose, not a durable flag.
/// Shared by every `setup_prodash_with` call site so they all respond to the
/// same knob.
pub(crate) fn progress_level_filter(
    default_max: prodash::progress::key::Level,
) -> std::ops::RangeInclusive<prodash::progress::key::Level> {
    let max = std::env::var("CACTUP_PROGRESS_DEPTH")
        .ok()
        .and_then(|v| v.parse::<u8>().ok())
        .filter(|&n| n >= 1)
        .unwrap_or(default_max);
    1..=max
}

/// How deep [`ensure_manifest_repo`]'s renderer draws: one level, the single
/// "release manifest" line. Everything gix would report below it (the phase
/// status line it renames as it goes, the per-phase bars, the per-thread
/// delta-resolution workers) is collapsed onto that line by
/// [`crate::progress::Line`], which also keeps gix's throughput chatter out
/// of the scrollback.
const MANIFEST_PROGRESS_MAX_LEVEL: prodash::progress::key::Level = 1;

pub fn ensure_manifest_repo(cactup_root: &Path, manifest_url: &str) -> Res<Repository> {
    let (progress, progress_renderer) = setup_prodash_with(
        Some(progress_level_filter(MANIFEST_PROGRESS_MAX_LEVEL)),
        true,
    );

    let manifest_dir = cactup_root.join("manifest");
    const HEADLINE: &str = "release manifest";
    let layout = crate::progress::Layout::for_names([HEADLINE]);
    let mut headline = crate::progress::Line::over(progress.add_child(HEADLINE), HEADLINE, layout);

    if manifest_dir.exists() && !manifest_dir.is_dir() {
        return Err(anyhow!("Manifest directory exists but is not a directory"));
    }

    let need_clone = !manifest_dir.exists()
        || manifest_dir.read_dir()
                       .map(|mut d| d.next().is_none()) // Checking for empty dir
                       .unwrap_or(true);

    if need_clone {
        headline.phase("fetching for the first time");

        fs::create_dir_all(&manifest_dir)
           .with_context(|| "Failed to create manifest directory")?;

        let mut prepare =
            gix::prepare_clone(manifest_url, &manifest_dir)?
                .configure_remote(|r| {
                    Ok(r.with_fetch_tags(Tags::All))
                });

        // gix renames whatever item it's given as the fetch moves through
        // phases and hangs its own bars off it; the Line keeps all of that
        // on the one headline, so the label follows the phase and the bar
        // never splits in two.
        let (repo, _) = prepare.fetch_only(
            &mut headline,
            &gix::interrupt::IS_INTERRUPTED
        ).with_context(|| "Failed to fetch manifest")?;

        headline.succeeded("fetched");
        progress_renderer.shutdown_and_wait();
        Ok(repo)
    } else {
        headline.phase("checking for updates");

        let mut repo =
            gix::open(&manifest_dir)
                .with_context(|| "Failed to open manifest repository")?;
        // The ref edits and fetch below write reflog entries, which gix
        // refuses without a committer identity — fall back to its generic
        // in-memory one so a HOME with no ~/.gitconfig still works.
        let _ = repo.committer_or_set_generic_fallback();

        // Fetching tags won't delete old ones. To keep it simple, we'll just annihilate
        // whatever tags are already there before fetching.

        let tag_deletions: Vec<RefEdit> =
            repo.references()?
                .tags()?
                .filter_map(|t| t.ok())
                .map(|t| RefEdit {
                    change: Change::Delete {
                        expected: PreviousValue::Any,
                        log: RefLog::AndReference
                    },
                    name: t.name().to_owned(),
                    deref: false,
                })
                .collect();

        repo.edit_references(tag_deletions)?;

        let remote =
            repo.find_fetch_remote(None)? // origin
                .with_fetch_tags(Tags::All);

        remote.connect(Direction::Fetch)?
              .prepare_fetch(&mut headline, Default::default())?
              .receive(&mut headline, &gix::interrupt::IS_INTERRUPTED)?;

        headline.succeeded("up to date");
        progress_renderer.shutdown_and_wait();
        Ok(repo)
    }
}

/// The release selector naming the tip of the manifest's master branch rather
/// than a release tag. Master is newer than every release *and* moves under
/// you, so it is never a default: the user has to ask for it by name.
pub const MASTER: &str = "master";

/// The ref [`Release::master`] reads. Deliberately NOT `refs/heads/master`:
/// the initial clone writes a local `master` once and no later fetch touches
/// it (a fetch only updates `refs/remotes/origin/*`), so the local branch is
/// frozen at first-clone time while this ref is whatever
/// [`ensure_manifest_repo`] just fetched — which is the whole point of
/// selecting master.
const MASTER_REF: &str = "refs/remotes/origin/master";

/// Resolve a release selector against an already-fetched manifest: either
/// [`MASTER`] or a release tag's short name (e.g. `ET_2026_05_v0`). `Ok(None)`
/// means "no such release", which callers report and re-prompt on; an `Err`
/// means the selector named something real that could not be read. Shared by
/// `install` (positional/interactive selection) and `installation refetch
/// --release`.
pub fn resolve_release<'repo>(
    repo: &'repo Repository,
    releases: &[Release<'repo>],
    selector: &str,
) -> Res<Option<Release<'repo>>> {
    if selector == MASTER {
        return Release::master(repo).map(Some);
    }
    Ok(releases.iter().find(|release| release.name == selector).cloned())
}

/// Every release tag in the manifest, newest first. Master is deliberately
/// absent: it is not a release, and `install`'s default is this list's head.
pub fn get_releases(repo: &Repository) -> Res<Vec<Release<'_>>> {
    let mut ranked: Vec<(usize, Release<'_>)> =
        repo.references()?
            .tags()?
            .filter_map(|t| t.ok())
            .filter_map(|t| Release::from_tag(repo, t).ok())
            .filter_map(|r| r.ancestry_rank().ok().map(|rank| (rank, r)))
            .collect();

    ranked.sort_by_key(|(rank, _)| std::cmp::Reverse(*rank));
    Ok(ranked.into_iter().map(|(_, release)| release).collect())
}

/// A point in the manifest's history whose tree holds the thornlist to
/// install: a release tag, or the tip of master ([`MASTER`]).
#[derive(Clone)]
pub struct Release<'repo> {
    repo: &'repo Repository,
    /// What the user typed and what the database records: a tag's short name,
    /// or `master`. Deliberately not the commit — it has to survive as an
    /// alias default and be passable back on the command line.
    pub name: String,
    commit_id: gix::ObjectId,
    tree_id: gix::ObjectId
}

impl<'repo> Release<'repo> {
    /// A release tag, named by the tag's short name.
    fn from_tag(repo: &'repo Repository, tag: Reference<'repo>) -> Res<Self> {
        let name = tag.name().shorten().to_string();
        Self::at(repo, name, tag)
    }

    /// The tip of master, named [`MASTER`]. Always the just-fetched commit;
    /// see [`MASTER_REF`].
    fn master(repo: &'repo Repository) -> Res<Self> {
        let master = repo.find_reference(MASTER_REF)
                         .with_context(|| format!("the manifest repository has no {MASTER_REF}"))?;
        Self::at(repo, MASTER.to_owned(), master)
    }

    fn at(repo: &'repo Repository, name: String, reference: Reference<'repo>) -> Res<Self> {
        let peeled_id = reference.into_fully_peeled_id()?;
        let tree_id = peeled_id.object()?.peel_to_commit()?.tree_id()?.detach();

        Ok(Self {
            repo,
            name,
            commit_id: peeled_id.detach(),
            tree_id
        })
    }

    /// Number of commits between this release and the root of the repository.
    /// We use this as a stand-in for commit time to determine the release order
    /// of the tags, since the git history has become too mangled for the former
    /// to work. I also do not trust that the current naming convention, where
    /// the release date is encoded in the tag name, will be followed in
    /// perpetuity. This method is more robust.
    fn ancestry_rank(&self) -> Res<usize> {
        Ok(self.repo.rev_walk(Some(self.commit_id)).all()?.count())
    }

    fn is_master(&self) -> bool {
        self.name == MASTER
    }

    /// How to name this release to the user. A tag names itself; master needs
    /// its commit spelled out, because the name alone says nothing about which
    /// tip of master this was.
    pub fn describe(&self) -> String {
        if self.is_master() {
            format!("{} (commit {})", self.name, self.commit_id.to_hex_with_len(7))
        } else {
            self.name.clone()
        }
    }

    pub fn read_file(&self, path: impl AsRef<Path>) -> Res<Vec<u8>> {
        let tree = self.repo.find_tree(self.tree_id)?;
        Ok(
            tree.lookup_entry_by_path(&path)?
                .ok_or({
                    let path_name = path.as_ref().to_str().ok_or(anyhow!("Invalid path"))?;
                    anyhow!("File {} not found", path_name)
                })?
                .object()?
                .into_blob()
                .data
                .clone()
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gix::objs::tree::EntryKind;

    fn sig() -> gix::actor::SignatureRef<'static> {
        gix::actor::SignatureRef {
            name: "cactup tests".into(),
            email: "tests@example.invalid".into(),
            time: "1700000000 +0000",
        }
    }

    /// Commit a tree holding `einsteintoolkit.th` with `text` in it, and point
    /// `refname` at the result.
    fn commit(
        repo: &Repository,
        refname: &str,
        text: &str,
        parents: Vec<gix::ObjectId>,
    ) -> gix::ObjectId {
        let oid = repo.write_blob(text.as_bytes()).unwrap().detach();
        let tree = repo
            .write_object(&gix::objs::Tree {
                entries: vec![gix::objs::tree::Entry {
                    mode: EntryKind::Blob.into(),
                    filename: "einsteintoolkit.th".into(),
                    oid,
                }],
            })
            .unwrap()
            .detach();
        repo.commit_as(sig(), sig(), refname, "manifest", tree, parents).unwrap().detach()
    }

    fn tag(repo: &Repository, name: &str, target: gix::ObjectId) {
        let kind = gix::objs::Kind::Commit;
        repo.tag(name, target, kind, Some(sig()), "release", PreviousValue::MustNotExist).unwrap();
    }

    #[test]
    fn master_reads_the_fetched_origin_master_not_the_stale_local_branch() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        // Exactly the shape a clone leaves behind: a local `master` frozen at
        // clone time, and an `origin/master` that later fetches move.
        commit(&repo, "refs/heads/master", "AT CLONE TIME", vec![]);
        commit(&repo, MASTER_REF, "JUST FETCHED", vec![]);

        let master = resolve_release(&repo, &[], MASTER).unwrap().unwrap();

        assert_eq!(master.name, MASTER);
        assert!(master.is_master());
        assert_eq!(master.read_file("einsteintoolkit.th").unwrap(), b"JUST FETCHED");
        // "master" alone pins nothing, so the description names the commit.
        assert!(master.describe().starts_with("master (commit "), "{}", master.describe());
    }

    #[test]
    fn releases_are_the_tags_newest_first_and_never_master() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        let old = commit(&repo, MASTER_REF, "OLD", vec![]);
        tag(&repo, "ET_2026_05_v0", old);
        let new = commit(&repo, MASTER_REF, "NEW", vec![old]);
        tag(&repo, "ET_2026_11_v0", new);
        // The tip is past the newest tag — where master's whole point lies.
        commit(&repo, MASTER_REF, "UNRELEASED", vec![new]);

        let releases = get_releases(&repo).unwrap();

        assert_eq!(
            releases.iter().map(|r| r.name.as_str()).collect::<Vec<_>>(),
            ["ET_2026_11_v0", "ET_2026_05_v0"]
        );
        assert_eq!(releases[0].describe(), "ET_2026_11_v0");
        assert_eq!(releases[1].read_file("einsteintoolkit.th").unwrap(), b"OLD");

        let selected = resolve_release(&repo, &releases, "ET_2026_05_v0").unwrap().unwrap();
        assert_eq!(selected.read_file("einsteintoolkit.th").unwrap(), b"OLD");
        let master = resolve_release(&repo, &releases, MASTER).unwrap().unwrap();
        assert_eq!(master.read_file("einsteintoolkit.th").unwrap(), b"UNRELEASED");
    }

    #[test]
    fn an_unknown_selector_is_no_release_while_a_missing_master_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let repo = gix::init(dir.path()).unwrap();
        commit(&repo, MASTER_REF, "TIP", vec![]);
        let releases = get_releases(&repo).unwrap();

        // Unknown names are reported and re-prompted on, so they are not errors...
        assert!(resolve_release(&repo, &releases, "ET_1999_01").unwrap().is_none());
        // ...but a manifest that cannot answer for master at all is.
        let dir = tempfile::tempdir().unwrap();
        let masterless = gix::init(dir.path()).unwrap();
        assert!(resolve_release(&masterless, &[], MASTER).is_err());
    }
}
