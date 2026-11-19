use std::fs;
use std::path::Path;
use std::sync::Arc;
use gix::{Reference, Repository};
use anyhow::{anyhow, Context};
use gix::protocol::fetch::Tags;
use gix::refs::transaction::{Change, PreviousValue, RefEdit, RefLog};
use gix::remote::Direction;
use prodash::render::line::StreamKind;
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
/// downloads). Every renderer starts drawing after a 500ms delay regardless
/// — a run that finishes (or finds nothing to do) inside that window never
/// flashes a bar at all — and draws the whole tree as it is then.
pub(crate) fn setup_prodash_with(
    level_filter: Option<std::ops::RangeInclusive<prodash::progress::key::Level>>,
    throughput: bool,
) -> (ProgressHandle, JoinHandle) {
    let progress = new_root();
    let options = prodash::render::line::Options {
        frames_per_second: 6.0,
        hide_cursor: true, // signal-hook feature restores cursor on exit
        level_filter,
        throughput,
        // The delay is ours (see `JoinHandle`): prodash's draws a frame only
        // when the tree changed, and counts changes during its delay as
        // seen, so a line that settled in the first 500ms was never drawn.
        initial_delay: None,
        ..Default::default()
    }
    .auto_configure(StreamKind::Stderr);
    let renderer = JoinHandle::start(&progress, options, std::time::Duration::from_millis(500), Arc::new(stderr));
    (progress, renderer)
}

/// How many scrollback messages (`done`, `info`, `fail`) the tree keeps
/// between two frames: prodash's default of 20 loses the start of a burst (a
/// refetch pinning 80 repos), and a frame drains only what is still there.
const MESSAGES: usize = 1000;

/// A progress tree that keeps [`MESSAGES`] messages.
fn new_root() -> ProgressHandle {
    prodash::tree::root::Options { initial_capacity: 100, message_buffer_capacity: MESSAGES }.into()
}

/// The renderer of a phase's progress, started only once the phase has run
/// for `delay`. prodash's own initial delay cannot stand in for this: its
/// renderer draws a frame only when the tree has changed since the last
/// tick, and the ticks during its delay count, so whatever settled before
/// the delay was over (a request that takes a minute, the line of a wait)
/// stayed blank until something moved. Started late, its first frame draws
/// the whole tree. Shut down before it started, it starts nothing, but
/// prints the messages items left (the renderer prints those, terminal or
/// not).
pub(crate) struct JoinHandle {
    out: Output,
    state: Arc<std::sync::Mutex<Started>>,
    starter: Option<std::thread::JoinHandle<()>>,
    stop: Option<std::sync::mpsc::Sender<()>>,
    progress: std::sync::Weak<prodash::tree::Root>,
    options: Option<prodash::render::line::Options>,
}

/// Where a renderer draws: stderr, but for a test.
type Output = Arc<dyn Fn() -> Box<dyn std::io::Write + Send> + Send + Sync>;

fn stderr() -> Box<dyn std::io::Write + Send> {
    Box::new(std::io::stderr())
}

#[derive(Default)]
struct Started {
    renderer: Option<prodash::render::line::JoinHandle>,
    /// The phase is over: never start.
    done: bool,
}

impl JoinHandle {
    fn start(
        progress: &ProgressHandle,
        options: prodash::render::line::Options,
        delay: std::time::Duration,
        out: Output,
    ) -> JoinHandle {
        let state = Arc::new(std::sync::Mutex::new(Started::default()));
        let (stop, stopped) = std::sync::mpsc::channel::<()>();
        let weak = Arc::downgrade(progress);
        let starter = {
            let (state, weak, options, out) = (state.clone(), weak.clone(), copy_options(&options), out.clone());
            std::thread::Builder::new()
                .name("progress delay".into())
                .spawn(move || {
                    // Woken early (or orphaned) by a shutdown: nothing to draw.
                    if !matches!(stopped.recv_timeout(delay), Err(std::sync::mpsc::RecvTimeoutError::Timeout)) {
                        return;
                    }
                    let mut state = state.lock().expect("progress renderer poisoned");
                    if !state.done {
                        state.renderer = Some(prodash::render::line::render(out(), weak, options));
                    }
                })
                .expect("failed to spawn the progress delay thread")
        };
        JoinHandle { out, state, starter: Some(starter), stop: Some(stop), progress: weak, options: Some(options) }
    }

