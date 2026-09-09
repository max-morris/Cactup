//! NFS-safe `link()`-based locking (spec §2.3).
//!
//! Protocol: write a unique temp file (recording our hostname + pid) in the
//! lock's directory, then `hard_link()` it to the canonical lock path. The
//! link succeeding is the atomic acquire — atomic on POSIX including over
//! NFS, where `flock` is unreliable. Release is unlink-on-drop.
//!
//! Stale locks: same-host holders are probed via `/proc/<pid>` (no libc),
//! with the recorded process start time distinguishing a recycled pid from
//! the original holder; cross-host holders are judged solely by the lock
//! file's mtime, compared against the mtime of a freshly-created sibling
//! file so both timestamps come from the same (fileserver) clock domain.
//! Breaking a stale lock happens under a `<path>.breaker` side-lock (taken
//! with the same link() protocol) and re-verifies the staleness verdict
//! there, so a breaker delayed between judging and breaking can never
//! rename away a lock that was freshly re-acquired in the meantime; the
//! rename-then-unlink inside still serializes any breakers that race the
//! side-lock's crash recovery.

// Consumed by the Phase-2/3 streams (DB, INST, CFG, SIM); unused until then.

use crate::Res;
use anyhow::{anyhow, bail, Context};
use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Cadence at which a live long-running holder re-stamps locks / heartbeats.
pub const HEARTBEAT_SECS: u64 = 60;
/// A restart heartbeat older than this is considered stale by the reaper (§8.3).
pub const HEARTBEAT_STALE_SECS: u64 = 300;
/// A lock whose mtime has not advanced for this long may be broken (cross-host).
pub const LOCK_STALE_SECS: u64 = 900;
/// How long a compute-node handoff waits for the submitter to release the
/// per-simulation / per-test-run lock (§8.3.1, §11.6) before giving up.
/// Well under LOCK_STALE_SECS, so a genuinely dead holder is still only ever
/// broken by the staleness rules, never by this wait running out.
pub const HANDOFF_WAIT_SECS: u64 = 300;

/// How often try_acquire retries after finding a vanished or breakable lock
/// before giving up. Each retry re-runs the full link() protocol.
const ACQUIRE_ATTEMPTS: u32 = 8;

fn our_hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The lock-file payload identifying a holder:
/// `<hostname>\n<pid>\n[<pid-start-time>\n]`. The start time pins the stamp
/// to one incarnation of the pid, so a recycled pid does not read as a live
/// holder; the line is absent when /proc has no answer (dead pid, non-Linux).
fn stamp_for(hostname: &str, pid: u32) -> String {
    match proc_starttime(pid) {
        Some(start) => format!("{hostname}\n{pid}\n{start}\n"),
        None => format!("{hostname}\n{pid}\n"),
    }
}

/// The process start time (clock ticks since boot) from `/proc/<pid>/stat`
/// field 22 — the standard pid-reuse discriminator. `comm` (field 2) may
/// contain spaces and parentheses, so fields are counted after the last `)`.
pub(crate) fn proc_starttime(pid: u32) -> Option<u64> {
    proc_stat_field(pid, 22)
}

/// The session id from `/proc/<pid>/stat` field 6: the pid of the session
/// leader, which for an interactive login is the login shell. Together with
/// that leader's start time it names one login session (§4.3's verification
/// stamp).
pub(crate) fn proc_session(pid: u32) -> Option<u32> {
    proc_stat_field(pid, 6)
}

/// Field `n` (1-based, as `proc(5)` numbers them) of `/proc/<pid>/stat`.
/// Fields are counted after the last `)` because `comm` (field 2) may hold
/// spaces and parentheses, so field 3 is index 0 of the remainder.
fn proc_stat_field<T: std::str::FromStr>(pid: u32, n: usize) -> Option<T> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    stat.rsplit_once(')')?.1.split_whitespace().nth(n - 3)?.parse().ok()
}

/// What we concluded about the current holder of a lock file.
enum Holder {
    /// The lock file disappeared while we were looking at it.
    Vanished,
    /// Held by a live process; the string names it for error messages.
    Live(String),
    /// Dead same-host pid or an mtime past [`LOCK_STALE_SECS`]: may be broken.
    Stale,
}

