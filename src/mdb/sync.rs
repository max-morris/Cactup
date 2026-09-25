//! The system MDB of a distribution build (§2.2): the published `mdb` branch,
//! fetched into `~/.cactup/mdb` and kept current.
//!
//! Layout under the MDB root:
//!
//! - `repo/` — a bare repository, fetch-only, holding the full history of the
//!   `mdb` branch at `refs/mdb/head`. A cache: when it cannot be opened it is
//!   thrown away and fetched again.
//! - `<sha>/` — the tree of one commit, exported as plain files. Never
//!   modified once in place, so a process that resolved it reads one
//!   consistent MDB for as long as it runs.
//! - `gen-<N>` — a relative symlink to the `<sha>` a binary of MDB generation
//!   N reads, swapped atomically (temp symlink + rename). One link per
//!   generation: a binary that just updated to N+1 finds no link and syncs
//!   regardless of the throttle, and two binaries of different generations
//!   sharing a home never fight over one link.
//! - `<sha>.retired` — stamped when a link moves off `<sha>`; the tree is
//!   deleted a day later, long after any process that resolved it is done.
//! - `.synced-<N>` — the throttle stamp (TOML: the tip's generation, the tip,
//!   the installed commit). Rewritten after every attempt, failed ones
//!   included, so an offline host pays for the attempt once per interval.
//! - `.lock` — a `LinkLock` serializing syncs across processes and hosts.
//!
//! A binary of generation N installs the newest commit on the branch whose
//! `GENERATION` file says N. When the branch has moved to a newer generation
//! the stamp records that, and [`newer_generation_notice`] tells the user.
//!
//! Temp names (`*.tmp-*`) all live in the MDB root itself, so every rename
//! into place stays on one filesystem; leftovers of an aborted process are
//! pruned after an hour.

use crate::database::Db;
use crate::lock::{self, LinkLock};
use crate::progress::{Layout, Line};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use gix::bstr::ByteSlice;
use gix::objs::tree::EntryKind;
use gix::protocol::fetch::Tags;
use gix::remote::Direction;
use gix::ObjectId;
use prodash::{Count, NestedProgress, Progress};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// How long a successful (or failed) sync is trusted before the next one.
const SYNC_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// How long to wait for another process that is syncing when there is no
/// copy to fall back on.
const LOCK_WAIT: Duration = Duration::from_secs(120);
/// The TCP preflight's budget. gix's http transport hard-codes a 20 s
/// connect timeout and only checks the interrupt flag between phases.
const PREFLIGHT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long an interrupted fetch gets to unwind (and so remove gix's lock
/// and temp files) before it is abandoned.
const FETCH_WIND_DOWN: Duration = Duration::from_secs(2);
/// How long a retired tree outlives the link that pointed at it.
const RETIRED_GRACE: Duration = Duration::from_secs(24 * 3600);
/// How old a `*.tmp-*` leftover must be before it is taken for abandoned.
const TEMP_GRACE: Duration = Duration::from_secs(3600);

/// The published branch, and the local ref it is fetched into. `+`: CI may
/// force-push the branch.
const REFSPEC: &str = "+refs/heads/mdb:refs/mdb/head";
const MDB_REF: &str = "refs/mdb/head";
/// The progress line's name.
const HEADLINE: &str = "machine database";

/// Whether a sync may be skipped because the last one is recent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Skip the sync while the last attempt is younger than the interval
    /// (and a copy exists). What every command does.
    Throttled,
    /// Always check the remote (`cactup update`).
    Force,
}

/// The throttle stamp `.synced-<N>`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct SyncStamp {
    /// The generation of the branch tip at the last successful fetch.
    tip_generation: u32,
    /// The branch tip at the last successful fetch.
    tip: String,
    /// The commit `gen-<N>` points at.
    commit: String,
}

/// The system MDB directory a distribution build reads: the exported tree
/// `gen-<MDB_GENERATION>` points at, under `root` (`~/.cactup/mdb`), synced
/// first unless the throttle says the copy is fresh. The `mdb-url` knob is
/// read only when a sync actually happens. Resolved once per process — a
/// later `Force` call still syncs.
pub fn system_root(root: &Path, db: &Db, mode: Mode) -> Res<PathBuf> {
    static RESOLVED: std::sync::Mutex<Option<PathBuf>> = std::sync::Mutex::new(None);
    let mut resolved = RESOLVED.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if mode == Mode::Throttled
        && let Some(dir) = resolved.as_ref()
    {
        return Ok(dir.clone());
    }
    let generation = crate::build_info::MDB_GENERATION;
    let dir = match fresh_link(root, generation, mode) {
        Some(dir) => dir,
        None => {
            let url = db
                .read()?
                .knob_or_default("mdb-url")
                .unwrap_or_else(|| crate::update::DEFAULT_MDB_URL.to_owned());
            sync_at(root, &url, generation, mode)?
        }
    };
    *resolved = Some(dir.clone());
    Ok(dir)
}

/// The warning to print when the published MDB has moved past this binary's
/// generation, from the throttle stamp alone (no network).
pub fn newer_generation_notice(root: &Path) -> Option<String> {
    generation_notice(root, crate::build_info::MDB_GENERATION)
}

fn generation_notice(root: &Path, generation: u32) -> Option<String> {
    let stamp = read_stamp(root, generation)?;
    (stamp.tip_generation > generation).then(|| {
        format!(
            "the machine database has moved to generation {}; this cactup (generation {generation}) \
             keeps using the last generation-{generation} revision. Run `cactup update` (if that \
             reports up to date, a release is still propagating; retry later)",
            stamp.tip_generation
        )
    })
}

fn link_path(root: &Path, generation: u32) -> PathBuf {
    root.join(format!("gen-{generation}"))
}

fn stamp_path(root: &Path, generation: u32) -> PathBuf {
    root.join(format!(".synced-{generation}"))
}

