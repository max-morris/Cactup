//! Network work that must outlast neither a Ctrl-C nor a remote that has
//! gone quiet: run on a thread of its own, watched from the caller's.
//!
//! gix looks at an interrupt flag only once data flows (the pack, a
//! checkout). Connecting and the handshake block in a read nothing can cut
//! short, and only the http transport has timeouts of its own (gix's reqwest
//! backend connects within 20 s, and reqwest gives up on any one wait for
//! headers or body after 30 s). A `git://` remote is a bare `TcpStream`
//! with no read timeout, an ssh one a child process: a server that accepts
//! the connection and never answers holds either for ever, and with it the
//! thread waiting on it — and the whole process, when that thread is a
//! scoped worker the main thread must join.
//!
//! [`abandonable`] runs the work on a detached thread, reporting through a
//! [`Watched`] handle that notes every movement, and polls it. It gives the
//! work up when Ctrl-C is pressed, when nothing has moved for a while (a
//! silence, not a cap on a fetch that is making progress), or past an
//! optional overall cap. Given up, the work's cancel flag is set (gix stops
//! at its next check, should the remote ever wake up), the processes the
//! work's thread started (ssh, `git upload-pack`, a credential helper) are
//! killed, which ends a read on their pipes at once, and the work gets a
//! short wind-down to unwind on its own: a fetch that does see the flag
//! drops gix's lock and temp files on its way out. A thread still stuck after
//! that is abandoned: never joined, it dies with the process, and its
//! progress handles are retired at once, so its line leaves the display
//! with the caller's last word on it rather than linger.

use crate::Res;
use prodash::messages::MessageLevel;
use prodash::progress::{Id, Step, StepShared};
use prodash::{Count, NestedProgress, Progress, Unit};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::time::{Duration, Instant};

/// How often the caller looks at the work, the interrupt flag and the clock.
const POLL: Duration = Duration::from_millis(50);

/// When [`abandonable`] gives its work up.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Bounds {
    /// The longest the work may go with nothing reported.
    pub stall: Duration,
    /// The longest the work may take in all, however steadily it moves
    /// (`None`: no cap).
    pub total: Option<Duration>,
    /// How long work given up on gets to unwind before it is abandoned.
    pub wind_down: Duration,
}

/// Why [`abandonable`] returned without the work's answer.
#[derive(Debug)]
pub(crate) enum Stop {
    /// The work finished, and failed with this.
    Failed(anyhow::Error),
    /// Ctrl-C.
    Interrupted,
    /// Nothing moved for this long ([`Bounds::stall`]).
    Stalled(Duration),
    /// The work ran past [`Bounds::total`].
    Overran(Duration),
    /// The work's thread ended without an answer (it panicked).
    Vanished,
}

/// Run `work` on a thread of its own, reporting through a [`Watched`]
/// wrapper of `handle` (a handle of its own: the caller keeps its own to
/// say how the work ended) and stopping when the flag it is given is set,
/// while this thread watches it within `bounds` and the interrupt flag. See
/// the module docs.
pub(crate) fn abandonable<P, T, W>(handle: P, bounds: Bounds, work: W) -> Result<T, Stop>
where
    P: NestedProgress + 'static,
    T: Send + 'static,
    W: FnOnce(&mut Watched<P>, &AtomicBool) -> Res<T> + Send + 'static,
{
    use std::sync::mpsc::RecvTimeoutError;
    let (send, recv) = std::sync::mpsc::channel();
    let activity = Activity::default();
    let cancel = Arc::new(AtomicBool::new(false));
    // The work's thread, as the kernel knows it (0 until it says): what its
    // child processes are listed under.
    let tid = Arc::new(AtomicU32::new(0));
    let mut watched = Watched::new(handle, activity.clone());
    let (worker_cancel, worker_tid) = (Arc::clone(&cancel), Arc::clone(&tid));
    std::thread::spawn(move || {
        worker_tid.store(rustix::thread::gettid().as_raw_nonzero().get() as u32, Ordering::SeqCst);
        let answer = work(&mut watched, &worker_cancel);
        drop(watched);
        let _ = send.send(answer);
    });
    let started = Instant::now();
    let (mut seen, mut moved) = (activity.reading(), started);
    let stop = loop {
        match recv.recv_timeout(POLL) {
            Ok(answer) => return answer.map_err(Stop::Failed),
            Err(RecvTimeoutError::Disconnected) => return Err(Stop::Vanished),
            Err(RecvTimeoutError::Timeout) => {}
        }
        if gix::interrupt::is_triggered() {
            break Stop::Interrupted;
        }
        let reading = activity.reading();
        if reading != seen {
            (seen, moved) = (reading, Instant::now());
        }
        if moved.elapsed() >= bounds.stall {
            break Stop::Stalled(bounds.stall);
        }
        if let Some(total) = bounds.total
            && started.elapsed() >= total
        {
            break Stop::Overran(total);
        }
    };
    cancel.store(true, Ordering::SeqCst);
    kill_children(tid.load(Ordering::SeqCst));
    // Whatever the work says now, it was given up on: the wait is only for
    // its clean-up.
    let _ = recv.recv_timeout(bounds.wind_down);
    activity.retire();
    Err(stop)
}

