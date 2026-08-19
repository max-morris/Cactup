use std::fs;
use std::path::Path;
use std::sync::Arc;
use gix::{Reference, Repository};
use anyhow::{anyhow, Context};
use gix::protocol::fetch::Tags;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use gix::remote::Direction;
use prodash::render::line::{JoinHandle, StreamKind};
use prodash::{Progress, Root};
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

/// How deep [`ensure_manifest_repo`]'s renderer draws. Levels:
///  1. our own stable headline ("release manifest"), never handed to gix
///  2. the item(s) we hand gix, which it renames through fetch phases
///     ("negotiate (round N)", "receiving pack", ...)
///  3. gix's per-phase bars (remote, read pack, checkout, writing)
///  4. and deeper: per-thread delta-resolution/decoding noise — not useful,
///     hidden
const MANIFEST_PROGRESS_MAX_LEVEL: prodash::progress::key::Level = 3;

pub fn ensure_manifest_repo(cactup_root: &Path, manifest_url: &str) -> Res<Repository> {
    let (progress, progress_renderer) = setup_prodash_with(
        Some(progress_level_filter(MANIFEST_PROGRESS_MAX_LEVEL)),
        true,
    );

    let manifest_dir = cactup_root.join("manifest");
    let mut headline = progress.add_child("release manifest");

    if manifest_dir.exists() && !manifest_dir.is_dir() {
        return Err(anyhow!("Manifest directory exists but is not a directory"));
    }

    let need_clone = !manifest_dir.exists()
        || manifest_dir.read_dir()
                       .map(|mut d| d.next().is_none()) // Checking for empty dir
                       .unwrap_or(true);

    if need_clone {
        headline.info("Fetching the manifest for the first time.".to_string());

        fs::create_dir_all(&manifest_dir)
           .with_context(|| "Failed to create manifest directory")?;

        let mut prepare =
            gix::prepare_clone(manifest_url, &manifest_dir)?
                .configure_remote(|r| {
                    Ok(r.with_fetch_tags(Tags::All))
                });

        // gix renames whatever item it's given as the fetch moves through
        // phases — it cannot carry our own headline, so the headline lives
        // one level above the item handed to it here.
        let gix_item = headline.add_child("connecting");
        let (repo, _) = prepare.fetch_only(
            gix_item,
            &gix::interrupt::IS_INTERRUPTED
        ).with_context(|| "Failed to fetch manifest")?;

        progress_renderer.shutdown_and_wait();
        Ok(repo)
    } else {
        headline.info("Checking for updates.".to_string());

        let fetch_progress_1 = headline.add_child("Checking for updates");
        let fetch_progress_2 = headline.add_child("Fetching updates");

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
              .prepare_fetch(fetch_progress_1, Default::default())?
              .receive(fetch_progress_2, &gix::interrupt::IS_INTERRUPTED)?;

        headline.done("Manifest is up to date.".to_string());
        progress_renderer.shutdown_and_wait();
        Ok(repo)
    }
}

/// Look a release tag up by its short name (e.g. `ET_2026_05`). Shared by
/// `install` (interactive selection) and `installation refetch --release`.
pub fn find_tag<'t, 'repo>(tags: &'t [Tag<'repo>], short_name: &str) -> Option<&'t Tag<'repo>> {
    tags.iter().find(|tag| tag.short_name == short_name)
}

pub fn get_tags(repo: &Repository) -> Res<Vec<Tag<'_>>> {
    let mut tags: Vec<Tag<'_>> =
        repo.references()?
            .tags()?
            .filter_map(|t| t.ok())
            .filter_map(|t| Tag::new(repo, t).ok())
            .collect();

    tags.sort_by_key(|t| std::cmp::Reverse(t.ancestry_rank));
    Ok(tags)
}

pub struct Tag<'repo> {
    repo: &'repo Repository,
    pub short_name: String,
    ancestry_rank: usize,
    tree_id: gix::ObjectId
}

impl<'repo> Tag<'repo> {
    pub fn new(repo: &'repo Repository, tag: Reference<'repo>) -> Res<Self> {
        let short_name = tag.name().shorten().to_string();
        let peeled_id = tag.into_fully_peeled_id()?;
        let tree_id = peeled_id.object()?.peel_to_commit()?.tree_id()?.detach();
        let commit_id = peeled_id.detach();

        // Number of commits between the tag and the root of the repository.
        // We use this as a stand-in for commit time to determine the release order of the tags,
        // since the git history has become too mangled for the former to work.
        // I also do not trust that the current naming convention, where the release date is
        // encoded in the tag name, will be followed in perpetuity. This method is more robust.
        let ancestry_rank = repo.rev_walk(Some(commit_id)).all()?.count();

        Ok(Self {
            repo,
            short_name,
            ancestry_rank,
            tree_id
        })
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