/// The exported tree `gen-<generation>` points at, if the link exists and
/// its target is in place.
pub fn resolve_link(root: &Path, generation: u32) -> Option<PathBuf> {
    let target = fs::read_link(link_path(root, generation)).ok()?;
    let dir = root.join(target);
    dir.is_dir().then_some(dir)
}

/// The linked tree, when `mode` lets the throttle skip the sync: the copy
/// exists and the last attempt is younger than the interval, measured in
/// the fileserver's clock.
fn fresh_link(root: &Path, generation: u32, mode: Mode) -> Option<PathBuf> {
    if mode == Mode::Force {
        return None;
    }
    let dir = resolve_link(root, generation)?;
    let age = lock::mtime_age(&stamp_path(root, generation))?;
    (age < SYNC_INTERVAL).then_some(dir)
}

fn read_stamp(root: &Path, generation: u32) -> Option<SyncStamp> {
    let text = fs::read_to_string(stamp_path(root, generation)).ok()?;
    toml::from_str(&text).ok()
}

/// Best-effort: a stamp that cannot be written only costs another sync
/// attempt next time.
fn write_stamp(root: &Path, generation: u32, stamp: &SyncStamp) {
    if let Ok(text) = toml::to_string(stamp) {
        let _ = lock::write_stamp(&stamp_path(root, generation), text.as_bytes());
    }
}

/// Shuts the progress renderer down on every way out of [`sync_at`], so an
/// error is never printed underneath a live bar.
struct Renderer(Option<prodash::render::line::JoinHandle>);

impl Drop for Renderer {
    fn drop(&mut self) {
        if let Some(renderer) = self.0.take() {
            renderer.shutdown_and_wait();
        }
    }
}

/// Sync the generation-`generation` MDB under `root` from `url` and return
/// the tree to read. With a copy in place, a failure to reach `url` warns and
/// keeps the copy; without one it is a hard error — falling back to the
/// built-in generic machine on a real cluster would persist a wrong overlay.
pub fn sync_at(root: &Path, url: &str, generation: u32, mode: Mode) -> Res<PathBuf> {
    if let Some(dir) = fresh_link(root, generation, mode) {
        return Ok(dir);
    }
    fs::create_dir_all(root).with_context(|| format!("Failed to create {}", root.display()))?;

    // Phase-scoped renderer, one line, gix's phases collapsed onto it (the
    // same shape as the release manifest's fetch). Tests get the tree alone,
    // so no progress lines land in their output.
    let (progress, renderer) = if cfg!(test) {
        (prodash::tree::Root::new(), None)
    } else {
        let (progress, renderer) =
            crate::manifest::setup_prodash_with(Some(crate::manifest::progress_level_filter(1)), true);
        (progress, Some(renderer))
    };
    let _renderer = Renderer(renderer);
    let layout = Layout::for_names([HEADLINE]);
    let mut line = Line::over(progress.add_child(HEADLINE), HEADLINE, layout);

    // §2.3: link()-based lock only; `acquire_wait` polls the interrupt flag.
    let lock_path = root.join(".lock");
    let lock = match LinkLock::try_acquire(&lock_path)? {
        Some(lock) => lock,
        None => {
            // Someone else is syncing right now; their result is as good as
            // ours, and the copy in place is fine meanwhile.
            if let Some(dir) = resolve_link(root, generation) {
                return Ok(dir);
            }
            line.phase("waiting for another cactup");
            let lock = LinkLock::acquire_wait(&lock_path, LOCK_WAIT)?;
            if let Some(dir) = resolve_link(root, generation) {
                return Ok(dir);
            }
            lock
        }
    };
    let _lock = lock.with_heartbeat();
    // A sync that finished while we took the lock counts.
    if let Some(dir) = fresh_link(root, generation, mode) {
        return Ok(dir);
    }

    let outcome = sync_locked(root, url, generation, &mut line);
    if !gix::interrupt::is_triggered() {
        for warning in prune(root) {
            line.warned(warning);
        }
    }
    outcome
}

/// The sync proper, under the lock.
fn sync_locked(root: &Path, url: &str, generation: u32, line: &mut Line) -> Res<PathBuf> {
    let current = resolve_link(root, generation);
    line.phase("checking for updates");
    let repo_dir = root.join("repo");
    // We hold `.lock`, which owns `repo/`: any gix lock file in there is a
    // leftover of a fetch that died holding it (kill -9, OOM, a node crash,
    // an interrupt that abandoned it), and gix would refuse every later
    // fetch over it.
    remove_stale_locks(&repo_dir);
    // Anything short of a fetched tip — the cache cannot be opened, the host
    // is unreachable, the fetch fails — leaves the copy in place, if any.
    let fetch = |line: &Line| {
        open_repo(&repo_dir).and_then(|repo| {
            preflight(&repo, url)?;
            fetch_abandonable(repo, url, line)
        })
    };
    let mut fetched = fetch(line);
    if let Err(e) = &fetched
        && !gix::interrupt::is_triggered()
        && is_lock_error(e)
    {
        // A lock the sweep could not remove: the cache holds nothing a fetch
        // cannot restore, so start it over and fetch once more.
        let _ = fs::remove_dir_all(&repo_dir);
        fetched = fetch(line);
    }
    let (repo, tip) = match fetched {
        Ok(fetched) => fetched,
        Err(e) if gix::interrupt::is_triggered() => return Err(e),
        Err(e) => {
            let Some(dir) = current else {
                bail!(
                    "could not fetch the machine database from {url}: {e:#}\n\
                     This cactup has no copy of it yet, and without one it cannot know which machine \
                     it is on. Run `cactup update` on a host with network access (the copy in {} serves \
                     every host that shares this home directory), or pass --mdb-path with the path of \
                     a machine database on disk.",
                    root.display()
                );
            };
            keep_stamp(root, generation, &dir, None);
            line.warned(format!(
                "could not reach {url} ({}); using the copy from {}",
                first_line(&format!("{e:#}")),
                copy_date(root, &dir)
            ));
            return Ok(dir);
        }
    };

    let (found, tip_generation) = find_generation(&repo, tip, generation)?;
    let Some(commit) = found else {
        let Some(dir) = current else {
            bail!(
                "{url} has never published MDB generation {generation} (its newest is generation \
                 {tip_generation}), which this cactup needs. {}",
                if tip_generation > generation {
                    "Run `cactup update` to get a cactup that reads the newer generation."
                } else {
                    "A release may still be propagating; retry later."
                }
            );
        };
        keep_stamp(root, generation, &dir, Some((tip_generation, tip)));
        line.warned(format!(
            "{url} no longer carries MDB generation {generation}; using the copy from {}",
            copy_date(root, &dir)
        ));
        return Ok(dir);
    };

    let changed =
        install_commit(&repo, commit, root, generation, line, &gix::interrupt::IS_INTERRUPTED)?;
    let stamp = SyncStamp { tip_generation, tip: tip.to_string(), commit: commit.to_string() };
    write_stamp(root, generation, &stamp);
    if changed {
        line.succeeded(format!("updated to generation {generation} ({})", short(commit)));
    }
    Ok(root.join(commit.to_string()))
}