/// SIGKILL every process the thread `tid` of this process started and has
/// not yet reaped, and everything those started in turn: an ssh run through
/// `sh -c` (as a `GIT_SSH_COMMAND` is) is the shell's child, not ours, and
/// killing the shell alone would leave it holding the pipe gix reads.
/// Children are listed per thread (`/proc/<pid>/task/<tid>/children`,
/// Linux 3.5 on), so only the work's own are touched. Nothing when the
/// thread is gone, or the kernel does not list children.
///
/// A process in another session than ours is left alone, with all it
/// started: that is a daemon, not part of the fetch. OpenSSH's
/// ControlPersist master is one: the ssh that starts it forks, and the child
/// `daemon(3)`s (fork again, `setsid`) before it serves the connections other
/// sessions share; so once detached it is neither our descendant nor in our
/// session, and in the moment before, the session check spares it.
fn kill_children(tid: u32) {
    if tid == 0 {
        return;
    }
    let ours = session_of("self");
    let mut found = Vec::new();
    let mut pending = children_of(&format!("/proc/self/task/{tid}/children"));
    while let Some(pid) = pending.pop() {
        if found.contains(&pid) {
            continue;
        }
        if ours.is_some() && session_of(&pid.to_string()) != ours {
            continue;
        }
        found.push(pid);
        let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else { continue };
        for task in tasks.flatten() {
            pending.extend(children_of(&format!("{}/children", task.path().display())));
        }
    }
    // All found before any is killed: a killed process's children are
    // handed to init and listed under it no more.
    for pid in found {
        if let Some(pid) = rustix::process::Pid::from_raw(pid) {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

/// The session of process `pid` (or `self`), from `/proc/<pid>/stat` (read
/// there rather than through getsid(2): in a pid namespace a session led
/// from outside it reads as 0, which is still one session to compare).
fn session_of(pid: &str) -> Option<i64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // After the command name, which may hold anything, up to its last ')':
    // state, ppid, pgrp, session.
    stat.rsplit_once(')')?.1.split_whitespace().nth(3)?.parse().ok()
}

/// The pids a `children` file of `/proc` lists.
fn children_of(path: &str) -> Vec<i32> {
    std::fs::read_to_string(path)
        .map(|text| text.split_whitespace().filter_map(|p| p.parse().ok()).collect())
        .unwrap_or_default()
}

/// Everything a piece of work reports, reduced to one number that changes
/// whenever anything moves — how [`abandonable`] tells slow work from hung
/// work — and the handles it reports through, so they can be retired.
#[derive(Clone, Default)]
pub(crate) struct Activity(Arc<ActivityState>);

#[derive(Default)]
struct ActivityState {
    /// Bumped by every report made through a [`Watched`] handle.
    reports: AtomicU64,
    /// Counters handed out to be incremented directly, never through a
    /// handle (gix's threaded delta resolution counts that way).
    counters: Mutex<Vec<StepShared>>,
    /// Every handle handed out, to let go of when the work is abandoned.
    handles: Mutex<Vec<Weak<dyn Retire>>>,
}

impl Activity {
    fn bump(&self) {
        self.0.reports.fetch_add(1, Ordering::Relaxed);
    }

    fn watch(&self, counter: &StepShared) {
        let mut counters = self.0.counters.lock().unwrap_or_else(PoisonError::into_inner);
        if !counters.iter().any(|known| Arc::ptr_eq(known, counter)) {
            counters.push(Arc::clone(counter));
        }
    }

    fn reading(&self) -> u64 {
        let counters = self.0.counters.lock().unwrap_or_else(PoisonError::into_inner);
        counters.iter().fold(self.0.reports.load(Ordering::Relaxed), |sum, counter| {
            sum.wrapping_add(counter.load(Ordering::Relaxed) as u64)
        })
    }

    fn register(&self, handle: Weak<dyn Retire>) {
        let mut handles = self.0.handles.lock().unwrap_or_else(PoisonError::into_inner);
        handles.retain(|h| h.strong_count() > 0);
        handles.push(handle);
    }

    /// Drop every handle still held by the work: whatever it reports from
    /// now on goes nowhere.
    fn retire(&self) {
        let handles = std::mem::take(&mut *self.0.handles.lock().unwrap_or_else(PoisonError::into_inner));
        for handle in handles.iter().filter_map(Weak::upgrade) {
            handle.retire();
        }
    }
}

/// A handle [`Activity::retire`] can let go of.
trait Retire: Send + Sync {
    fn retire(&self);
}

impl<P: Send> Retire for Mutex<Option<P>> {
    fn retire(&self) {
        let handle = self.lock().unwrap_or_else(PoisonError::into_inner).take();
        drop(handle);
    }
}

/// A progress handle, and every child it hands out, that records each
/// report in an [`Activity`] before passing it on — until the work is
/// abandoned, when the handle is let go of and reports go nowhere.
pub(crate) struct Watched<P> {
    inner: Arc<Mutex<Option<P>>>,
    activity: Activity,
    /// What [`Count::step`] and [`Count::counter`] answer once retired.
    retired: StepShared,
}

impl<P: Send + 'static> Watched<P> {
    fn new(inner: P, activity: Activity) -> Watched<P> {
        let inner = Arc::new(Mutex::new(Some(inner)));
        let weak: Weak<Mutex<Option<P>>> = Arc::downgrade(&inner);
        activity.register(weak);
        Watched { inner, activity, retired: Default::default() }
    }
}

impl<P> Watched<P> {
    /// `f` on the handle, unless it was retired.
    fn with<R>(&self, f: impl FnOnce(&P) -> R) -> Option<R> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).as_ref().map(f)
    }

    fn with_mut<R>(&self, f: impl FnOnce(&mut P) -> R) -> Option<R> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner).as_mut().map(f)
    }
}