/// A held `link()`-based lock. The lock file records the holder's hostname and
/// pid. Released (unlinked) on drop.
#[derive(Debug)]
pub struct LinkLock {
    path: PathBuf,
    /// Exactly what we wrote into the lock file; on release we unlink only if
    /// the file still carries it, in case a stale-break stole the lock from us.
    stamp: String,
}

impl LinkLock {
    /// Try to acquire the lock at `path`, breaking a stale lock per §2.3
    /// (same host: probe `/proc/<pid>` — NOT libc `kill(pid, 0)`; this project
    /// must not depend on the libc crate. Different host: mtime older than
    /// [`LOCK_STALE_SECS`]). Returns `Ok(None)` when held by a live holder.
    // Pinned foundation API (§2.3); production code uses the blocking
    // `acquire`, tests exercise this form.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn try_acquire(path: &Path) -> Res<Option<LinkLock>> {
        match Self::acquire_inner(path)? {
            Ok(lock) => Ok(Some(lock)),
            Err(_holder) => Ok(None),
        }
    }

    /// Acquire the lock at `path` or fail with an error naming the holder.
    pub fn acquire(path: &Path) -> Res<LinkLock> {
        match Self::acquire_inner(path)? {
            Ok(lock) => Ok(lock),
            Err(holder) => bail!(
                "{} is locked by {holder}. If that process is gone, the lock \
                 will expire on its own; a live holder re-stamps it every \
                 {HEARTBEAT_SECS}s.",
                path.display()
            ),
        }
    }

    /// Acquire the lock at `path`, waiting up to `timeout` for a live holder to
    /// release it (one probe per second, interrupt-aware). This is the form the
    /// compute-node handoffs use: the submitter still holds the simulation's
    /// (or test run's) lock for a moment after the scheduler has accepted the
    /// job, and on an idle partition the job can start inside that window —
    /// failing fast there aborted the run on nothing more than the submitter's
    /// own bookkeeping. A holder that never releases still fails, naming it.
    // §2.3 item 3, §8.3.1 handoff.
    pub fn acquire_wait(path: &Path, timeout: Duration) -> Res<LinkLock> {
        let started = std::time::Instant::now();
        loop {
            let holder = match Self::acquire_inner(path)? {
                Ok(lock) => return Ok(lock),
                Err(holder) => holder,
            };
            if gix::interrupt::is_triggered() {
                bail!("interrupted while waiting for lock {}", path.display());
            }
            let elapsed = started.elapsed();
            if elapsed >= timeout {
                bail!(
                    "{} is still locked by {holder} after waiting {}s. If that process \
                     is gone, the lock will expire on its own; a live holder re-stamps \
                     it every {HEARTBEAT_SECS}s.",
                    path.display(),
                    timeout.as_secs()
                );
            }
            std::thread::sleep((timeout - elapsed).min(Duration::from_secs(1)));
        }
    }

    /// The shared acquire path: `Ok(Err(holder))` means a live holder owns the
    /// lock (soft failure); `Err` is an actual I/O problem.
    fn acquire_inner(path: &Path) -> Res<Result<LinkLock, String>> {
        let dir = lock_dir(path)?;
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create lock directory {}", dir.display()))?;
        let stamp = stamp_for(&our_hostname(), std::process::id());

        for attempt in 0..ACQUIRE_ATTEMPTS {
            if attempt > 0 {
                // Brief backoff so contending acquirers don't spin the full
                // link/assess/break cycle against each other in lockstep.
                std::thread::sleep(Duration::from_millis(25 * u64::from(attempt)));
            }
            let temp = tempfile::Builder::new()
                .prefix(".cactup-lock.")
                .tempfile_in(dir)
                .with_context(|| format!("Failed to create lock temp file in {}", dir.display()))?;
            temp.as_file()
                .write_all(stamp.as_bytes())
                .with_context(|| format!("Failed to write lock temp file {}", temp.path().display()))?;

            let link_result = fs::hard_link(temp.path(), path);

            // NFS can lose the *reply* to a successful LINK and report an
            // error for a link that in fact happened; the temp file's link
            // count is the ground truth.
            let nlink = {
                use std::os::unix::fs::MetadataExt;
                temp.as_file().metadata().map(|m| m.nlink()).unwrap_or(1)
            };

            if link_result.is_ok() || nlink == 2 {
                return Ok(Ok(LinkLock { path: path.to_owned(), stamp }));
            }

            match link_result {
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                    // The temp file was just written, so its mtime is the
                    // fileserver's "now" — the same clock that stamped the
                    // existing lock's mtime.
                    let fs_now = temp
                        .as_file()
                        .metadata()
                        .and_then(|m| m.modified())
                        .with_context(|| "Failed to read lock temp file mtime")?;
                    match assess_holder(path, fs_now)? {
                        Holder::Vanished => continue, // released under us; retry
                        Holder::Live(holder) => return Ok(Err(holder)),
                        Holder::Stale => {
                            break_stale(path, fs_now)?;
                            continue;
                        }
                    }
                }
                Err(e) => {
                    return Err(e).with_context(|| {
                        format!("Failed to link lock file {}", path.display())
                    });
                }
                Ok(()) => unreachable!(),
            }
        }
        bail!(
            "Gave up acquiring lock {} after {ACQUIRE_ATTEMPTS} attempts \
             (it keeps being taken and released or broken under us)",
            path.display()
        )
    }

    /// Re-stamp the lock's mtime; long-running holders call this every
    /// [`HEARTBEAT_SECS`] so cross-host staleness detection sees them as
    /// live. Rewriting the stamp (rather than set_modified(now)) stamps the
    /// mtime with the *fileserver's* clock — the same domain assess_holder
    /// measures staleness in; the local clock may be skewed from it.
    pub fn restamp(&self) -> Res<()> {
        fs::write(&self.path, &self.stamp)
            .with_context(|| format!("Failed to re-stamp lock file {}", self.path.display()))
    }

    /// Whether `path` is currently held by a live holder (per the §2.3
    /// liveness rules). Used by the stale-restart reaper on `running.lock`.
    pub fn is_held_live(path: &Path) -> Res<bool> {
        if !path.exists() {
            return Ok(false);
        }
        let dir = lock_dir(path)?;
        let temp = tempfile::Builder::new()
            .prefix(".cactup-lock.")
            .tempfile_in(dir)
            .with_context(|| format!("Failed to create probe temp file in {}", dir.display()))?;
        let fs_now = temp
            .as_file()
            .metadata()
            .and_then(|m| m.modified())
            .with_context(|| "Failed to read probe temp file mtime")?;
        Ok(matches!(assess_holder(path, fs_now)?, Holder::Live(_)))
    }
}