/// Re-stamp after a failed attempt, keeping what is known (plus a freshly
/// learned tip, when the fetch itself worked). Without a stamp to keep, the
/// last tip fetched into the cache stands in for it.
fn keep_stamp(root: &Path, generation: u32, dir: &Path, tip: Option<(u32, ObjectId)>) {
    let commit = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut stamp = read_stamp(root, generation).unwrap_or_else(|| {
        let cached = gix::open(root.join("repo")).ok().and_then(|repo| {
            let tip = repo.find_reference(MDB_REF).ok()?.peel_to_id().ok()?.detach();
            Some((commit_generation(&repo, tip).ok()?, tip.to_string()))
        });
        let (tip_generation, tip) = cached.unwrap_or_else(|| (generation, commit.clone()));
        SyncStamp { tip_generation, tip, commit: commit.clone() }
    });
    stamp.commit = commit;
    if let Some((tip_generation, tip)) = tip {
        stamp.tip_generation = tip_generation;
        stamp.tip = tip.to_string();
    }
    write_stamp(root, generation, &stamp);
}

fn short(id: ObjectId) -> String {
    id.to_hex_with_len(7).to_string()
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or(s)
}

/// The committer date of the copy at `dir` (a `<sha>` tree under `root`),
/// for the stale warning; its abbreviated id when the date cannot be read.
fn copy_date(root: &Path, dir: &Path) -> String {
    let name = dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let repo = gix::open(root.join("repo")).ok();
    let date = ObjectId::from_hex(name.as_bytes())
        .ok()
        .and_then(|id| repo.as_ref()?.find_commit(id).ok())
        .and_then(|commit| commit.time().ok())
        .and_then(|time| chrono::DateTime::from_timestamp(time.seconds, 0))
        .map(|date| date.format("%Y-%m-%d").to_string());
    date.unwrap_or_else(|| name.chars().take(7).collect())
}

/// Delete every gix lock file (`*.lock`) in the fetch cache `repo`: at its
/// top (`HEAD.lock`, `config.lock`, `packed-refs.lock`), anywhere under
/// `refs/`, and in `objects/pack/`. Called only under `.lock`, which owns
/// `repo/`, so every one of them is stale. Best-effort and silent: one that
/// cannot be removed fails the fetch with a lock error, and the cache is
/// rebuilt.
fn remove_stale_locks(repo: &Path) {
    fn sweep(dir: &Path, recurse: bool) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if recurse {
                    sweep(&path, true);
                }
            } else if path.extension().is_some_and(|ext| ext == "lock") {
                let _ = fs::remove_file(&path);
            }
        }
    }
    sweep(repo, false);
    sweep(&repo.join("refs"), true);
    sweep(&repo.join("objects").join("pack"), false);
}

/// Did `e` come from a gix lock file that is in the way?
fn is_lock_error(e: &anyhow::Error) -> bool {
    e.chain()
        .any(|cause| cause.downcast_ref::<gix::lock::acquire::Error>().is_some())
}

/// Open the bare fetch cache, (re-)creating it when it is missing or broken:
/// it holds nothing a fetch cannot restore.
fn open_repo(dir: &Path) -> Res<gix::Repository> {
    let mut repo = match gix::open(dir) {
        Ok(repo) => repo,
        Err(_) => {
            if dir.exists() {
                fs::remove_dir_all(dir)
                    .with_context(|| format!("Failed to remove {}", dir.display()))?;
            }
            gix::init_bare(dir).with_context(|| format!("Failed to create {}", dir.display()))?
        }
    };
    // The fetch writes a reflog entry for the ref it moves, which gix refuses
    // without a committer identity; fall back to its in-memory generic one.
    let _ = repo.committer_or_set_generic_fallback();
    Ok(repo)
}

/// Before an http(s) fetch, check within [`PREFLIGHT_TIMEOUT`] that the host
/// accepts a TCP connection at all, so an offline host fails fast (and the
/// wait stays interruptible). Other transports, and hosts behind a proxy
/// (where a direct connection proves nothing), go straight to the fetch —
/// a proxy from the environment or from git config.
fn preflight(repo: &gix::Repository, url: &str) -> Res<()> {
    use gix::url::Scheme;
    let Ok(parsed) = gix::url::parse(url.as_bytes().as_bstr()) else { return Ok(()) };
    if !matches!(parsed.scheme, Scheme::Http | Scheme::Https) {
        return Ok(());
    }
    let proxied = ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"]
        .iter()
        .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()));
    if proxied || git_config_proxy(repo) {
        return Ok(());
    }
    let Some(host) = parsed.host().map(str::to_owned) else { return Ok(()) };
    let port = parsed.port_or_default().unwrap_or(443);

    let (tx, rx) = std::sync::mpsc::channel();
    let target = host.clone();
    std::thread::spawn(move || {
        let _ = tx.send(connect(&target, port));
    });
    let started = std::time::Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(50)) {
            Ok(result) => return result,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                bail!("the connection check for {host}:{port} failed")
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
        }
        if gix::interrupt::is_triggered() {
            bail!("interrupted");
        }
        if started.elapsed() >= PREFLIGHT_TIMEOUT {
            bail!("no connection to {host}:{port} within {}s", PREFLIGHT_TIMEOUT.as_secs());
        }
    }
}

