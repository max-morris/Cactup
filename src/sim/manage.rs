//! `sim stop` / `clean` (§8.6), `sim delete` (§8.7), `sim list` / `sim show`
//! (including `sim show --output-dir`), and `sim log`.

use crate::commands::Ctx;
use crate::installation::{Installation, SimEntry};
use crate::scheduler::{
    display_state, is_live, DisplayState, JobRecord, JobRef, JobStatus, Scheduler, Statuses, Stopped, UnverifiedPid,
};
use crate::sim::restart::{self, Restart, NO_JOB_ID};
use crate::sim::{cache, Simulation};
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::fs;
use std::path::{Path, PathBuf};

fn machine_and_inst(ctx: &Ctx) -> Res<(crate::mdb::Machine, Installation)> {
    Ok((crate::commands::machine::resolve(ctx)?, Installation::resolve(ctx)?))
}

/// How long `sim stop` watches a graceful termination before handing the
/// decision back to the user. Cactus only reads `TERMINATE` at its next
/// termination check and usually checkpoints on the way out, so this is a
/// "did the request land" probe, not a wait for the run to finish.
const GRACEFUL_WAIT_SECS: u64 = 30;

/// Is the run behind `r` still alive? The queue is authoritative when the
/// restart has a job the scheduler here can be asked about; a foreground run
/// has none, and a pid recorded on another host is none this host can ask
/// about (§10), so the liveness marker answers instead (§9.3).
fn run_is_live(sched: &Scheduler, r: &Restart) -> Res<bool> {
    if r.meta.job_id != NO_JOB_ID && sched.recorded_elsewhere(r.meta.job()).is_none() {
        let status = sched.get_status_of(r.meta.job(), r.meta.terminated)?;
        return Ok(is_live(status, r.meta.terminated));
    }
    crate::lock::LinkLock::is_held_live(&r.running_lock_path())
}

/// Poll for up to [`GRACEFUL_WAIT_SECS`] until the run behind `r` is gone;
/// `true` means it really left. Interrupt-aware (§2.3): Ctrl-C fails the wait
/// rather than reporting a run gone that is still there.
fn await_run_exit(sched: &Scheduler, r: &Restart) -> Res<bool> {
    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let waiting = progress.add_child(if r.meta.job_id == NO_JOB_ID {
        "waiting for the run to exit".to_owned()
    } else {
        format!("waiting for job {} to leave the queue", r.meta.job_id)
    });
    waiting.init(Some(GRACEFUL_WAIT_SECS as usize), Some(prodash::unit::label("s")));

    let started = std::time::Instant::now();
    let outcome = loop {
        match run_is_live(sched, r) {
            Ok(false) => break Ok(true),
            Err(e) => break Err(e),
            Ok(true) => {}
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed >= GRACEFUL_WAIT_SECS {
            break Ok(false);
        }
        waiting.set(elapsed as usize);
        // Sleep in short ticks so Ctrl-C stops us within a fraction of a second.
        let tick = std::time::Instant::now();
        while tick.elapsed() < std::time::Duration::from_secs(3) {
            if gix::interrupt::is_triggered() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        if gix::interrupt::is_triggered() {
            break Err(anyhow::anyhow!("interrupted while waiting for the job to stop"));
        }
    };

    drop(waiting);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    outcome
}

/// `sim stop` (§8.6): graceful via the `TERMINATE` trigger when present;
/// forced (or `-f`) via the machine `stop` command. Both finish the restart —
/// but a graceful request that the run has not acted on yet leaves the restart
/// active, so the user can escalate with `-f`.
pub fn stop(ctx: &Ctx, name: &str, force: bool) -> Res<()> {
    let (machine, inst) = machine_and_inst(ctx)?;
    let sim = Simulation::locate(&inst, name)?;
    let _lock = sim.lock()?;

    let Some(active) = restart::active_id(&sim.dir)? else {
        println!("Simulation {} has no active restart; nothing to stop", name.bold());
        return Ok(());
    };
    let mut r = Restart::load(&sim.dir, active)?;
    let terminate = r.dir.join("TERMINATE");

    if terminate.is_file() && !force {
        fs::write(&terminate, b"1\n")
            .with_context(|| format!("Failed to write {}", terminate.display()))?;
        println!(
            "Requested graceful termination of {} (wrote 1 into TERMINATE)",
            restart::dir_name(active)
        );
        sim.log("stop", &format!("graceful termination of {} requested", restart::dir_name(active)));

        // The trigger is a request, not a kill: report what actually happened
        // instead of finishing the restart on faith. While the run is still
        // there the restart stays active — deactivating it would hide the live
        // job from `stop -f` and from the live-job guard in `delete` (§8.7).
        let sched = Scheduler::new(&machine.meta);
        if !await_run_exit(&sched, &r)? {
            let what = if r.meta.job_id == NO_JOB_ID {
                format!("{} is still running", restart::dir_name(active))
            } else {
                format!("job {} is still in the queue", r.meta.job_id)
            };
            println!(
                "{what} after {GRACEFUL_WAIT_SECS}s: Cactus acts on the trigger at its next \
                 termination check, often after a checkpoint. Leaving {} active — re-run \
                 `cactup sim stop -f {name}` to kill it now.",
                restart::dir_name(active)
            );
            sim.log(
                "stop",
                &format!("{} still live after the graceful request", restart::dir_name(active)),
            );
            return Ok(());
        }
        println!("{} stopped", restart::dir_name(active).bold());
    } else {
        if r.meta.job_id != NO_JOB_ID {
            let sched = Scheduler::new(&machine.meta);
            match sched.stop(r.meta.job())? {
                Stopped::Sent => println!("Stopped job {} via the scheduler", r.meta.job_id.bold()),
                // A pid whose process is not the job's any more (§10).
                Stopped::Gone => println!("Job {} had ended already; nothing to stop", r.meta.job_id),
                // Another host's pid (§10): its lock says whether it runs.
                Stopped::Elsewhere(host) if crate::lock::LinkLock::is_held_live(&r.running_lock_path())? => {
                    println!(
                        "Job {} runs on host {host}, where it was submitted, and this host cannot stop it: \
                         stop it there (`cactup sim stop -f {name}` on {host}). Nothing recorded.",
                        r.meta.job_id
                    );
                    return Ok(());
                }
                Stopped::Elsewhere(host) => println!(
                    "Job {} was submitted on host {host}, and nothing of its run holds the restart's \
                     lock: taken as ended",
                    r.meta.job_id
                ),
            }
        }
        sim.log("stop", &format!("stopped job {} ({})", r.meta.job_id, restart::dir_name(active)));
    }

    // finish: mark terminated + deactivate (§8.6).
    r.meta.terminated = true;
    r.meta.finished = Some(chrono::Utc::now());
    r.store()?;
    restart::deactivate(&sim.dir)?;
    Ok(())
}

/// `sim clean` (§8.6): deactivate + tighten TERMINATE + Formaline tarball
/// hard-link dedup. Checkpoints are never touched (§8.8).
pub fn clean(ctx: &Ctx, name: &str) -> Res<()> {
    let (_machine, inst) = machine_and_inst(ctx)?;
    let sim = Simulation::locate(&inst, name)?;
    let _lock = sim.lock()?;
    if restart::active_id(&sim.dir)?.is_none() {
        println!("Simulation {} has no active restart; nothing to clean", name.bold());
        return Ok(());
    }
    clean_active(&sim)?;
    println!("Cleaned simulation {}", name.bold());
    Ok(())
}

/// The §8.6 clean routine for the currently-active restart. Also the reaper's
/// auto-clean (§8.3). Caller holds the per-sim lock.
pub fn clean_active(sim: &Simulation) -> Res<()> {
    let Some(id) = restart::deactivate(&sim.dir)? else { return Ok(()) };
    let rdir = restart::restart_dir(&sim.dir, id);

    // Mark the restart finished if its run never did.
    if let Ok(mut r) = Restart::load(&sim.dir, id) && !r.meta.terminated {
        r.meta.terminated = true;
        r.meta.finished = Some(chrono::Utc::now());
        let _ = r.store();
    }

    // Tighten TERMINATE perms.
    let terminate = rdir.join("TERMINATE");
    if terminate.is_file() {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&terminate, fs::Permissions::from_mode(0o400));
    }

    // Checkpoints are left strictly alone (§8.6/§8.8): they may be any format,
    // in any location the parfile chose, so cactup cannot reliably recognize one
    // — and a wrong guess deletes real data.

    dedup_formaline(sim, id)?;
    sim.log("clean", &format!("cleaned {}", restart::dir_name(id)));
    Ok(())
}

/// Recursively collect files under `root`, as paths relative to it.
fn walk_relative(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(dir) else { return };
        for entry in entries.flatten() {
            let path = entry.path();
            match entry.file_type() {
                Ok(t) if t.is_dir() => walk(&path, root, out),
                Ok(t) if t.is_file() => {
                    if let Ok(rel) = path.strip_prefix(root) {
                        out.push(rel.to_owned());
                    }
                }
                _ => {}
            }
        }
    }
    let mut out = Vec::new();
    walk(root, root, &mut out);
    out
}