    /// Stop drawing, clear the bars, and wait for the renderer to finish.
    pub(crate) fn shutdown_and_wait(mut self) {
        self.finish();
    }

    fn finish(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(starter) = self.starter.take() {
            let _ = starter.join();
        }
        let renderer = {
            let mut state = self.state.lock().expect("progress renderer poisoned");
            state.done = true;
            state.renderer.take()
        };
        match renderer {
            Some(renderer) => renderer.shutdown_and_wait(),
            None => self.print_messages(),
        }
    }

    /// The messages a phase that ended before drawing left, printed as the
    /// renderer prints them, and no bar.
    fn print_messages(&mut self) {
        let (Some(progress), Some(mut options)) = (self.progress.upgrade(), self.options.take()) else { return };
        let mut messages = Vec::new();
        progress.copy_messages(&mut messages);
        if messages.is_empty() {
            return;
        }
        options.output_is_terminal = false;
        options.hide_cursor = false;
        prodash::render::line::render((self.out)(), Arc::downgrade(&progress), options).shutdown_and_wait();
    }
}

impl Drop for JoinHandle {
    fn drop(&mut self) {
        self.finish();
    }
}

/// prodash's options are not `Clone`.
fn copy_options(o: &prodash::render::line::Options) -> prodash::render::line::Options {
    prodash::render::line::Options {
        output_is_terminal: o.output_is_terminal,
        colored: o.colored,
        timestamp: o.timestamp,
        terminal_dimensions: o.terminal_dimensions,
        hide_cursor: o.hide_cursor,
        level_filter: o.level_filter.clone(),
        initial_delay: o.initial_delay,
        frames_per_second: o.frames_per_second,
        throughput: o.throughput,
        keep_running_if_progress_is_empty: o.keep_running_if_progress_is_empty,
    }
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

        // Retried from the top on a transient failure: gix deletes the
        // (empty) directory it cloned into when the clone does not finish.
        // Watched (Ctrl-C, a remote gone quiet): one given up on, whose gix
        // may never return, is deleted here instead.
        let repo = crate::fetch::retry::with_backoff(&mut headline, |headline| {
            fs::create_dir_all(&manifest_dir)
               .with_context(|| "Failed to create manifest directory")?;

            let (url, dir) = (manifest_url.to_owned(), manifest_dir.clone());
            let fetched = crate::fetch::git::watched(manifest_url, headline, move |headline, cancel| {
                let mut prepare =
                    gix::prepare_clone(url.as_str(), &dir)?
                        .configure_remote(|r| {
                            Ok(r.with_fetch_tags(Tags::All))
                        });

                // gix renames whatever item it's given as the fetch moves through
                // phases and hangs its own bars off it; the Line keeps all of that
                // on the one headline, so the label follows the phase and the bar
                // never splits in two.
                let (repo, _) = prepare.fetch_only(&mut *headline, cancel)
                    .with_context(|| "Failed to fetch manifest")?;
                Ok(repo)
            });
            if fetched.is_err() {
                let _ = fs::remove_dir_all(&manifest_dir);
            }
            fetched
        })?;

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

        let url = repo.find_fetch_remote(None)? // origin
            .url(Direction::Fetch)
            .map_or_else(|| manifest_url.to_owned(), |u| u.to_bstring().to_string());

        // Nothing moves until the pack is in, so a transient failure is
        // retried from the connect. Watched: Ctrl-C, a remote gone quiet.
        crate::fetch::retry::with_backoff(&mut headline, |headline| {
            let repo = repo.clone();
            crate::fetch::git::watched(&url, headline, move |headline, cancel| {
                repo.find_fetch_remote(None)? // origin
                    .with_fetch_tags(Tags::All)
                    .connect(Direction::Fetch)?
                    .prepare_fetch(&mut *headline, Default::default())?
                    .receive(&mut *headline, cancel)?;
                Ok(())
            })
        })?;

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
                         .with_context(|| format!("The manifest repository has no {MASTER_REF}"))?;
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
                    anyhow!("File '{}' not found", path_name)
                })?
                .object()?
                .into_blob()
                .data
                .clone()
        )
    }
}