/// Does git config name an http proxy (`http.proxy`, `http.<url>.proxy` or
/// `https.proxy`)? The cache's config snapshot includes the user's
/// `~/.gitconfig` and the system config, as `gix::open` reads them all.
fn git_config_proxy(repo: &gix::Repository) -> bool {
    let snapshot = repo.config_snapshot();
    let config = snapshot.plumbing();
    ["http", "https"].into_iter().any(|name| {
        config.sections_by_name(name).is_some_and(|mut sections| {
            sections.any(|section| section.value("proxy").is_some_and(|v| !v.trim().is_empty()))
        })
    })
}

/// Resolve `host` and try its addresses until one accepts, all within
/// [`PREFLIGHT_TIMEOUT`].
fn connect(host: &str, port: u16) -> Res<()> {
    use std::net::{TcpStream, ToSocketAddrs};
    let deadline = std::time::Instant::now() + PREFLIGHT_TIMEOUT;
    let addrs = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("cannot resolve {host}"))?;
    let mut last = None;
    for addr in addrs {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&addr, left) {
            Ok(_) => return Ok(()),
            Err(e) => last = Some(e),
        }
    }
    Err(match last {
        Some(e) => anyhow!("cannot connect to {host}:{port}: {e}"),
        None => anyhow!("no connection to {host}:{port} within {}s", PREFLIGHT_TIMEOUT.as_secs()),
    })
}

/// [`fetch_mdb_branch`] on a helper thread, polled from this one every
/// 100 ms; returns the repository along with the tip. gix's http transport
/// connects with a hard-coded 20 s timeout and checks the interrupt flag
/// only between phases, and behind a proxy no preflight bounds that wait,
/// so the calling thread is what keeps an interrupt prompt. On an interrupt
/// it gives the fetch up to [`FETCH_WIND_DOWN`] to see the flag and unwind —
/// its drops remove gix's lock and temp files, which the exit the interrupt
/// leads to would not — then fails with "interrupted" whether or not the
/// fetch finished; one still stuck in a connect is abandoned, never joined,
/// and dies with the process. The thread reports through its own handle on
/// `line`.
fn fetch_abandonable(repo: gix::Repository, url: &str, line: &Line) -> Res<(gix::Repository, ObjectId)> {
    use std::sync::mpsc::RecvTimeoutError;
    let (tx, rx) = std::sync::mpsc::channel();
    let url = url.to_owned();
    let mut handle = line.another_handle();
    std::thread::spawn(move || {
        let tip = fetch_mdb_branch(&repo, &url, &mut handle);
        drop(handle);
        let _ = tx.send(tip.map(|tip| (repo, tip)));
    });
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(result) => return result,
            Err(RecvTimeoutError::Timeout) => {
                if gix::interrupt::is_triggered() {
                    let deadline = std::time::Instant::now() + FETCH_WIND_DOWN;
                    while std::time::Instant::now() < deadline
                        && matches!(
                            rx.recv_timeout(Duration::from_millis(100)),
                            Err(RecvTimeoutError::Timeout)
                        )
                    {}
                    bail!("interrupted");
                }
            }
            Err(RecvTimeoutError::Disconnected) => {
                bail!("the machine database fetch stopped unexpectedly")
            }
        }
    }
}

/// Fetch the published `mdb` branch (full history, no tags) into
/// `refs/mdb/head` and return its tip. The remote is anonymous, so a changed
/// `mdb-url` knob needs no config rewrite; the fetch is incremental after
/// the first.
fn fetch_mdb_branch(repo: &gix::Repository, url: &str, line: &mut Line) -> Res<ObjectId> {
    let remote = repo
        .remote_at(url)
        .with_context(|| format!("invalid mdb-url {url}"))?
        .with_refspecs([REFSPEC], Direction::Fetch)
        .expect("the refspec is valid")
        .with_fetch_tags(Tags::None);
    remote
        .connect(Direction::Fetch)
        .with_context(|| format!("failed to connect to {url}"))?
        .prepare_fetch(&mut *line, Default::default())
        .with_context(|| format!("failed to list the branches of {url}"))?
        .receive(&mut *line, &gix::interrupt::IS_INTERRUPTED)
        .with_context(|| format!("failed to fetch the mdb branch of {url}"))?;
    if gix::interrupt::is_triggered() {
        bail!("interrupted");
    }
    let tip = repo
        .find_reference(MDB_REF)
        .with_context(|| format!("{url} has no mdb branch"))?
        .peel_to_id()
        .with_context(|| format!("failed to resolve the mdb branch of {url}"))?
        .detach();
    Ok(tip)
}

/// The MDB generation `commit` carries in its `GENERATION` file; 0 (no
/// generation) when it has none, or none that parses.
fn commit_generation(repo: &gix::Repository, commit: ObjectId) -> Res<u32> {
    let tree = repo
        .find_commit(commit)
        .with_context(|| format!("MDB commit {commit} is missing"))?
        .tree()
        .with_context(|| format!("MDB commit {commit} has no tree"))?;
    let Some(entry) = tree.lookup_entry_by_path("GENERATION")? else { return Ok(0) };
    let blob = entry.object()?;
    Ok(std::str::from_utf8(&blob.data)
        .ok()
        .and_then(super::parse_generation)
        .unwrap_or(0))
}