/// A [`LinkLock`] kept alive by a background heartbeat thread that re-stamps
/// it every [`HEARTBEAT_SECS`], for holders doing long work (builds, runs)
/// that would otherwise cross the [`LOCK_STALE_SECS`] window (§2.3).
/// Dropping stops the thread and releases the lock.
pub struct HeartbeatLock {
    lock: std::sync::Arc<LinkLock>,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LinkLock {
    /// Wrap this lock with the heartbeat thread.
    pub fn with_heartbeat(self) -> HeartbeatLock {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        let lock = Arc::new(self);
        let stop = Arc::new(AtomicBool::new(false));
        let thread = {
            let (lock, stop) = (Arc::clone(&lock), Arc::clone(&stop));
            std::thread::spawn(move || {
                let step = std::time::Duration::from_millis(250);
                let steps_per_beat = (HEARTBEAT_SECS * 1000 / 250) as u32;
                loop {
                    for _ in 0..steps_per_beat {
                        if stop.load(Ordering::Relaxed) {
                            return;
                        }
                        std::thread::sleep(step);
                    }
                    let _ = lock.restamp(); // best-effort; §2.3
                }
            })
        };
        HeartbeatLock { lock, stop, thread: Some(thread) }
    }
}

impl Drop for HeartbeatLock {
    fn drop(&mut self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join(); // wakes within one 250ms step
        }
        // `self.lock` (the last Arc) drops here, unlinking the lock file.
        debug_assert_eq!(std::sync::Arc::strong_count(&self.lock), 1);
    }
}

impl Drop for LinkLock {
    fn drop(&mut self) {
        // Unlink only while the file is still ours: if we went quiet past
        // LOCK_STALE_SECS, another process may have broken the lock and
        // re-acquired it, and then the path no longer belongs to us.
        match fs::read_to_string(&self.path) {
            Ok(content) if content == self.stamp => {
                let _ = fs::remove_file(&self.path);
            }
            _ => {}
        }
    }
}

