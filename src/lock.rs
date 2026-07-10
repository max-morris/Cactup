//! NFS-safe `link()`-based locking (spec §2.3).
//!
//! Protocol: write a unique temp file (recording our hostname + pid) in the
//! lock's directory, then `hard_link()` it to the canonical lock path. The
//! link succeeding is the atomic acquire — atomic on POSIX including over
//! NFS, where `flock` is unreliable. Release is unlink-on-drop.
//!
//! Stale locks: same-host holders are probed via `/proc/<pid>` (no libc);
//! cross-host holders are judged solely by the lock file's mtime, compared
//! against the mtime of a freshly-created sibling file so both timestamps
//! come from the same (fileserver) clock domain. Breaking a stale lock is
//! rename-then-unlink, so of two concurrent breakers only one succeeds and
//! the other retries instead of unlinking a freshly re-acquired lock.

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

/// How often try_acquire retries after finding a vanished or breakable lock
/// before giving up. Each retry re-runs the full link() protocol.
const ACQUIRE_ATTEMPTS: u32 = 8;

fn our_hostname() -> String {
    gethostname::gethostname().to_string_lossy().into_owned()
}

/// The lock-file payload identifying a holder: `<hostname>\n<pid>\n`.
fn stamp_for(hostname: &str, pid: u32) -> String {
    format!("{hostname}\n{pid}\n")
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

    /// The shared acquire path: `Ok(Err(holder))` means a live holder owns the
    /// lock (soft failure); `Err` is an actual I/O problem.
    fn acquire_inner(path: &Path) -> Res<Result<LinkLock, String>> {
        let dir = lock_dir(path)?;
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create lock directory {}", dir.display()))?;
        let stamp = stamp_for(&our_hostname(), std::process::id());

        for _ in 0..ACQUIRE_ATTEMPTS {
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
                            break_stale(path)?;
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
    /// [`HEARTBEAT_SECS`] so cross-host staleness detection sees them as live.
    pub fn restamp(&self) -> Res<()> {
        let file = fs::OpenOptions::new()
            .write(true)
            .open(&self.path)
            .with_context(|| format!("Failed to open lock file {} to re-stamp it", self.path.display()))?;
        file.set_modified(SystemTime::now())
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

    if let Some((host, pid)) = &holder
        && *host == our_hostname()
    {
        // Same host: /proc/<pid> existing is the liveness oracle.
        return Ok(if Path::new(&format!("/proc/{pid}")).exists() {
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
            Some((host, pid)) => format!("pid {pid} on host {host}"),
            None => "an unidentified holder".to_owned(),
        }))
    }
}

fn parse_stamp(content: &str) -> Option<(String, u32)> {
    let mut lines = content.lines();
    let host = lines.next()?.to_owned();
    let pid = lines.next()?.trim().parse().ok()?;
    Some((host, pid))
}

/// Break a lock we judged stale. Rename-then-unlink makes breaking atomic
/// between competing breakers: the loser's rename fails with NotFound and it
/// simply retries the acquire loop.
fn break_stale(path: &Path) -> Res<()> {
    let mut broken = path.as_os_str().to_owned();
    broken.push(format!(
        ".breaking.{}.{}.{}",
        our_hostname(),
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or(Duration::ZERO)
            .as_nanos()
    ));
    let broken = PathBuf::from(broken);
    match fs::rename(path, &broken) {
        Ok(()) => {
            let _ = fs::remove_file(&broken);
            Ok(())
        }
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()), // beaten to it
        Err(e) => Err(e).with_context(|| format!("Failed to break stale lock {}", path.display())),
    }
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