fn files_identical(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (fs::metadata(a), fs::metadata(b)) else { return false };
    if ma.len() != mb.len() {
        return false;
    }
    match (fs::read(a), fs::read(b)) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => false,
    }
}

/// Formaline tarball hard-link dedup, semantics preserved verbatim from
/// simfactory (§8.6): for each `*.tar.gz` ≥ 1000 bytes in restart `id`, scan
/// prior restarts (descending) for a byte-identical file at the same relative
/// path, and replace this copy with a hard link to it via a `.tmp` rename.
fn dedup_formaline(sim: &Simulation, id: u32) -> Res<()> {
    let rdir = restart::restart_dir(&sim.dir, id);
    let prior: Vec<u32> = restart::list_ids(&sim.dir)?.into_iter().filter(|&p| p < id).collect();
    if prior.is_empty() {
        return Ok(());
    }
    for rel in walk_relative(&rdir) {
        let is_tarball = rel.to_str().map(|s| s.ends_with(".tar.gz")).unwrap_or(false);
        if !is_tarball {
            continue;
        }
        let ours = rdir.join(&rel);
        if fs::metadata(&ours).map(|m| m.len() < 1000).unwrap_or(true) {
            continue;
        }
        for &p in prior.iter().rev() {
            let theirs = restart::restart_dir(&sim.dir, p).join(&rel);
            if !theirs.is_file() || !files_identical(&ours, &theirs) {
                continue;
            }
            // Byte-identical: replace ours with a hard link via .tmp rename.
            let tmp = ours.with_extension("gz.tmp");
            if fs::hard_link(&theirs, &tmp).is_ok() && fs::rename(&tmp, &ours).is_err() {
                let _ = fs::remove_file(&tmp);
            }
            break;
        }
    }
    Ok(())
}