fn lock_dir(path: &Path) -> Res<&Path> {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .ok_or_else(|| anyhow!("Lock path {} has no parent directory", path.display()))
}

/// Judge the holder recorded in the lock file at `path`. `fs_now` must be an
/// mtime freshly minted on the same filesystem, so the cross-host age
/// comparison stays in one clock domain.
fn assess_holder(path: &Path, fs_now: SystemTime) -> Res<Holder> {
    let meta = match fs::symlink_metadata(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Holder::Vanished),
        other => other.with_context(|| format!("Failed to stat lock file {}", path.display()))?,
    };
    let content = match fs::read_to_string(path) {
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Holder::Vanished),
        other => other.with_context(|| format!("Failed to read lock file {}", path.display()))?,
    };

    let holder = parse_stamp(&content);

    if let Some((host, pid, start)) = &holder
        && *host == our_hostname()
    {
        // Same host: /proc/<pid> existing is the liveness oracle, refined by
        // the recorded start time — a pid whose current occupant started at
        // a different tick is a recycled pid, and the holder is dead.
        let live = Path::new(&format!("/proc/{pid}")).exists()
            && match start {
                Some(start) => proc_starttime(*pid) == Some(*start),
                None => true, // old-style stamp: pid existence is all we have
            };
        return Ok(if live {
            Holder::Live(format!("pid {pid} on this host ({host})"))
        } else {
            Holder::Stale
        });
    }

    // Different host (or unparsable stamp): liveness falls back solely to the
    // mtime timeout — a remote pid cannot be probed.
    let mtime = meta
        .modified()
        .with_context(|| format!("Failed to read mtime of lock file {}", path.display()))?;
    let age = fs_now.duration_since(mtime).unwrap_or(Duration::ZERO);
    if age >= Duration::from_secs(LOCK_STALE_SECS) {
        Ok(Holder::Stale)
    } else {
        Ok(Holder::Live(match holder {
            Some((host, pid, _)) => format!("pid {pid} on host {host}"),
            None => "an unidentified holder".to_owned(),
        }))
    }
}

fn parse_stamp(content: &str) -> Option<(String, u32, Option<u64>)> {
    let mut lines = content.lines();
    let host = lines.next()?.to_owned();
    let pid = lines.next()?.trim().parse().ok()?;
    let start = lines.next().and_then(|l| l.trim().parse().ok());
    Some((host, pid, start))
}

/// A breaker side-lock older than this is a corpse (breaking takes
/// milliseconds) and is swept aside so stale-breaking cannot deadlock on a
/// crashed breaker.
const BREAKER_STALE_SECS: u64 = 60;