#[cfg(test)]
mod renderer_tests {
    use super::*;

    /// What a renderer wrote, shared with the test.
    #[derive(Clone, Default)]
    struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn renderer(progress: &ProgressHandle, sink: &Sink) -> JoinHandle {
        renderer_on(progress, sink, true)
    }

    fn renderer_on(progress: &ProgressHandle, sink: &Sink, terminal: bool) -> JoinHandle {
        let options = prodash::render::line::Options {
            output_is_terminal: terminal,
            colored: false,
            frames_per_second: 20.0,
            terminal_dimensions: (100, 20),
            ..Default::default()
        };
        let sink = sink.clone();
        let out: Output = Arc::new(move || Box::new(sink.clone()) as Box<dyn std::io::Write + Send>);
        JoinHandle::start(progress, options, std::time::Duration::from_millis(300), out)
    }

    /// A line that settled before the renderer's delay was over (a request
    /// that takes a minute, started at once) is drawn once the delay is
    /// over, not when something next moves.
    #[test]
    fn a_line_that_settled_early_is_drawn() {
        let progress = prodash::tree::Root::new();
        let sink = Sink::default();
        let renderer = renderer(&progress, &sink);
        let layout = crate::progress::Layout::for_names(["a request"]);
        let mut headline = crate::progress::Headline::over(progress.add_child("ask"), "ask", 1, "requests", layout.clone());
        let line = crate::progress::Line::over(headline.add_child("a request"), "a request", layout);
        line.phase("waiting for the answer".to_owned());
        std::thread::sleep(std::time::Duration::from_millis(900));
        let drawn = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
        assert!(drawn.contains("waiting for the answer"), "{drawn:?}");
        drop(line);
        headline.inc();
        renderer.shutdown_and_wait();
    }

    /// A burst of a hundred messages, before the renderer starts and while
    /// it runs, all reach the scrollback, in order (not a terminal: only the
    /// messages are written).
    #[test]
    fn a_burst_of_messages_is_printed_whole_and_in_order() {
        for after_start in [false, true] {
            let progress = new_root();
            let sink = Sink::default();
            let renderer = renderer_on(&progress, &sink, false);
            if after_start {
                std::thread::sleep(std::time::Duration::from_millis(500));
            }
            let mut item = progress.add_child("refetch");
            for i in 0..100 {
                item.done(format!("message {i:03}"));
            }
            drop(item);
            if after_start {
                std::thread::sleep(std::time::Duration::from_millis(200));
            }
            renderer.shutdown_and_wait();
            let drawn = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
            let seen: Vec<usize> = (0..100).filter_map(|i| drawn.find(&format!("message {i:03}"))).collect();
            assert_eq!(seen.len(), 100, "after start {after_start}: {drawn:?}");
            assert!(seen.windows(2).all(|w| w[0] < w[1]), "after start {after_start}: out of order");
        }
    }

    /// A phase over before the delay draws nothing at all (no bar flashes,
    /// not even the cursor escapes); one that left a message prints it.
    #[test]
    fn a_short_phase_draws_nothing_but_its_messages() {
        for message in [false, true] {
            let progress = prodash::tree::Root::new();
            let sink = Sink::default();
            let renderer = renderer(&progress, &sink);
            let mut item = progress.add_child("quick");
            if message {
                item.info("something worth saying".to_owned());
            }
            drop(item);
            renderer.shutdown_and_wait();
            let drawn = String::from_utf8_lossy(&sink.0.lock().unwrap()).into_owned();
            assert_eq!(drawn.contains("something worth saying"), message, "{drawn:?}");
            if !message {
                assert!(drawn.is_empty(), "{drawn:?}");
            }
        }
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