/// The newest commit on `tip`'s first-parent line that is of `generation`
/// (`None`: there is none), and the generation of `tip` itself. The walk
/// stops at the first older commit, since generations only ever go up.
pub fn find_generation(
    repo: &gix::Repository,
    tip: ObjectId,
    generation: u32,
) -> Res<(Option<ObjectId>, u32)> {
    let tip_generation = commit_generation(repo, tip)?;
    if tip_generation <= generation {
        return Ok(((tip_generation == generation).then_some(tip), tip_generation));
    }
    let walk = repo
        .rev_walk([tip])
        .first_parent_only()
        .all()
        .with_context(|| "failed to walk the MDB history")?;
    for info in walk {
        if gix::interrupt::is_triggered() {
            bail!("interrupted");
        }
        let id = info.with_context(|| "failed to walk the MDB history")?.id;
        let found = commit_generation(repo, id)?;
        if found == generation {
            return Ok((Some(id), tip_generation));
        }
        if found < generation {
            break;
        }
    }
    Ok((None, tip_generation))
}

/// Make `commit` the generation-`generation` MDB under `root`: export its
/// tree to `<sha>/` unless already there, then point `gen-<generation>` at
/// it. Returns whether the link moved.
pub fn install_commit(
    repo: &gix::Repository,
    commit: ObjectId,
    root: &Path,
    generation: u32,
    progress: &mut impl NestedProgress,
    should_interrupt: &AtomicBool,
) -> Res<bool> {
    let found = commit_generation(repo, commit)?;
    if found != generation {
        bail!(
            "publishing error: MDB commit {} says it is generation {found}, not {generation}",
            short(commit)
        );
    }
    let name = commit.to_string();
    if !root.join(&name).is_dir() {
        export_tree(repo, commit, root, &name, progress, should_interrupt)?;
    }
    swap_link(root, generation, &name)
}

/// Write `commit`'s tree to `root/<name>` as plain files: into a temp
/// directory first, renamed into place only once complete, so a reader (or
/// an interrupted export) never sees half a tree.
pub fn export_tree(
    repo: &gix::Repository,
    commit: ObjectId,
    root: &Path,
    name: &str,
    progress: &mut impl NestedProgress,
    should_interrupt: &AtomicBool,
) -> Res<PathBuf> {
    use std::os::unix::fs::PermissionsExt;
    let tree = repo.find_commit(commit)?.tree()?;
    let entries = tree
        .traverse()
        .breadthfirst
        .files()
        .with_context(|| format!("failed to read the tree of MDB commit {commit}"))?;
    let temp = tempfile::Builder::new()
        .prefix(&format!("{name}.tmp-"))
        .tempdir_in(root)
        .with_context(|| format!("Failed to create a temp directory in {}", root.display()))?;

    // Named as gix names its checkout, so the line reads "checking out".
    let mut files = progress.add_child("checkout");
    files.init(Some(entries.len()), Some(prodash::unit::label("files")));
    for entry in &entries {
        if should_interrupt.load(Ordering::Relaxed) {
            bail!("interrupted");
        }
        let rel = entry.filepath.to_str().map_err(|_| anyhow!("non-UTF-8 path in the MDB"))?;
        let rel = Path::new(rel);
        if !rel.components().all(|c| matches!(c, Component::Normal(_))) {
            bail!("unsafe path {} in the MDB", rel.display());
        }
        let path = temp.path().join(rel);
        let write = || -> Res<()> {
            let kind = entry.mode.kind();
            if kind == EntryKind::Tree {
                return Ok(fs::create_dir_all(&path)?);
            }
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            match kind {
                EntryKind::Blob | EntryKind::BlobExecutable => {
                    fs::write(&path, &repo.find_blob(entry.oid)?.data)?;
                    if kind == EntryKind::BlobExecutable {
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o755))?;
                    }
                }
                EntryKind::Link => {
                    let blob = repo.find_blob(entry.oid)?;
                    use std::os::unix::ffi::OsStrExt;
                    std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&blob.data), &path)?;
                }
                EntryKind::Commit => bail!("the MDB may not contain submodules"),
                EntryKind::Tree => unreachable!(),
            }
            Ok(())
        };
        write().with_context(|| format!("Failed to write {}", path.display()))?;
        files.inc();
    }
    // tempdir() creates its directory 0700.
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o755))?;
    let staged = temp.keep();
    let dest = root.join(name);
    if let Err(e) = fs::rename(&staged, &dest) {
        let _ = fs::remove_dir_all(&staged);
        return Err(e)
            .with_context(|| format!("Failed to move the MDB into place at {}", dest.display()));
    }
    Ok(dest)
}

/// Point `gen-<generation>` at the exported tree `name`, atomically (a temp
/// symlink renamed over the link), and mark the tree it pointed at before as
/// retired. Returns whether the link moved.
pub fn swap_link(root: &Path, generation: u32, name: &str) -> Res<bool> {
    let link = link_path(root, generation);
    let old = fs::read_link(&link).ok();
    if old.as_deref() == Some(Path::new(name)) {
        return Ok(false);
    }
    let temp = tempfile::Builder::new()
        .prefix(&format!("gen-{generation}.tmp-"))
        .make_in(root, |path| std::os::unix::fs::symlink(name, path))
        .with_context(|| format!("Failed to create a symlink in {}", root.display()))?;
    temp.persist(&link)
        .map_err(|e| e.error)
        .with_context(|| format!("Failed to move the link {} into place", link.display()))?;
    // A tree that is current again is no longer retired.
    let _ = fs::remove_file(root.join(format!("{name}.retired")));
    if let Some(old_name) = old.as_deref().and_then(Path::file_name) {
        let mut retired = old_name.to_owned();
        retired.push(".retired");
        lock::write_stamp(&root.join(retired), b"")?;
    }
    Ok(true)
}