/// Break a lock we judged stale. Breaking is itself mutually exclusive, via
/// a `<path>.breaker` side-lock taken with the same link() protocol, and the
/// staleness verdict is re-checked *under* that side-lock: without this, a
/// breaker delayed between judging and renaming could rename away a lock
/// that a faster breaker had already broken and freshly re-acquired — two
/// holders at once, the one hole rename-then-unlink alone leaves open.
/// Losing the side-lock is not an error: the acquire loop re-assesses from
/// scratch on its next attempt. The rename-then-unlink is kept so that even
/// breakers racing the corpse sweep-aside can never unlink a re-acquired
/// lock directly.
fn break_stale(path: &Path, fs_now: SystemTime) -> Res<()> {
    let breaker = {
        let mut p = path.as_os_str().to_owned();
        p.push(".breaker");
        PathBuf::from(p)
    };
    let dir = lock_dir(path)?;
    let temp = tempfile::Builder::new()
        .prefix(".cactup-lock.")
        .tempfile_in(dir)
        .with_context(|| format!("Failed to create breaker temp file in {}", dir.display()))?;
    temp.as_file()
        .write_all(stamp_for(&our_hostname(), std::process::id()).as_bytes())
        .with_context(|| format!("Failed to write breaker temp file {}", temp.path().display()))?;
    let link_result = fs::hard_link(temp.path(), &breaker);
    let nlink = {
        use std::os::unix::fs::MetadataExt;
        temp.as_file().metadata().map(|m| m.nlink()).unwrap_or(1)
    };
    if !(link_result.is_ok() || nlink == 2) {
        return match link_result {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                // Another breaker is at work; sweep its side-lock aside only
                // if it is a corpse. Either way this attempt yields — by the
                // next acquire attempt the lock is fresh, gone, or still
                // stale and breakable.
                if let Ok(mtime) = fs::symlink_metadata(&breaker).and_then(|m| m.modified())
                    && fs_now.duration_since(mtime).unwrap_or(Duration::ZERO)
                        >= Duration::from_secs(BREAKER_STALE_SECS)
                {
                    let _ = fs::remove_file(&breaker);
                }
                Ok(())
            }
            Err(e) => {
                Err(e).with_context(|| format!("Failed to link breaker {}", breaker.display()))
            }
            Ok(()) => unreachable!(),
        };
    }

    // Under the breaker: re-judge, and only then break. A verdict other than
    // Stale means the lock changed hands (or vanished) since we judged it —
    // exactly the case the side-lock exists to catch.
    let result = match assess_holder(path, fs_now) {
        Ok(Holder::Stale) => {
            let mut broken = path.as_os_str().to_owned();
            broken.push(format!(
                ".breaking.{}.{}",
                our_hostname(),
                std::process::id(),
            ));
            let broken = PathBuf::from(broken);
            match fs::rename(path, &broken) {
                Ok(()) => {
                    let _ = fs::remove_file(&broken);
                    Ok(())
                }
                Err(e) if e.kind() == ErrorKind::NotFound => Ok(()), // beaten to it
                Err(e) => Err(e)
                    .with_context(|| format!("Failed to break stale lock {}", path.display())),
            }
        }
        Ok(_) => Ok(()), // vanished or freshly re-acquired: nothing to break
        Err(e) => Err(e),
    };
    let _ = fs::remove_file(&breaker);
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lock_path(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("test.lock")
    }

    /// A pid that cannot exist: default /proc/sys/kernel/pid_max is 4194304.
    const DEAD_PID: u32 = 999_999_999;

    #[test]
    fn acquire_release_reacquire() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        let lock = LinkLock::acquire(&path).unwrap();
        assert!(path.exists());
        let content = fs::read_to_string(&path).unwrap();
        assert_eq!(content, stamp_for(&our_hostname(), std::process::id()));

        drop(lock);
        assert!(!path.exists());

        let _lock = LinkLock::acquire(&path).unwrap();
        assert!(path.exists());
    }

    #[test]
    fn held_by_live_same_host_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        // Our own pid is certainly alive.
        let _held = LinkLock::acquire(&path).unwrap();
        assert!(LinkLock::try_acquire(&path).unwrap().is_none());
        assert!(LinkLock::is_held_live(&path).unwrap());

        let err = LinkLock::acquire(&path).unwrap_err().to_string();
        assert!(err.contains(&std::process::id().to_string()), "error names the pid: {err}");
    }

    #[test]
    fn acquire_wait_outlasts_a_short_lived_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        let held = LinkLock::acquire(&path).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
        });
        // Plain acquire fails fast while the holder is live ...
        assert!(LinkLock::try_acquire(&path).unwrap().is_none());
        // ... the waiting form outlasts it.
        let lock = LinkLock::acquire_wait(&path, Duration::from_secs(10)).unwrap();
        releaser.join().unwrap();
        assert!(path.exists());
        drop(lock);
        assert!(!path.exists());
    }

    #[test]
    fn acquire_wait_times_out_on_a_live_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        let _held = LinkLock::acquire(&path).unwrap();
        let started = std::time::Instant::now();
        let err = LinkLock::acquire_wait(&path, Duration::from_millis(250)).unwrap_err().to_string();
        assert!(started.elapsed() >= Duration::from_millis(250));
        assert!(err.contains(&std::process::id().to_string()), "error names the holder: {err}");
        assert!(err.contains("after waiting"), "error says it waited: {err}");
    }

    #[test]
    fn breaks_dead_same_host_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, stamp_for(&our_hostname(), DEAD_PID)).unwrap();
        assert!(!LinkLock::is_held_live(&path).unwrap());

        let lock = LinkLock::try_acquire(&path).unwrap();
        assert!(lock.is_some(), "stale same-host lock must be broken and taken");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            stamp_for(&our_hostname(), std::process::id())
        );
    }

    #[test]
    fn breaks_recycled_pid_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        // Our own (live) pid, but a start time that cannot be this
        // incarnation's: the recorded holder is a previous occupant of the
        // pid, i.e. dead.
        fs::write(&path, format!("{}\n{}\n1\n", our_hostname(), std::process::id())).unwrap();
        assert!(!LinkLock::is_held_live(&path).unwrap());
        assert!(LinkLock::try_acquire(&path).unwrap().is_some(), "recycled pid must be breakable");
    }

    #[test]
    fn stamp_records_a_verifiable_start_time() {
        let stamp = stamp_for(&our_hostname(), std::process::id());
        let (host, pid, start) = parse_stamp(&stamp).unwrap();
        assert_eq!(host, our_hostname());
        assert_eq!(pid, std::process::id());
        assert!(start.is_some(), "/proc must yield our own start time");
        assert_eq!(start, proc_starttime(std::process::id()));
        // An old-style two-line stamp still parses (start unknown).
        assert_eq!(parse_stamp("h\n42\n"), Some(("h".to_owned(), 42, None)));
    }

    fn breaker_path(path: &Path) -> PathBuf {
        let mut p = path.as_os_str().to_owned();
        p.push(".breaker");
        PathBuf::from(p)
    }

    #[test]
    fn a_live_breaker_blocks_stale_breaking() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, stamp_for(&our_hostname(), DEAD_PID)).unwrap();
        // A fresh side-lock: someone is mid-break right now. Every break
        // attempt must yield to it, so the acquire eventually gives up.
        fs::write(breaker_path(&path), "x").unwrap();
        let err = LinkLock::acquire(&path).unwrap_err().to_string();
        assert!(err.contains("Gave up"), "{err}");
        assert!(path.exists(), "the stale lock must not be broken past a live breaker");
    }

    #[test]
    fn a_crashed_breaker_is_swept_aside() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, stamp_for(&our_hostname(), DEAD_PID)).unwrap();
        let breaker = breaker_path(&path);
        fs::write(&breaker, "x").unwrap();
        let old = SystemTime::now() - Duration::from_secs(BREAKER_STALE_SECS + 60);
        fs::OpenOptions::new().write(true).open(&breaker).unwrap().set_modified(old).unwrap();

        // The corpse is removed and the stale lock then broken and taken.
        let lock = LinkLock::try_acquire(&path).unwrap();
        assert!(lock.is_some(), "a crashed breaker must not block stale-breaking forever");
        assert!(!breaker.exists(), "the break released its side-lock");
    }

    #[test]
    fn respects_fresh_cross_host_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, stamp_for("some-other-host", 1)).unwrap();
        assert!(LinkLock::try_acquire(&path).unwrap().is_none());
        assert!(LinkLock::is_held_live(&path).unwrap());

        let err = LinkLock::acquire(&path).unwrap_err().to_string();
        assert!(err.contains("some-other-host"), "error names the host: {err}");
    }

    #[test]
    fn breaks_stale_cross_host_holder() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        fs::write(&path, stamp_for("some-other-host", 1)).unwrap();
        let old = SystemTime::now() - Duration::from_secs(LOCK_STALE_SECS + 60);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();

        assert!(!LinkLock::is_held_live(&path).unwrap());
        assert!(LinkLock::try_acquire(&path).unwrap().is_some());
    }

    #[test]
    fn restamp_advances_mtime() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        let lock = LinkLock::acquire(&path).unwrap();
        let old = SystemTime::now() - Duration::from_secs(LOCK_STALE_SECS + 60);
        fs::OpenOptions::new()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(old)
            .unwrap();

        lock.restamp().unwrap();
        let mtime = fs::metadata(&path).unwrap().modified().unwrap();
        assert!(mtime > old + Duration::from_secs(LOCK_STALE_SECS));
    }

    #[test]
    fn drop_leaves_foreign_lock_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = lock_path(&dir);

        let lock = LinkLock::acquire(&path).unwrap();
        // Simulate another process having broken + re-acquired our lock.
        fs::write(&path, stamp_for("thief-host", 42)).unwrap();
        drop(lock);
        assert!(path.exists(), "drop must not unlink a lock that is no longer ours");
    }

    #[test]
    fn missing_lock_is_not_held() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!LinkLock::is_held_live(&lock_path(&dir)).unwrap());
    }
}