impl<P: Count> Count for Watched<P> {
    fn set(&self, step: Step) {
        self.activity.bump();
        self.with(|p| p.set(step));
    }

    fn step(&self) -> Step {
        self.with(|p| p.step()).unwrap_or_else(|| self.retired.load(Ordering::Relaxed))
    }

    fn inc_by(&self, step: Step) {
        self.activity.bump();
        self.with(|p| p.inc_by(step));
    }

    fn counter(&self) -> StepShared {
        let counter = self.with(|p| p.counter()).unwrap_or_else(|| Arc::clone(&self.retired));
        self.activity.watch(&counter);
        counter
    }
}

impl<P: Progress> Progress for Watched<P> {
    fn init(&mut self, max: Option<Step>, unit: Option<Unit>) {
        self.activity.bump();
        self.with_mut(|p| p.init(max, unit));
    }

    fn unit(&self) -> Option<Unit> {
        self.with(|p| p.unit()).flatten()
    }

    fn max(&self) -> Option<Step> {
        self.with(|p| p.max()).flatten()
    }

    fn set_max(&mut self, max: Option<Step>) -> Option<Step> {
        self.activity.bump();
        self.with_mut(|p| p.set_max(max)).flatten()
    }

    fn set_name(&mut self, name: String) {
        self.activity.bump();
        self.with_mut(|p| p.set_name(name));
    }

    fn name(&self) -> Option<String> {
        self.with(|p| p.name()).flatten()
    }

    fn id(&self) -> Id {
        self.with(|p| p.id()).unwrap_or(prodash::progress::UNKNOWN)
    }

    fn message(&self, level: MessageLevel, message: String) {
        self.activity.bump();
        self.with(|p| p.message(level, message));
    }
}

impl<P> NestedProgress for Watched<P>
where
    P: NestedProgress + 'static,
    P::SubProgress: 'static,
{
    type SubProgress = Watched<P::SubProgress>;

    fn add_child(&mut self, name: impl Into<String>) -> Watched<P::SubProgress> {
        self.child(|p| p.add_child(name))
    }

    fn add_child_with_id(&mut self, name: impl Into<String>, id: Id) -> Watched<P::SubProgress> {
        self.child(|p| p.add_child_with_id(name, id))
    }
}