/// `sim delete` (§8.7): refuse on live jobs without `-f`; `-f` stops them and
/// permanently removes; the default moves the sim into `TRASH/` — then GC the
/// executable cache.
pub fn delete(ctx: &Ctx, name: &str, force: bool) -> Res<()> {
    let (machine, inst) = machine_and_inst(ctx)?;
    let sim_home = inst.meta()?.sim_home()?.to_owned();

    // A stale registry entry (dir vanished) is pruned rather than fataled.
    let registry = inst.simulations()?;
    let Some(entry) = registry.simulations.get(name) else {
        bail!("no simulation named \"{name}\" (see `cactup sim list`)");
    };
    if !entry.dir.is_dir() {
        let locked = inst.locked()?;
        let mut reg = locked.simulations()?;
        reg.simulations.shift_remove(name);
        locked.set_simulations(&reg)?;
        println!(
            "Simulation {} directory {} was already gone; pruned the stale registry entry",
            name.bold(),
            entry.dir.display()
        );
        return Ok(());
    }

    let sim = Simulation::locate(&inst, name)?;
    let sched = Scheduler::new(&machine.meta);

    // The lock comes BEFORE the live-job guard: submit holds it for the whole
    // submission (§2.3 item 3), so taking it first closes the window where a
    // concurrent submit queues a job between our scan and the removal.
    let _lock = sim.lock()?;

    // Live-job guard (§8.7).
    let live = live_jobs(&sim, &sched)?;
    if !live.is_empty() {
        if !force {
            bail!(
                "simulation \"{name}\" has live jobs ({}); `cactup sim stop {name}` first, \
                 or use -f to stop and delete",
                live.iter()
                    .map(|(id, job)| format!("{} = job {}", restart::dir_name(*id), job.id))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for (_, job) in &live {
            // A pid cactup cannot vouch for (§10) says itself that nothing ran.
            sched.stop(job.job()).map_err(|e| match e.downcast_ref::<UnverifiedPid>() {
                Some(_) => e,
                None => e.context(format!("stopping job {}", job.id)),
            })?;
        }
    }

    sim.log("delete", if force { "purging simulation" } else { "moving simulation to TRASH" });

    if force {
        // -f implies --purge (§8.7 ASSUMPTION): permanent removal, links no
        // longer count for cache GC.
        fs::remove_dir_all(&sim.dir)
            .with_context(|| format!("Failed to remove {}", sim.dir.display()))?;
        println!("Deleted simulation {} permanently", name.bold());
    } else {
        let dst = cache::move_to_trash(&sim_home, &sim.dir, &sim.meta.simulation_id)?;
        println!("Moved simulation {} to {}", name.bold(), dst.display());
    }

    let locked = inst.locked()?;
    let mut reg = locked.simulations()?;
    reg.simulations.shift_remove(name);
    locked.set_simulations(&reg)?;
    drop(locked);

    // Executable-cache GC (§8.1): authoritative on delete.
    let reaped = cache::gc(&sim_home, None)?;
    if !reaped.is_empty() && ctx.globals.verbose {
        eprintln!("Reaped orphaned executable cache entries: {}", reaped.join(", "));
    }
    Ok(())
}

/// Whether restart `id`, whose run recorded its end, is still asked about
/// (§8.6, §10): only on a batch scheduler, which may requeue a job, and
/// only the simulation's newest restart, the one a requeue concerns. An
/// older restart's job id is never asked again: on a batch scheduler it may
/// by now name an unrelated job (ids wrap around, a cluster is reinstalled,
/// two clusters share a home), which would refuse a delete or be stopped.
fn asks_after_end(sched: &Scheduler, id: u32, newest: Option<u32>) -> bool {
    sched.may_requeue() && newest == Some(id)
}

/// The restarts of `sim` whose jobs are live (§8.7), asked about in one
/// batch (`get_statuses`: the machine's listing, else the bounded pool of
/// per-job queries) under a progress line. Of the restarts whose run
/// recorded its end only the newest is asked about, where a batch scheduler
/// may have requeued its job, and is live then only queued or held
/// (`is_live`); where job ids are pids none is (`asks_after_end`). A job whose query failed is asked once more on
/// its own: before its run's end that is an error, and `delete` refuses
/// with it; after, it is taken as ended. A job recorded on another host
/// (pids) refuses the delete while its restart's `running.lock` is live,
/// whatever `-f` says: this host can neither ask about it nor stop it.
fn live_jobs(sim: &Simulation, sched: &Scheduler) -> Res<Vec<(u32, JobRecord)>> {
    let mut asked: Vec<(u32, Restart)> = Vec::new();
    let ids = restart::list_ids(&sim.dir)?;
    let newest = ids.last().copied();
    for id in ids {
        let Ok(r) = Restart::load(&sim.dir, id) else { continue };
        if r.meta.job_id == NO_JOB_ID || (r.meta.terminated && !asks_after_end(sched, id, newest)) {
            continue;
        }
        if let Some(host) = sched.recorded_elsewhere(r.meta.job()) {
            if crate::lock::LinkLock::is_held_live(&r.running_lock_path())? {
                bail!(
                    "restart {} of simulation \"{}\" runs on host {host}, where its job {} was \
                     submitted, and this host can neither ask about it nor stop it: stop it there \
                     (`cactup sim stop -f {}` on {host}), then delete the simulation",
                    restart::dir_name(id),
                    sim.name,
                    r.meta.job_id,
                    sim.name
                );
            }
            continue;
        }
        asked.push((id, r));
    }
    let jobs: Vec<JobRef> = asked.iter().map(|(_, r)| r.meta.job()).collect();

    let (progress, renderer) = crate::manifest::setup_prodash_if_tty();
    let asking = progress.add_child("ask the scheduler");
    asking.init(Some(jobs.len()), Some(prodash::unit::label("jobs")));
    let asking = std::sync::Mutex::new(asking);
    let statuses = sched.get_statuses_counted(&jobs, &|n| asking.lock().expect("progress poisoned").inc_by(n));
    drop(asking);
    if let Some(renderer) = renderer {
        renderer.shutdown_and_wait();
    }
    if gix::interrupt::is_triggered() {
        bail!("interrupted");
    }

    let mut live = Vec::new();
    for (id, r) in &asked {
        let ended = r.meta.terminated;
        let status = match statuses.get(r.meta.job()) {
            Some(status) => *status,
            None => sched.get_status_of(r.meta.job(), ended)?,
        };
        if is_live(status, ended) {
            live.push((*id, r.meta.job().to_record()));
        }
    }
    Ok(live)
}

/// One simulation's derived display state (§8.6, §10) with the live scheduler
/// query held back: [`SimState::state`] is exact once the queried status is
/// folded in, and [`SimState::job_to_query`] names the only job whose status
/// can still change the answer. Splitting the query out is what lets `sim list`
/// resolve a whole registry in one batched round of scheduler calls instead of
/// one call per simulation.
struct SimState {
    /// The restart the state is derived from: the active one, else the latest.
    subject: Option<u32>,
    job: Option<JobRecord>,
    active: bool,
    chained: bool,
    terminated: bool,
    /// Set when the state is decided whatever the queue says: no restart at
    /// all, or unreadable restart metadata.
    forced: Option<DisplayState>,
}

impl SimState {
    /// A simulation with nothing to derive a state from.
    fn nothing() -> SimState {
        SimState {
            subject: None,
            job: None,
            active: false,
            chained: false,
            terminated: false,
            forced: Some(DisplayState::Inactive),
        }
    }

    /// The job whose live status still has to be queried, if any. Only an
    /// active restart — or a non-active one holding a pre-submitted chain
    /// (§8.6) — is sensitive to it; anything else is INACTIVE whatever the
    /// queue says, so a long finished history costs no scheduler calls. A
    /// restart whose run recorded its end is asked about only where a batch
    /// scheduler may have requeued its job (`may_requeue`, §10).
    fn job_to_query(&self, may_requeue: bool) -> Option<JobRef<'_>> {
        if self.forced.is_some() || !(self.active || self.chained) || (self.terminated && !may_requeue) {
            return None;
        }
        self.job.as_ref().map(JobRecord::job)
    }

    /// The derived state, given the query of [`SimState::job_to_query`]:
    /// `None` when there was nothing to query, `Some(None)` when the query
    /// failed (see [`shown`]).
    fn state(&self, query: Option<Option<JobStatus>>) -> DisplayState {
        self.forced.unwrap_or_else(|| shown(self.active, self.chained, self.terminated, query))
    }
}

/// What a display makes of a restart's queried status (§8.6, §10): `query`
/// is `None` when nothing was asked, `Some(None)` when the query failed.
/// Before its run recorded its end, a failed query is UNKNOWN, never a
/// state the query did not give. After the end, a failed query, a job
/// listed running (Slurm's `CG`, `TO`) or in a state no pattern reads is
/// the ended job on its way out of the queue (`is_live`); only a queued or
/// held one is a job put back in the queue.
fn shown(active: bool, chained: bool, terminated: bool, query: Option<Option<JobStatus>>) -> DisplayState {
    let status = match query {
        Some(None) if !terminated => return DisplayState::Unknown,
        Some(Some(status)) if terminated && !is_live(status, true) => Some(JobStatus::Unknown),
        Some(status) => status,
        None => None,
    };
    display_state(active, status, chained, terminated)
}

/// One simulation's display state from a directory scan already taken, minus
/// the live query (§8.6, §10) — see [`SimState`].
fn sim_state(sim: &Simulation, scan: &restart::Scan) -> SimState {
    let active = scan.active_id(&sim.dir).ok().flatten();
    // The restart that determines the state: the active one, else the latest.
    let Some(subject) = active.or_else(|| scan.latest()) else { return SimState::nothing() };
    let Ok(r) = Restart::load(&sim.dir, subject) else {
        return SimState { subject: Some(subject), ..SimState::nothing() };
    };
    SimState {
        subject: Some(subject),
        job: (r.meta.job_id != NO_JOB_ID).then(|| r.meta.job().to_record()),
        active: active.is_some(),
        chained: r.meta.chained_job_id.is_some(),
        terminated: r.meta.terminated,
        forced: None,
    }
}

/// [`sim_state`] for a single simulation: scan and query it on the spot.
fn sim_state_now(
    sim: &Simulation,
    sched: &Scheduler,
) -> (DisplayState, Option<u32>, Option<String>) {
    let Ok(scan) = restart::scan(&sim.dir) else {
        return (DisplayState::Inactive, None, None);
    };
    let st = sim_state(sim, &scan);
    let query = st.job_to_query(sched.may_requeue()).map(|job| sched.get_status(job).ok());
    (st.state(query), st.subject, st.job.map(|job| job.id))
}

fn state_str(state: DisplayState) -> colored::ColoredString {
    match state {
        DisplayState::Presubmitted => "PRESUBMITTED".cyan(),
        DisplayState::Running => "RUNNING".green().bold(),
        DisplayState::Queued => "QUEUED".cyan(),
        DisplayState::Holding => "HOLDING".yellow(),
        DisplayState::Active => "ACTIVE".green(),
        DisplayState::Finished => "FINISHED".blue(),
        DisplayState::Error => "ERROR".red().bold(),
        DisplayState::Unknown => "UNKNOWN".yellow(),
        DisplayState::Inactive => "INACTIVE".normal(),
    }
}

/// `sim show` (§8.6): one simulation in detail.
pub fn show(ctx: &Ctx, name: &str, long: bool, output_dir: bool, restart_id: Option<u32>) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    if output_dir {
        return print_output_dir(&inst, name, restart_id);
    }
    let machine = crate::commands::machine::resolve(ctx)?;
    let sched = Scheduler::new(&machine.meta);
    show_one(&inst, &sched, name, long)
}