/// Delete retired trees no `gen-*` link points at once their `.retired`
/// stamp is a day old, and `*.tmp-*` leftovers once an hour old. Best-effort:
/// returns what could not be removed, for warnings.
pub fn prune(root: &Path) -> Vec<String> {
    let mut warnings = Vec::new();
    let Ok(now) = lock::fileserver_now(root) else { return warnings };
    let Ok(entries) = fs::read_dir(root) else { return warnings };
    let entries: Vec<(OsString, PathBuf)> =
        entries.flatten().map(|e| (e.file_name(), e.path())).collect();
    let referenced: Vec<OsString> = entries
        .iter()
        .filter(|(name, _)| name.to_string_lossy().starts_with("gen-"))
        .filter_map(|(_, path)| fs::read_link(path).ok())
        .filter_map(|target| target.file_name().map(ToOwned::to_owned))
        .collect();
    let age = |path: &Path| {
        let mtime = fs::symlink_metadata(path).and_then(|m| m.modified()).ok()?;
        Some(now.duration_since(mtime).unwrap_or(Duration::ZERO))
    };
    let older = |path: &Path, grace: Duration| age(path).is_some_and(|age| age > grace);
    let mut remove = |path: &Path| {
        let result = match fs::symlink_metadata(path) {
            Ok(meta) if meta.is_dir() => fs::remove_dir_all(path),
            Ok(_) => fs::remove_file(path),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            warnings.push(format!("could not remove {}: {e}", path.display()));
        }
    };
    for (name, path) in &entries {
        let name = name.to_string_lossy();
        if name.contains(".tmp-") {
            if older(path, TEMP_GRACE) {
                remove(path);
            }
        } else if let Some(sha) = name.strip_suffix(".retired") {
            if referenced.iter().any(|r| r.to_string_lossy() == sha) {
                remove(path);
            } else if older(path, RETIRED_GRACE) {
                remove(&root.join(sha));
                remove(path);
            }
        }
    }
    warnings
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    /// A bare repository standing in for the published one, with an `mdb`
    /// branch built commit by commit.
    struct Remote {
        _dir: tempfile::TempDir,
        path: PathBuf,
        repo: gix::Repository,
        commits: Vec<ObjectId>,
    }

    impl Remote {
        fn new() -> Remote {
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("published.git");
            gix::init_bare(&path).unwrap();
            // gix refuses to commit without an identity; the ambient user
            // config must not leak in.
            let config = fs::read_to_string(path.join("config")).unwrap();
            fs::write(
                path.join("config"),
                format!("{config}[user]\n\tname = cactup-test\n\temail = test@invalid\n"),
            )
            .unwrap();
            let repo = gix::open(&path).unwrap();
            Remote { _dir: dir, path, repo, commits: Vec::new() }
        }

        fn url(&self) -> String {
            self.path.display().to_string()
        }

        /// Commit an MDB of `generation` (`None`: no GENERATION file) on top
        /// of the branch; `marker` makes each tree distinct.
        fn commit(&mut self, generation: Option<u32>, marker: &str) -> ObjectId {
            let repo = &self.repo;
            let empty = ObjectId::empty_tree(repo.object_hash());
            let mut editor = repo.edit_tree(empty).unwrap();
            let blob = |data: &str| repo.write_blob(data.as_bytes()).unwrap().detach();
            if let Some(generation) = generation {
                editor
                    .upsert("GENERATION", EntryKind::Blob, blob(&format!("{generation}\n")))
                    .unwrap();
            }
            editor.upsert("generic/meta.toml", EntryKind::Blob, blob(marker)).unwrap();
            editor
                .upsert("generic/bin/probe", EntryKind::BlobExecutable, blob("#!/bin/sh\n"))
                .unwrap();
            editor.upsert("generic/alias", EntryKind::Link, blob("meta.toml")).unwrap();
            let tree = editor.write().unwrap().detach();
            let parents: Vec<ObjectId> = self.commits.last().copied().into_iter().collect();
            let id = repo.commit("refs/heads/mdb", marker, tree, parents).unwrap().detach();
            self.commits.push(id);
            id
        }
    }

    fn history(generations: &[u32]) -> Remote {
        let mut remote = Remote::new();
        for (i, generation) in generations.iter().enumerate() {
            remote.commit(Some(*generation), &format!("commit {i}"));
        }
        remote
    }

    fn names(root: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(root)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    fn install(remote: &Remote, commit: ObjectId, root: &Path, generation: u32) -> Res<bool> {
        let never = AtomicBool::new(false);
        install_commit(&remote.repo, commit, root, generation, &mut gix::progress::Discard, &never)
    }

    #[test]
    fn find_generation_walks_back_to_the_newest_commit_of_a_generation() {
        let remote = history(&[1, 1, 2, 2]);
        let tip = *remote.commits.last().unwrap();
        assert_eq!(find_generation(&remote.repo, tip, 1).unwrap(), (Some(remote.commits[1]), 2));
        assert_eq!(find_generation(&remote.repo, tip, 2).unwrap(), (Some(tip), 2));
        assert_eq!(find_generation(&remote.repo, tip, 3).unwrap(), (None, 2));

        // History from before generations existed ends the walk.
        let mut remote = Remote::new();
        remote.commit(None, "prehistoric");
        let tip = remote.commit(Some(2), "later");
        assert_eq!(find_generation(&remote.repo, tip, 1).unwrap(), (None, 2));
    }

    #[test]
    fn install_exports_the_tree_and_links_it() {
        let remote = history(&[1, 1]);
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let first = remote.commits[0];
        assert!(install(&remote, first, root, 1).unwrap());
        let dir = resolve_link(root, 1).unwrap();
        assert_eq!(dir, root.join(first.to_string()));
        assert_eq!(fs::read_link(root.join("gen-1")).unwrap(), Path::new(&first.to_string()));
        assert_eq!(fs::read_to_string(dir.join("GENERATION")).unwrap(), "1\n");
        assert_eq!(fs::read_to_string(dir.join("generic/meta.toml")).unwrap(), "commit 0");
        assert_eq!(fs::read_to_string(dir.join("generic/alias")).unwrap(), "commit 0");
        assert_eq!(fs::read_link(dir.join("generic/alias")).unwrap(), Path::new("meta.toml"));
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&dir.join("generic/bin/probe")) & 0o111, 0o111);
        assert_eq!(mode(&dir.join("generic/meta.toml")) & 0o111, 0);
        assert_eq!(mode(&dir), 0o755);

        // Installing it again changes nothing.
        assert!(!install(&remote, first, root, 1).unwrap());

        // Moving on retires the old tree, which stays readable.
        let second = remote.commits[1];
        assert!(install(&remote, second, root, 1).unwrap());
        assert_eq!(resolve_link(root, 1).unwrap(), root.join(second.to_string()));
        assert!(root.join(format!("{first}.retired")).is_file());
        assert!(root.join(first.to_string()).join("generic/meta.toml").is_file());
        assert!(!names(root).iter().any(|n| n.contains(".tmp-")), "{:?}", names(root));

        // Swapping back un-retires it.
        assert!(swap_link(root, 1, &first.to_string()).unwrap());
        assert!(!root.join(format!("{first}.retired")).exists());
        assert!(root.join(format!("{second}.retired")).is_file());
    }

    #[test]
    fn a_generation_mismatch_leaves_nothing_behind() {
        let remote = history(&[1, 2]);
        let root = tempfile::tempdir().unwrap();
        let err = install(&remote, remote.commits[1], root.path(), 1).unwrap_err().to_string();
        assert!(err.contains("publishing error"), "{err}");
        assert!(names(root.path()).is_empty(), "{:?}", names(root.path()));
    }

    #[test]
    fn an_interrupted_export_leaves_nothing_behind() {
        let remote = history(&[1]);
        let root = tempfile::tempdir().unwrap();
        let interrupted = AtomicBool::new(true);
        let err = install_commit(
            &remote.repo,
            remote.commits[0],
            root.path(),
            1,
            &mut gix::progress::Discard,
            &interrupted,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("interrupted"), "{err}");
        assert!(names(root.path()).is_empty(), "{:?}", names(root.path()));
    }

    #[test]
    fn prune_removes_old_retired_trees_and_temp_leftovers() {
        let remote = history(&[1, 1]);
        let root = tempfile::tempdir().unwrap();
        let root = root.path();
        let (first, second) = (remote.commits[0].to_string(), remote.commits[1].to_string());
        install(&remote, remote.commits[0], root, 1).unwrap();
        install(&remote, remote.commits[1], root, 1).unwrap();
        fs::create_dir(root.join("abandoned.tmp-x")).unwrap();
        fs::write(root.join("abandoned.tmp-x/file"), "").unwrap();

        // Fresh stamps: nothing goes yet.
        assert!(prune(root).is_empty());
        assert!(root.join(&first).is_dir() && root.join("abandoned.tmp-x").is_dir());

        // Age the stamps past their grace periods (test-only; production
        // never sets mtimes).
        let old = SystemTime::now() - Duration::from_secs(2 * 24 * 3600);
        let age = |p: &Path| fs::File::options().write(true).open(p).unwrap().set_modified(old).unwrap();
        age(&root.join(format!("{first}.retired")));
        fs::File::open(root.join("abandoned.tmp-x")).unwrap().set_modified(old).unwrap();
        assert!(prune(root).is_empty());
        assert!(!root.join(&first).exists());
        assert!(!root.join(format!("{first}.retired")).exists());
        assert!(!root.join("abandoned.tmp-x").exists());
        assert!(root.join(&second).is_dir());

        // A retired tree a link points at again is kept, and its stamp goes.
        fs::write(root.join(format!("{second}.retired")), "").unwrap();
        age(&root.join(format!("{second}.retired")));
        prune(root);
        assert!(root.join(&second).is_dir());
        assert!(!root.join(format!("{second}.retired")).exists());
    }

    #[test]
    fn the_notice_reads_the_stamp() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(generation_notice(root.path(), 1), None);
        assert_eq!(newer_generation_notice(root.path()), None);
        let stamp = |tip_generation| SyncStamp { tip_generation, tip: "t".into(), commit: "c".into() };
        write_stamp(root.path(), 1, &stamp(1));
        assert_eq!(generation_notice(root.path(), 1), None);
        write_stamp(root.path(), 1, &stamp(3));
        let notice = generation_notice(root.path(), 1).unwrap();
        assert!(notice.contains("moved to generation 3"), "{notice}");
        assert!(notice.contains("this cactup (generation 1)"), "{notice}");
        assert!(notice.contains("cactup update"), "{notice}");
    }

    #[test]
    fn the_lock_sweep_removes_lock_files_only() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path().join("repo");
        gix::init_bare(&repo).unwrap();
        fs::create_dir_all(repo.join("refs/mdb")).unwrap();
        fs::create_dir_all(repo.join("objects/pack")).unwrap();
        let stale = [
            "HEAD.lock",
            "config.lock",
            "packed-refs.lock",
            "refs/mdb/head.lock",
            "objects/pack/pack-1.keep.lock",
        ];
        let kept = ["HEAD", "config", "refs/mdb/head", "objects/pack/pack-1.pack", "objects/ab.lock"];
        for name in stale.iter().chain(&kept) {
            fs::write(repo.join(name), "").unwrap();
        }
        remove_stale_locks(&repo);
        for name in stale {
            assert!(!repo.join(name).exists(), "{name} survived");
        }
        for name in kept {
            assert!(repo.join(name).exists(), "{name} was removed");
        }
    }

    #[test]
    fn a_git_config_proxy_skips_the_preflight() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("repo");
        let repo = open_repo(&path).unwrap();
        let env_proxy =
            ["https_proxy", "HTTPS_PROXY", "http_proxy", "HTTP_PROXY", "all_proxy", "ALL_PROXY"]
                .iter()
                .any(|var| std::env::var_os(var).is_some_and(|v| !v.is_empty()));
        if env_proxy || git_config_proxy(&repo) {
            eprintln!("note: a proxy is configured here already; skipping the preflight proxy test");
            return;
        }
        // Nothing listens on port 1: the direct probe fails at once.
        let url = "https://127.0.0.1:1/cactup.git";
        assert!(preflight(&repo, url).is_err());
        let config = fs::read_to_string(path.join("config")).unwrap();
        fs::write(path.join("config"), format!("{config}[http]\n\tproxy = http://proxy.invalid:3128\n"))
            .unwrap();
        let repo = gix::open(&path).unwrap();
        assert!(git_config_proxy(&repo));
        preflight(&repo, url).unwrap();
    }

    /// Local-path fetches run `git-upload-pack`; without it on PATH the
    /// transport tests have nothing to talk to.
    fn have_upload_pack() -> bool {
        let found = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|dir| dir.join("git-upload-pack").is_file())
        });
        if !found {
            eprintln!("note: git-upload-pack is not on PATH; skipping an MDB transport test");
        }
        found
    }

    const BOGUS: &str = "/nonexistent/cactup-test/published.git";

    #[test]
    fn an_older_binary_syncs_the_last_commit_of_its_generation() {
        if !have_upload_pack() {
            return;
        }
        let mut remote = history(&[1, 1]);
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("mdb");

        let dir = sync_at(&root, &remote.url(), 1, Mode::Throttled).unwrap();
        assert_eq!(dir, root.join(remote.commits[1].to_string()));
        assert_eq!(generation_notice(&root, 1), None);

        // The branch moves on to generation 2; a generation-1 binary keeps the
        // last generation-1 commit and learns that a newer generation exists.
        remote.commit(Some(2), "commit 2");
        let tip = remote.commit(Some(2), "commit 3");
        let dir = sync_at(&root, &remote.url(), 1, Mode::Force).unwrap();
        assert_eq!(dir, root.join(remote.commits[1].to_string()));
        let stamp = read_stamp(&root, 1).unwrap();
        assert_eq!(stamp.tip_generation, 2);
        assert_eq!(stamp.tip, tip.to_string());
        assert_eq!(stamp.commit, remote.commits[1].to_string());
        assert!(generation_notice(&root, 1).unwrap().contains("generation 2"));

        // A generation-2 binary sharing the home gets its own link.
        assert_eq!(
            sync_at(&root, &remote.url(), 2, Mode::Throttled).unwrap(),
            root.join(tip.to_string())
        );
        assert_eq!(resolve_link(&root, 1).unwrap(), root.join(remote.commits[1].to_string()));

        // A generation nobody published is a hard error without a copy.
        let err = sync_at(&root, &remote.url(), 5, Mode::Throttled).unwrap_err().to_string();
        assert!(err.contains("never published MDB generation 5"), "{err}");
    }

    #[test]
    fn a_fresh_stamp_skips_the_fetch_and_a_failed_one_keeps_the_copy() {
        if !have_upload_pack() {
            return;
        }
        let remote = history(&[1]);
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("mdb");
        let synced = sync_at(&root, &remote.url(), 1, Mode::Throttled).unwrap();

        // Within the interval the URL is never consulted.
        assert_eq!(sync_at(&root, BOGUS, 1, Mode::Throttled).unwrap(), synced);

        // Forced (or once the stamp is old), an unreachable URL keeps the copy
        // and re-stamps it, keeping what the stamp knew.
        let before = read_stamp(&root, 1).unwrap();
        assert_eq!(sync_at(&root, BOGUS, 1, Mode::Force).unwrap(), synced);
        assert_eq!(read_stamp(&root, 1).unwrap(), before);
    }

    #[test]
    fn a_stale_gix_lock_does_not_wedge_the_cache() {
        if !have_upload_pack() {
            return;
        }
        let mut remote = history(&[1]);
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("mdb");
        sync_at(&root, &remote.url(), 1, Mode::Throttled).unwrap();
        let repo = root.join("repo");

        // Survives exactly as long as the cache is not rebuilt.
        fs::write(repo.join("marker"), "").unwrap();

        // Left behind by a fetch that died holding them: swept away, and the
        // next sync moves the ref they guarded, in the same cache.
        let stale = ["refs/mdb/head.lock", "packed-refs.lock", "HEAD.lock", "objects/pack/x.lock"];
        fs::create_dir_all(repo.join("objects/pack")).unwrap();
        for name in stale {
            fs::write(repo.join(name), "").unwrap();
        }
        let tip = remote.commit(Some(1), "commit 1");
        assert_eq!(sync_at(&root, &remote.url(), 1, Mode::Force).unwrap(), root.join(tip.to_string()));
        for name in stale {
            assert!(!repo.join(name).exists(), "{name} survived");
        }
        assert!(repo.join("marker").exists(), "the sweep alone should have sufficed");

        // One the sweep cannot remove (a directory here) fails the fetch with
        // a lock error: the cache is rebuilt and the fetch retried.
        fs::create_dir(repo.join("refs/mdb/head.lock")).unwrap();
        fs::write(repo.join("refs/mdb/head.lock/x"), "").unwrap();
        let tip = remote.commit(Some(1), "commit 2");
        assert_eq!(sync_at(&root, &remote.url(), 1, Mode::Force).unwrap(), root.join(tip.to_string()));
        assert!(!repo.join("refs/mdb/head.lock").exists());
        assert!(!repo.join("marker").exists());
    }

    #[test]
    fn no_copy_and_no_remote_is_a_hard_error() {
        if !have_upload_pack() {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let root = root.path().join("mdb");
        let err = format!("{:#}", sync_at(&root, BOGUS, 1, Mode::Throttled).unwrap_err());
        assert!(err.contains("cactup update"), "{err}");
        assert!(err.contains("--mdb-path"), "{err}");
        assert!(resolve_link(&root, 1).is_none());
    }
}