impl<P: NestedProgress + 'static> Watched<P>
where
    P::SubProgress: 'static,
{
    /// A child made by `make` on the handle — or, retired, an empty one.
    fn child(&mut self, make: impl FnOnce(&mut P) -> P::SubProgress) -> Watched<P::SubProgress> {
        self.activity.bump();
        match self.with_mut(make) {
            Some(child) => Watched::new(child, self.activity.clone()),
            None => Watched {
                inner: Arc::new(Mutex::new(None)),
                activity: self.activity.clone(),
                retired: Default::default(),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bounds(stall_ms: u64) -> Bounds {
        Bounds { stall: Duration::from_millis(stall_ms), total: None, wind_down: Duration::from_millis(50) }
    }

    /// Work that keeps reporting is never given up on, however long it takes
    /// in all (three times the stall bound here, reporting every sixth of
    /// it); work that goes quiet is, after the stall bound.
    #[test]
    fn only_silence_stops_the_work() {
        let root = prodash::tree::Root::new();
        let answer = abandonable(root.add_child("moving"), bounds(1200), |progress, _| {
            progress.init(None, None);
            for _ in 0..18 {
                std::thread::sleep(Duration::from_millis(200));
                progress.inc();
            }
            Ok(42)
        });
        assert_eq!(answer.unwrap(), 42);

        let started = Instant::now();
        let stopped = abandonable(root.add_child("quiet"), bounds(300), |_, cancel| {
            while !cancel.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(())
        });
        assert!(matches!(stopped, Err(Stop::Stalled(_))), "{stopped:?}");
        assert!(started.elapsed() < Duration::from_secs(3), "{:?}", started.elapsed());
    }

    /// Work that counts only through a counter it was handed (as gix's
    /// threaded delta resolution does), never through the handle, is moving
    /// all the same.
    #[test]
    fn a_counter_handed_out_counts_as_movement() {
        let root = prodash::tree::Root::new();
        let answer = abandonable(root.add_child("resolving"), bounds(1200), |progress, _| {
            let counter = progress.counter();
            for _ in 0..18 {
                std::thread::sleep(Duration::from_millis(200));
                counter.fetch_add(1, Ordering::Relaxed);
            }
            Ok(7)
        });
        assert_eq!(answer.unwrap(), 7);
    }

    /// Work given up on is told to stop, its child processes are killed,
    /// and the handles it still holds are let go of: its item leaves the
    /// tree although its thread never ends.
    #[test]
    fn abandoned_work_is_canceled_and_let_go_of() {
        let root = prodash::tree::Root::new();
        let (pid_send, pid_recv) = std::sync::mpsc::channel();
        let (canceled_send, canceled_recv) = std::sync::mpsc::channel();
        let stopped = abandonable(root.add_child("stuck"), bounds(300), move |progress, cancel| -> Res<()> {
            let _phase = progress.add_child("handshake");
            // A shell's child, as an ssh run through `sh -c` is; and one
            // in a session of its own, as a ControlPersist master is.
            let mut child = std::process::Command::new("sh")
                .args(["-c", "sleep 30 & echo $!; setsid sleep 30 & echo $!; wait"])
                .stdout(std::process::Stdio::piped())
                .spawn()
                .unwrap();
            let mut said = std::io::BufReader::new(child.stdout.take().unwrap());
            for _ in 0..2 {
                let mut line = String::new();
                std::io::BufRead::read_line(&mut said, &mut line).unwrap();
                pid_send.send(line.trim().parse::<i32>().unwrap()).unwrap();
            }
            let _ = child.wait();
            let _ = canceled_send.send(cancel.load(Ordering::SeqCst));
            // Stuck for good, holding its handles.
            loop {
                std::thread::sleep(Duration::from_secs(60));
            }
        });
        assert!(matches!(stopped, Err(Stop::Stalled(_))), "{stopped:?}");
        let (pid, daemon) = (pid_recv.recv().unwrap(), pid_recv.recv().unwrap());
        // The shell was killed: the work's wait on it returned at once.
        assert!(canceled_recv.recv_timeout(Duration::from_secs(5)).unwrap());
        // And so was its sleep (handed to init, which reaps it).
        let gone = |pid: i32| std::fs::read_to_string(format!("/proc/{pid}/stat")).map_or(true, |stat| stat.contains(") Z "));
        let waited = Instant::now();
        while !gone(pid) && waited.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(gone(pid), "the shell's child {pid} outlived the work given up on");
        // Not the one in another session.
        let spared = !gone(daemon);
        if let Some(daemon) = rustix::process::Pid::from_raw(daemon) {
            let _ = rustix::process::kill_process(daemon, rustix::process::Signal::KILL);
        }
        assert!(spared, "the process in a session of its own was killed");
        assert_eq!(root.num_tasks(), 0);
    }
}