/// How wide the row-gathering pool runs. Each row is a handful of stats and
/// small reads on what is usually a networked filesystem, so a long history is
/// spent waiting rather than working — but stay bounded, since the thing being
/// waited on is one shared metadata server.
const ROW_WORKERS: usize = 8;

/// One `sim list` row, gathered from disk before any scheduler query.
enum Row {
    /// A registry entry whose directory is gone (§8.1).
    Missing(PathBuf),
    /// The directory is there but is not a readable cactup simulation.
    Broken(String),
    Listed { config: String, restarts: usize, dir: PathBuf, state: SimState },
}

impl Row {
    fn job_to_query(&self, may_requeue: bool) -> Option<JobRef<'_>> {
        match self {
            Row::Listed { state, .. } => state.job_to_query(may_requeue),
            _ => None,
        }
    }
}

/// Everything one row needs from disk: the registry entry's directory, the
/// simulation metadata, and a single scan of the restart directories.
fn gather_row(name: &str, entry: &SimEntry) -> Row {
    if !entry.dir.is_dir() {
        return Row::Missing(entry.dir.clone());
    }
    let sim = match Simulation::open(name, &entry.dir) {
        Ok(sim) => sim,
        Err(e) => return Row::Broken(format!("{e:#}")),
    };
    // One scan feeds both the restart count and the state; an unreadable
    // simulation directory leaves the row restart-less, as it always has.
    let scan = restart::scan(&sim.dir).ok();
    Row::Listed {
        config: entry.config.clone(),
        restarts: scan.as_ref().map_or(0, |s| s.ids.len()),
        state: scan.as_ref().map_or_else(SimState::nothing, |scan| sim_state(&sim, scan)),
        dir: sim.dir,
    }
}

/// Gather every row on the [`ROW_WORKERS`] pool, back in registry order.
fn gather_rows(entries: &[(&String, &SimEntry)]) -> Vec<Row> {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let next = AtomicUsize::new(0);
    let rows: std::sync::Mutex<Vec<(usize, Row)>> = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..ROW_WORKERS.min(entries.len()).max(1) {
            scope.spawn(|| loop {
                let i = next.fetch_add(1, Ordering::Relaxed);
                let Some((name, entry)) = entries.get(i) else { return };
                let row = gather_row(name, entry);
                rows.lock().expect("sim list rows poisoned").push((i, row));
            });
        }
    });
    let mut rows = rows.into_inner().expect("sim list rows poisoned");
    rows.sort_unstable_by_key(|(i, _)| *i);
    rows.into_iter().map(|(_, row)| row).collect()
}

fn print_row(name: &str, row: &Row, statuses: &Statuses, may_requeue: bool, long: bool) {
    match row {
        Row::Missing(dir) => println!(
            "  {:24} {:12} {} (prune with `cactup sim delete {name}`)",
            name.bold(),
            "MISSING".red(),
            dir.display()
        ),
        Row::Broken(e) => println!("  {:24} {:12} {e}", name.bold(), "BROKEN".red()),
        Row::Listed { config, restarts, dir, state } => {
            let query = state.job_to_query(may_requeue).map(|job| statuses.get(job).copied());
            let mut extra = format!("config {config}, {restarts} restart(s)");
            if let Some(job) = &state.job {
                extra.push_str(&format!(", job {}", job.id));
            }
            if long {
                if let Some(s) = state.subject {
                    extra.push_str(&format!(", latest {}", restart::dir_name(s)));
                }
                extra.push_str(&format!(", {}", dir.display()));
            }
            println!("  {:24} {:12} {}", name.bold(), state_str(state.state(query)), extra);
        }
    }
}

/// `sim list` (§8.1, §8.6): list the registry; `--all` unions every
/// installation's registry.
///
/// Done in three passes rather than one simulation at a time, because both the
/// per-simulation directory scans and the scheduler status queries are
/// latency-bound: a cluster with a long history spent one `squeue` round-trip
/// per simulation, serially, which is what made this crawl (§10).
pub fn list(ctx: &Ctx, long: bool, all: bool) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let sched = Scheduler::new(&machine.meta);

    let installations: Vec<Installation> = if all {
        let db = ctx.db.read()?;
        db.installations
            .values()
            .map(|i| Installation::new(i.alias.clone(), i.path.clone()))
            .collect()
    } else {
        vec![Installation::resolve(ctx)?]
    };

    let registries = installations
        .iter()
        .map(|inst| inst.simulations())
        .collect::<Res<Vec<_>>>()?;
    let entries: Vec<(&String, &SimEntry)> =
        registries.iter().flat_map(|reg| reg.simulations.iter()).collect();
    if entries.is_empty() {
        println!("No simulations (create one with `cactup sim create <name> <parfile>`)");
        return Ok(());
    }

    let rows = gather_rows(&entries);
    // One batched query for the simulations whose state the queue can still
    // change; the rest of the history needs no scheduler round-trip at all.
    let pending: Vec<JobRef> = rows.iter().filter_map(|row| row.job_to_query(sched.may_requeue())).collect();
    let statuses = sched.get_statuses(&pending);

    let mut rows = rows.iter();
    for (inst, registry) in installations.iter().zip(&registries) {
        if registry.simulations.is_empty() {
            continue;
        }
        if all {
            println!("{}", format!("[{}]", inst.alias).bold());
        }
        for name in registry.simulations.keys() {
            let row = rows.next().expect("one row per registry entry");
            print_row(name, row, &statuses, sched.may_requeue(), long);
        }
    }
    Ok(())
}

fn show_one(inst: &Installation, sched: &Scheduler, name: &str, long: bool) -> Res<()> {
    let sim = Simulation::locate(inst, name)?;
    let (state, _, _) = sim_state_now(&sim, sched);

    println!("{}", name.bold());
    println!("  state:         {}", state_str(state));
    println!("  directory:     {}", sim.dir.display());
    println!("  configuration: {} (build-id {})", sim.meta.configuration, sim.meta.build_id);
    println!("  machine:       {}", sim.meta.machine);
    println!("  parfile:       {}", sim.meta.parfile);
    println!("  simulation-id: {}", sim.meta.simulation_id);
    if long {
        // Build provenance snapshotted into `.cactup/cfg/` at create time
        // (§8.2): what this sim's frozen binary was compiled from, which
        // outlives a rebuild or deletion of the config itself. Empty for
        // simulations created before cactup recorded the artifact.
        let cfg = sim.dir.join(".cactup/cfg");
        for (label, file) in
            [("optionlist", &sim.meta.optionlist), ("thornlist", &sim.meta.thornlist)]
        {
            // Padded to the width of the longest label above ("simulation-id").
            let label = format!("{label}:");
            if file.is_empty() {
                println!("  {label:<14} {}", "(not recorded)".yellow());
            } else {
                println!("  {label:<14} {}", cfg.join(file).display());
            }
        }
    }

    let scan = restart::scan(&sim.dir)?;
    let active = scan.active_id(&sim.dir)?;
    if scan.ids.is_empty() {
        println!("  restarts:      none");
        return Ok(());
    }
    println!("  restarts:");
    // Load the whole chain first, so every restart's live status comes out of
    // one batched query instead of a scheduler round-trip each (§10).
    let loaded: Vec<Res<Restart>> =
        scan.ids.iter().map(|&id| Restart::load(&sim.dir, id)).collect();
    // Of the restarts whose run recorded its end, only the newest is asked
    // about, and only where a batch scheduler may have requeued its job
    // (`asks_after_end`).
    let newest = scan.ids.last().copied();
    let asked =
        |r: &Restart| r.meta.job_id != NO_JOB_ID && (!r.meta.terminated || asks_after_end(sched, r.id, newest));
    let queries: Vec<JobRef> = loaded
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|r| asked(r))
        .map(|r| r.meta.job())
        .collect();
    let statuses = sched.get_statuses(&queries);

    for (&id, loaded) in scan.ids.iter().zip(&loaded) {
        let marker = if active == Some(id) { " (active)" } else { "" };
        match loaded {
            Ok(r) => {
                let query = asked(r).then(|| statuses.get(r.meta.job()).copied());
                let status = query.flatten();
                let state = shown(active == Some(id), r.meta.chained_job_id.is_some(), r.meta.terminated, query);
                let mut line = format!(
                    "    {}{marker}: {} — job {}, queue {}, wall {}",
                    restart::dir_name(id),
                    state_str(state),
                    r.meta.job_id,
                    r.meta.queue,
                    r.meta.walltime.canonical(),
                );
                if long {
                    if let Some(chained) = &r.meta.chained_job_id {
                        line.push_str(&format!(", after job {chained}"));
                    }
                    if let Some(u) = &r.meta.universe {
                        line.push_str(&format!(", universe {}", u.name));
                    }
                    if let Some(s) = status {
                        // The raw simfactory-letter scheduler status (§10),
                        // alongside the derived display state.
                        line.push_str(&format!(", sched {}", s.letter()));
                        if s == JobStatus::Running
                            && let Ok(Some(host)) = sched.exec_host(&r.meta.job_id)
                        {
                            line.push_str(&format!(", host {host}"));
                        }
                    }
                }
                println!("{line}");
            }
            Err(_) => println!("    {}{marker}: (no metadata)", restart::dir_name(id)),
        }
    }
    Ok(())
}

/// `sim show --output-dir`: print the active (or Nth) restart's directory.
fn print_output_dir(inst: &Installation, name: &str, restart_id: Option<u32>) -> Res<()> {
    let sim = Simulation::locate(inst, name)?;
    let id = match restart_id {
        Some(id) => id,
        None => match restart::active_id(&sim.dir)? {
            Some(id) => id,
            None => *restart::list_ids(&sim.dir)?
                .last()
                .ok_or_else(|| anyhow::anyhow!("simulation \"{name}\" has no restarts yet"))?,
        },
    };
    let dir = restart::restart_dir(&sim.dir, id);
    if !dir.is_dir() {
        bail!("restart {} does not exist", restart::dir_name(id));
    }
    println!("{}", dir.display());
    Ok(())
}

/// `sim log`: print the tail of the active/latest restart's stdout/stderr
/// (paths from the frozen `@STDOUT_FILE@`/`@STDERR_FILE@` vars when present).
/// Per `mode`, keep streaming newly-appended bytes until Ctrl-C.
pub fn log_cmd(ctx: &Ctx, name: &str, mode: crate::tail::FollowMode) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let sim = Simulation::locate(&inst, name)?;
    let id = restart::active_id(&sim.dir)?
        .or(restart::list_ids(&sim.dir)?.last().copied());
    let Some(id) = id else {
        bail!("simulation \"{name}\" has no restarts yet (nothing to show)");
    };
    let rdir = restart::restart_dir(&sim.dir, id);

    let (mut out, mut err) = (
        rdir.join(format!("{name}.out")),
        rdir.join(format!("{name}.err")),
    );
    if let Ok(r) = Restart::load(&sim.dir, id) {
        if let Some(toml::Value::String(s)) = r.meta.vars.get("STDOUT_FILE") {
            out = PathBuf::from(s);
        }
        if let Some(toml::Value::String(s)) = r.meta.vars.get("STDERR_FILE") {
            err = PathBuf::from(s);
        }
    }

    let sources = [("stdout", out), ("stderr", err)];
    let subject = format!("{} {}", name.bold(), restart::dir_name(id));
    crate::tail::tail_log(&sources, mode, &subject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimulationMeta;

    fn sim_at(dir: &Path) -> Simulation {
        let meta = SimulationMeta { parfile: "bbh.par".to_owned(), ..Default::default() };
        Simulation { name: "bbh".to_owned(), dir: dir.to_owned(), meta }
    }

    #[test]
    fn formaline_dedup_links_identical_only() {
        let tmp = tempfile::tempdir().unwrap();
        let sim = sim_at(tmp.path());
        for id in 0..2 {
            fs::create_dir_all(restart::workdir(&sim, id).join("cactus")).unwrap();
        }
        let big = vec![b'x'; 2000];
        let w0 = restart::workdir(&sim, 0).join("cactus");
        let w1 = restart::workdir(&sim, 1).join("cactus");
        // Identical big tarball → deduped; different content → kept; small → kept.
        fs::write(w0.join("src.tar.gz"), &big).unwrap();
        fs::write(w1.join("src.tar.gz"), &big).unwrap();
        fs::write(w0.join("other.tar.gz"), &big).unwrap();
        fs::write(w1.join("other.tar.gz"), vec![b'y'; 2000]).unwrap();
        fs::write(w0.join("tiny.tar.gz"), b"abc").unwrap();
        fs::write(w1.join("tiny.tar.gz"), b"abc").unwrap();

        dedup_formaline(&sim, 1).unwrap();

        use std::os::unix::fs::MetadataExt;
        assert_eq!(fs::metadata(w1.join("src.tar.gz")).unwrap().nlink(), 2, "identical → hard link");
        assert_eq!(fs::metadata(w1.join("other.tar.gz")).unwrap().nlink(), 1, "different → untouched");
        assert_eq!(fs::metadata(w1.join("tiny.tar.gz")).unwrap().nlink(), 1, "< 1000 bytes → untouched");
        assert_eq!(fs::read(w1.join("other.tar.gz")).unwrap(), vec![b'y'; 2000]);
    }

    #[test]
    fn clean_active_tidies_the_restart() {
        let tmp = tempfile::tempdir().unwrap();
        let sim = sim_at(tmp.path());
        fs::create_dir_all(sim.cactup_dir()).unwrap();
        let w = restart::workdir(&sim, 0);
        fs::create_dir_all(&w).unwrap();
        fs::create_dir_all(restart::restart_dir(&sim.dir, 0).join(".cactup")).unwrap();
        fs::write(restart::restart_dir(&sim.dir, 0).join("TERMINATE"), b"0\n").unwrap();
        // Anything checkpoint-shaped, half-written or not, must survive: cactup
        // does not know what a checkpoint looks like and never guesses (§8.8).
        fs::write(w.join("bbh.chkpt.tmp.it_10.h5"), b"half").unwrap();
        fs::write(w.join("bbh.chkpt.it_5.h5"), b"good").unwrap();
        restart::make_active(&sim.dir, 0).unwrap();

        clean_active(&sim).unwrap();

        assert_eq!(restart::active_id(&sim.dir).unwrap(), None, "deactivated");
        assert!(w.join("bbh.chkpt.tmp.it_10.h5").exists(), "checkpoints left alone");
        assert!(w.join("bbh.chkpt.it_5.h5").exists(), "checkpoints left alone");
        use std::os::unix::fs::PermissionsExt;
        let mode = fs::metadata(restart::restart_dir(&sim.dir, 0).join("TERMINATE"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o400, "TERMINATE perms tightened");
    }

    /// One simulation on disk under `root`: a `simulation.toml` plus a single
    /// `output-0000` restart — just enough for `gather_row`/`sim_state`.
    fn fake_sim(
        root: &Path,
        name: &str,
        job_id: &str,
        active: bool,
        chained: Option<&str>,
        terminated: bool,
    ) -> SimEntry {
        let dir = root.join(name);
        let meta = SimulationMeta { parfile: "bbh.par".to_owned(), ..Default::default() };
        crate::installation::write_toml(&dir.join(".cactup").join("simulation.toml"), &meta).unwrap();

        let rdir = restart::restart_dir(&dir, 0);
        fs::create_dir_all(rdir.join(".cactup")).unwrap();
        let mut toml_text = format!(
            "created = \"2024-01-01T00:00:00Z\"\n\
             nodes = 1\n\
             tasks = 1\n\
             tpn = 1\n\
             cpus = 1\n\
             queue = \"debug\"\n\
             walltime = \"24:00:00\"\n\
             checkpt-buffer = \"00:10:00\"\n\
             job-id = \"{job_id}\"\n"
        );
        if let Some(chain) = chained {
            toml_text.push_str(&format!("chained-job-id = \"{chain}\"\n"));
        }
        if terminated {
            toml_text.push_str("terminated = true\n");
        }
        fs::write(rdir.join(".cactup").join("restart.toml"), toml_text).unwrap();

        if active {
            restart::make_active(&dir, 0).unwrap();
        }

        SimEntry { dir, config: "sim".to_owned(), created: chrono::Utc::now() }
    }

    /// The `SimState` behind a `Row::Listed`; panics on anything else.
    fn listed_state(row: &Row) -> &SimState {
        match row {
            Row::Listed { state, .. } => state,
            _ => panic!("expected Row::Listed"),
        }
    }

    #[test]
    fn list_rows_query_only_the_live_simulations() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // Two finished-history sims — the case that dominates a real cluster
        // history — then one live sim, one pre-submitted chain, one NO_JOB_ID
        // active sim, one missing directory, one broken (no simulation.toml).
        let hist1 = fake_sim(root, "hist1", "100", false, None, true);
        let hist2 = fake_sim(root, "hist2", "101", false, None, true);
        let live = fake_sim(root, "live", "200", true, None, false);
        let chain = fake_sim(root, "chain", "300", false, Some("301"), false);
        let nojob = fake_sim(root, "nojob", NO_JOB_ID, true, None, false);
        let missing = SimEntry {
            dir: root.join("ghost"),
            config: "sim".to_owned(),
            created: chrono::Utc::now(),
        };
        let broken_dir = root.join("broken");
        fs::create_dir_all(&broken_dir).unwrap();
        let broken =
            SimEntry { dir: broken_dir, config: "sim".to_owned(), created: chrono::Utc::now() };

        let names: Vec<String> = ["hist1", "hist2", "live", "chain", "nojob", "missing", "broken"]
            .into_iter()
            .map(String::from)
            .collect();
        let values = [hist1, hist2, live, chain, nojob, missing, broken];
        let entries: Vec<(&String, &SimEntry)> = names.iter().zip(values.iter()).collect();

        let rows = gather_rows(&entries);
        assert_eq!(rows.len(), 7, "one row per registry entry");

        for (i, row) in rows.iter().enumerate().take(5) {
            match row {
                Row::Listed { restarts, config, .. } => {
                    assert_eq!(*restarts, 1, "entry {i}");
                    assert_eq!(config, "sim", "entry {i}");
                }
                _ => panic!("entry {i}: expected Row::Listed"),
            }
        }
        assert!(matches!(&rows[5], Row::Missing(_)), "missing directory");
        assert!(matches!(&rows[6], Row::Broken(_)), "no simulation.toml");

        let pending: Vec<&str> = rows.iter().filter_map(|row| row.job_to_query(true)).map(|job| job.id).collect();
        assert_eq!(
            pending,
            vec!["200", "300"],
            "a finished history must cost zero scheduler round-trips"
        );
    }

    #[test]
    fn deferred_states_match_the_derivation() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        let live = fake_sim(root, "live", "200", true, None, false);
        let hist = fake_sim(root, "hist", "100", false, None, true);
        let chain = fake_sim(root, "chain", "300", false, Some("301"), false);
        let nojob = fake_sim(root, "nojob", NO_JOB_ID, true, None, false);

        let names: Vec<String> =
            ["live", "hist", "chain", "nojob"].into_iter().map(String::from).collect();
        let values = [live, hist, chain, nojob];
        let entries: Vec<(&String, &SimEntry)> = names.iter().zip(values.iter()).collect();
        let rows = gather_rows(&entries);

        let live_state = listed_state(&rows[0]);
        assert_eq!(live_state.state(Some(Some(JobStatus::Running))), DisplayState::Running);
        assert_eq!(live_state.state(None), DisplayState::Active, "never asked, but the restart is still active");
        assert_eq!(live_state.state(Some(None)), DisplayState::Unknown, "a failed query shows no state it did not give");

        let hist_state = listed_state(&rows[1]);
        assert_eq!(
            hist_state.state(None),
            DisplayState::Inactive,
            "finished history is INACTIVE whatever the queue says"
        );

        let chain_state = listed_state(&rows[2]);
        assert_eq!(chain_state.state(Some(Some(JobStatus::Queued))), DisplayState::Presubmitted);

        let nojob_state = listed_state(&rows[3]);
        assert_eq!(nojob_state.state(None), display_state(true, None, false, false));
    }

    /// A restart whose run recorded its end (`terminated`): on a batch
    /// scheduler its job is still asked about, so a requeued one is a live
    /// job to `delete` and the display; where job ids are pids it is done,
    /// never asked. A status query that fails is an error to `delete`.
    #[test]
    fn a_terminated_restarts_job_is_asked_about_only_where_it_may_be_requeued() {
        use crate::mdb::meta::Meta;
        let fake = |scheduler: &str| -> Meta {
            toml::from_str(&format!(
                "[machine]\nnickname = \"fake\"\n[scheduler]\n{scheduler}\n[queues.local]\ndefault = true\n\
                 [submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n"
            ))
            .unwrap()
        };
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let entry = fake_sim(tmp.path(), "requeued", "4242", true, None, true);
        let sim = Simulation::open("requeued", &entry.dir).unwrap();
        let listing = format!(
            "get-status = \"echo @JOB_ID@ >> {}; echo '@JOB_ID@ PD'\"\nstatus-pattern = \"^@JOB_ID@ \"\nqueued-pattern = \" PD\"",
            log.display()
        );

        let batch = fake(&listing);
        let batch = Scheduler::new(&batch);
        let live = live_jobs(&sim, &batch).unwrap();
        assert_eq!(live.iter().map(|(id, job)| (*id, job.id.as_str())).collect::<Vec<_>>(), [(0, "4242")]);
        let (state, _, _) = sim_state_now(&sim, &batch);
        assert_eq!(state, DisplayState::Queued);

        let pids = fake(&format!("job-ids = \"pids\"\n{listing}"));
        let pids = Scheduler::new(&pids);
        fs::remove_file(&log).unwrap();
        assert!(live_jobs(&sim, &pids).unwrap().is_empty());
        let (state, _, _) = sim_state_now(&sim, &pids);
        assert_eq!(state, DisplayState::Finished);
        assert!(!log.exists(), "a pid was asked about after its run's end");

        let failing = fake(
            "get-status = \"echo 'slurm_load_jobs error: Socket timed out on send/recv operation' >&2; exit 1\"\n\
             unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'\nstatus-pattern = \"^@JOB_ID@ \"",
        );
        let failing = Scheduler::new(&failing);
        // Ended: a failed query is taken as the job ended, never a refusal
        // for good.
        assert!(live_jobs(&sim, &failing).unwrap().is_empty());
        let (state, _, _) = sim_state_now(&sim, &failing);
        assert_eq!(state, DisplayState::Finished);
        // Not ended: the query's failure is the delete's refusal, and the
        // display says the state is unknown.
        let path = restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml");
        let text = fs::read_to_string(&path).unwrap().replace("terminated = true", "terminated = false");
        fs::write(&path, text).unwrap();
        let err = format!("{:#}", live_jobs(&sim, &failing).unwrap_err());
        assert!(err.contains("the status query for job 4242"), "{err}");
        let (state, _, _) = sim_state_now(&sim, &failing);
        assert_eq!(state, DisplayState::Unknown);

        // Ended, and the scheduler lists the job running (Slurm's `CG` on
        // its way out) or in a state no pattern reads (`CD`): the ended
        // job, neither live nor shown as running; queued is a requeued job.
        let text = fs::read_to_string(&path).unwrap().replace("terminated = false", "terminated = true");
        fs::write(&path, text).unwrap();
        for (state_letter, live, shown) in [("CG", false, DisplayState::Finished), ("CD", false, DisplayState::Finished), ("PD", true, DisplayState::Queued)] {
            let m = fake(&format!(
                "get-status = \"echo '@JOB_ID@ {state_letter} '\"\nstatus-pattern = \"^@JOB_ID@ \"\nqueued-pattern = \" PD \"\nrunning-pattern = \" (CF|CG|R|TO) \""
            ));
            let sched = Scheduler::new(&m);
            assert_eq!(!live_jobs(&sim, &sched).unwrap().is_empty(), live, "{state_letter}");
            assert_eq!(sim_state_now(&sim, &sched).0, shown, "{state_letter}");
        }
    }

    /// Where job ids are pids, a restart's job recorded on another host is
    /// never asked about or taken for gone while the restart's
    /// `running.lock` is live: `delete` refuses, `-f` too. With the lock let
    /// go it is taken as ended.
    #[test]
    fn a_job_on_another_host_holds_off_delete_while_its_lock_is_live() {
        use crate::mdb::meta::Meta;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let entry = fake_sim(tmp.path(), "away", "4242", true, None, false);
        let sim = Simulation::open("away", &entry.dir).unwrap();
        let path = restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml");
        let text = fs::read_to_string(&path).unwrap().replace("job-id = \"4242\"", "job-id = \"4242\"\njob-host = \"some-other-host\"\njob-start = 7");
        fs::write(&path, text).unwrap();
        let m: Meta = toml::from_str(&format!(
            "[machine]\nnickname = \"fake\"\n[scheduler]\njob-ids = \"pids\"\nget-status = \"echo @JOB_ID@ >> {}\"\n\
             [queues.local]\ndefault = true\n[submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
            log.display()
        ))
        .unwrap();
        let sched = Scheduler::new(&m);
        let r = Restart::load(&sim.dir, 0).unwrap();
        let held = crate::lock::LinkLock::acquire(&r.running_lock_path()).unwrap();
        let err = live_jobs(&sim, &sched).unwrap_err().to_string();
        assert!(err.contains("runs on host some-other-host") && err.contains("stop it there"), "{err}");
        assert!(run_is_live(&sched, &r).unwrap(), "the graceful stop takes the lock for it");
        drop(held);
        assert!(live_jobs(&sim, &sched).unwrap().is_empty());
        assert!(!run_is_live(&sched, &r).unwrap());
        assert!(!log.exists(), "another host's pid was asked about");
    }

    /// `sim delete`'s batch: a failing `get-status-many` listing (exit 1)
    /// is no job gone; the per-job queries answer, and their failure refuses
    /// the delete before the restart's end and is taken as ended after it.
    /// Of the terminated restarts only the newest is asked about: an older
    /// one's job id may by now name an unrelated job.
    #[test]
    fn sim_delete_asks_job_by_job_when_the_listing_fails_and_only_the_newest_ended_restart() {
        use crate::mdb::meta::Meta;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let entry = fake_sim(tmp.path(), "s", "4242", true, None, false);
        let sim = Simulation::open("s", &entry.dir).unwrap();
        let timeout = "echo 'slurm_load_jobs error: Socket timed out on send/recv operation' >&2; exit 1";
        let fake = |single: &str| -> Meta {
            toml::from_str(&format!(
                "[machine]\nnickname = \"fake\"\n[scheduler]\nget-status-many = \"{timeout}\"\n\
                 get-status = \"echo @JOB_ID@ >> {log}; {single}\"\n\
                 unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'\n\
                 status-pattern = \"^@JOB_ID@ \"\nqueued-pattern = \" PD\"\nrunning-pattern = \" R\"\n\
                 [queues.local]\ndefault = true\n[submitscripts]\n\"default\" = [\"local\"]\n\
                 [runscripts]\n\"default\" = [\"local\"]\n",
                log = log.display()
            ))
            .unwrap()
        };
        let answers = fake("echo '@JOB_ID@ R'");
        let answers = Scheduler::new(&answers);
        let live = live_jobs(&sim, &answers).unwrap();
        assert_eq!(live.iter().map(|(_, job)| job.id.as_str()).collect::<Vec<_>>(), ["4242"]);

        let failing = fake(timeout);
        let failing = Scheduler::new(&failing);
        let err = format!("{:#}", live_jobs(&sim, &failing).unwrap_err());
        assert!(err.contains("the status query for job 4242"), "{err}");
        assert_eq!(sim_state_now(&sim, &failing).0, DisplayState::Unknown);
        let path = restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml");
        let ended = fs::read_to_string(&path).unwrap() + "terminated = true\n";
        fs::write(&path, &ended).unwrap();
        assert!(live_jobs(&sim, &failing).unwrap().is_empty());

        // A newer restart: the older terminated one is no longer asked about.
        let newer = restart::restart_dir(&sim.dir, 1);
        fs::create_dir_all(newer.join(".cactup")).unwrap();
        fs::write(newer.join(".cactup/restart.toml"), ended.replace("job-id = \"4242\"", "job-id = \"4243\"")).unwrap();
        fs::remove_file(&log).unwrap();
        let queued = fake("echo '@JOB_ID@ PD'");
        let live = live_jobs(&sim, &Scheduler::new(&queued)).unwrap();
        assert_eq!(live.iter().map(|(id, job)| (*id, job.id.as_str())).collect::<Vec<_>>(), [(1, "4243")]);
        assert_eq!(fs::read_to_string(&log).unwrap().trim(), "4243", "an older ended restart's job was asked about");
    }

    /// The graceful path must confirm the run actually left before finishing
    /// the restart: a `TERMINATE` the parfile never reads would otherwise be
    /// reported as a stop while the job runs on (§8.6).
    #[test]
    fn graceful_stop_checks_whether_the_run_really_left() {
        use crate::mdb::meta::Meta;

        fn meta(get_status: &str) -> Meta {
            toml::from_str(&format!(
                r#"
                [machine]
                nickname = "fake"
                [scheduler]
                get-status = "{get_status}"
                status-pattern = "^@JOB_ID@ "
                running-pattern = " R"
                [queues.local]
                default = true
                [submitscripts]
                "default" = ["local"]
                [runscripts]
                "default" = ["local"]
                "#
            ))
            .unwrap()
        }

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        let entry = fake_sim(root, "queued", "200", true, None, false);
        let r = Restart::load(&entry.dir, 0).unwrap();

        // Still in the queue: the trigger has not been acted on.
        let live_meta = meta("echo '@JOB_ID@ R'");
        let live = Scheduler::new(&live_meta);
        assert!(run_is_live(&live, &r).unwrap());

        // Gone from the queue: the stop took effect, and the probe returns at
        // once rather than sitting out GRACEFUL_WAIT_SECS.
        let gone_meta = meta("true");
        let gone = Scheduler::new(&gone_meta);
        assert!(!run_is_live(&gone, &r).unwrap());
        let waited = std::time::Instant::now();
        assert!(await_run_exit(&gone, &r).unwrap());
        assert!(waited.elapsed() < std::time::Duration::from_secs(GRACEFUL_WAIT_SECS));

        // A foreground run has no job: its liveness marker decides instead.
        let fg = fake_sim(root, "foreground", NO_JOB_ID, true, None, false);
        let r = Restart::load(&fg.dir, 0).unwrap();
        assert!(!run_is_live(&gone, &r).unwrap(), "no marker → nothing running");
        let held = crate::lock::LinkLock::acquire(&r.running_lock_path()).unwrap();
        assert!(run_is_live(&gone, &r).unwrap(), "marker held by a live pid → running");
        drop(held);
    }

    #[test]
    fn batched_status_query_runs_one_command_per_pending_job() {
        use crate::mdb::meta::Meta;

        fn meta(scheduler_toml: &str) -> Meta {
            toml::from_str(&format!(
                r#"
                [machine]
                nickname = "fake"
                [scheduler]
                {scheduler_toml}
                [queues.local]
                default = true
                [submitscripts]
                "default" = ["local"]
                [runscripts]
                "default" = ["local"]
                "#
            ))
            .unwrap()
        }

        let tmp = tempfile::tempdir().unwrap();
        let calls = tmp.path().join("calls.txt");
        let m = meta(&format!(
            r#"
            get-status = "echo @JOB_ID@ >> {}"
            status-pattern = "^@JOB_ID@ "
            running-pattern = "^"
            "#,
            calls.display()
        ));
        let sched = Scheduler::new(&m);

        // "10" appears twice among the pending jobs.
        let pending: Vec<JobRef> = ["10", "20", "10", "30"].into_iter().map(JobRef::bare).collect();
        let statuses = sched.get_statuses(&pending);
        assert_eq!(statuses.len(), 3, "distinct pending ids resolved");

        let mut lines: Vec<String> =
            fs::read_to_string(&calls).unwrap().lines().map(str::to_owned).collect();
        assert_eq!(lines.len(), 3, "one invocation per distinct pending id");
        lines.sort();
        assert_eq!(lines, vec!["10", "20", "30"]);

        // No pending jobs at all: the command must never run.
        let untouched = tmp.path().join("never.txt");
        let m2 = meta(&format!(r#"get-status = "echo x >> {}""#, untouched.display()));
        Scheduler::new(&m2).get_statuses(&[]);
        assert!(!untouched.exists(), "empty pending list runs the command zero times");
    }
}
