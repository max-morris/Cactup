//! `sim stop` / `clean` (§8.6), `sim delete` (§8.7), `sim list` / `sim show`
//! (including `sim show --output-dir`), and `sim log`.

use crate::commands::Ctx;
use crate::installation::{Installation, SimEntry};
use crate::scheduler::{
    display_state, is_live, AfterEnd, DisplayState, JobRecord, JobRef, JobStatus, Scheduler, Statuses, Stopped, UnverifiedPid,
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
/// on a batch scheduler's machine has none (`RestartMeta::scheduler_job`),
/// and a pid recorded on another host is none this host can ask about
/// (§10), so the liveness marker answers instead (§9.3).
fn run_is_live(sched: &Scheduler, r: &Restart) -> Res<bool> {
    if let Some(job) = r.meta.scheduler_job(sched)
        && sched.recorded_elsewhere(job).is_none()
    {
        let status = sched.get_status_of(job, r.meta.terminated)?;
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
    stop_sim(&sim, &Scheduler::for_machine(&machine), name, force)
}

/// [`stop`], on `sim` (locked), by `sched`.
fn stop_sim(sim: &Simulation, sched: &Scheduler, name: &str, force: bool) -> Res<()> {
    // The active restart comes first while it has not recorded its end (at
    // a chain's handover the restart before it has ended, its job still
    // listed while it leaves the queue, and the active one is the run): the
    // graceful or forced stop below.
    if !active_runs(sim)? {
        return stop_ended(sim, sched, name);
    }

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
        if !await_run_exit(sched, &r)? {
            let what = if r.meta.scheduler_job(sched).is_none() {
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
        if r.meta.foreground && r.meta.scheduler_job(sched).is_none() {
            // A foreground run on a batch scheduler's machine: its pid is no
            // job, and only its lock says whether it runs.
            if crate::lock::LinkLock::is_held_live(&r.running_lock_path())? {
                println!(
                    "{} runs in the foreground (pid {}), which the scheduler knows nothing of: \
                     interrupt it where it runs (Ctrl-C). Nothing recorded.",
                    restart::dir_name(active),
                    r.meta.job_id
                );
                return Ok(());
            }
            println!(
                "{} ran in the foreground, and nothing of its run holds its lock: taken as ended",
                restart::dir_name(active)
            );
        } else if let Some(job) = r.meta.scheduler_job(sched) {
            match sched.stop(job)? {
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
    note_chained_after(sim, sched, active)
}

/// [`stop_sim`] with no active restart, or an active one that has recorded
/// its end (§8.6): the newest ended restart's job is stopped where the
/// scheduler still lists it (queued again, or leaving the queue) or could
/// not be asked (stopping a job that is gone costs nothing), with no
/// graceful path (no Cactus of that restart reads its TERMINATE any more: a
/// requeued job's own run recreates the file) and nothing else recorded on
/// the restart. Where the scheduler answered that the job is gone or over,
/// or nothing was asked, nothing is stopped, and only what is known is said.
/// An active restart that has ended is then deactivated, as a stop finishes
/// it. Restarts chained behind the job are named, never stopped unasked.
fn stop_ended(sim: &Simulation, sched: &Scheduler, name: &str) -> Res<()> {
    let active = restart::active_id(&sim.dir)?;
    let Some((r, answer)) = ended_newest(sim, sched)? else {
        println!("Simulation {} has no active restart; nothing to stop", name.bold());
        return Ok(());
    };
    let dir = restart::dir_name(r.id);
    let job_id = r.meta.job_id.clone();
    let stop_it = |why: &str| -> Res<()> {
        let job = r.meta.job();
        match sched.stop(job)? {
            Stopped::Sent => println!("{why}"),
            // Not on a batch scheduler, the only one asked after an end.
            Stopped::Gone | Stopped::Elsewhere(_) => println!("Job {job_id} had ended already; nothing to stop"),
        }
        sim.log("stop", &format!("asked the scheduler to stop job {job_id} ({dir}), after its end"));
        Ok(())
    };
    match answer {
        Some(AfterEnd::Listed(_)) => stop_it(&format!(
            "Asked the scheduler to stop job {}, still listed after {dir} ended (queued again, or \
             leaving the queue)",
            job_id.bold()
        ))?,
        Some(AfterEnd::Unanswered) => stop_it(&format!(
            "Could not ask the scheduler about job {}, of {dir}, which has ended; asked it to stop the \
             job in case it was queued again",
            job_id.bold()
        ))?,
        Some(AfterEnd::Over(JobStatus::Unknown)) => {
            println!("The scheduler does not know job {job_id} ({dir} has ended); nothing to stop")
        }
        Some(AfterEnd::Over(_)) => {
            println!("The scheduler lists job {job_id} as over ({dir} has ended); nothing to stop")
        }
        None if active == Some(r.id) => println!("{dir} has recorded its end; nothing to stop"),
        None => println!("Simulation {} has no active restart; nothing to stop", name.bold()),
    }
    // Nothing was stopped: an active restart that has ended is finished, as
    // a stop finishes it. Where a stop was sent, nothing else is recorded:
    // the restart's end stands, and a requeued job's run hands it over.
    let stopped = matches!(answer, Some(AfterEnd::Listed(_) | AfterEnd::Unanswered));
    if let Some(active) = active
        && !stopped
    {
        restart::deactivate(&sim.dir)?;
        sim.log("stop", &format!("{} had recorded its end; deactivated", restart::dir_name(active)));
    }
    note_chained_after(sim, sched, r.id)
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
    let sched = Scheduler::for_machine(&machine);

    // The lock comes BEFORE the live-job guard: submit holds it for the whole
    // submission (§2.3 item 3), so taking it first closes the window where a
    // concurrent submit queues a job between our scan and the removal.
    let _lock = sim.lock()?;

    // Live-job guard (§8.7).
    let live = live_jobs(&sim, &sched)?;
    // An older record's pid (§10) is never stopped, `-f` neither: say what
    // works, not the advice below.
    if let Some(old) = live.iter().find(|l| l.unverified) {
        bail!(
            "restart {} of simulation \"{name}\": {}; then delete the simulation",
            restart::dir_name(old.id),
            UnverifiedPid::of(&old.job.id)
        );
    }
    // Nor a pid sharing its process group (§10): `sim stop` and `-f` would
    // refuse it too.
    if let Some(shared) = live.iter().find(|l| l.shared_group) {
        bail!(
            "restart {} of simulation \"{name}\": {}; then delete the simulation",
            restart::dir_name(shared.id),
            crate::scheduler::SharedGroup(shared.job.id.clone())
        );
    }
    if !live.is_empty() {
        if !force {
            let stops = match active_runs(&sim)? {
                true => restart::active_id(&sim.dir)?,
                false => live.iter().find(|l| l.ended).map(|l| l.id),
            };
            bail!("{}", live_jobs_refusal(name, &live, stops));
        }
        for l in &live {
            // A pid cactup cannot vouch for (§10) says itself that nothing ran.
            sched.stop(l.job.job()).map_err(|e| match e.downcast_ref::<UnverifiedPid>() {
                Some(_) => e,
                None => e.context(format!("stopping job {}", l.job.id)),
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

/// Whether `sim` has an active restart that has not recorded its end: the
/// one `sim stop` stops (gracefully or forced), before anything else.
fn active_runs(sim: &Simulation) -> Res<bool> {
    Ok(match restart::active_id(&sim.dir)? {
        Some(id) => Restart::load(&sim.dir, id).is_ok_and(|r| !r.meta.terminated),
        None => false,
    })
}

/// The newest of `sim`'s restarts whose run recorded its end: the one a
/// requeue concerns (restarts after it, chained behind its job, have not
/// run). Read from the newest back, so usually one restart is read.
fn newest_ended(sim: &Simulation, ids: &[u32]) -> Option<u32> {
    ids.iter()
        .rev()
        .copied()
        .find(|&id| Restart::load(&sim.dir, id).is_ok_and(|r| r.meta.terminated))
}

/// Whether `r`, whose run recorded its end, is still asked about (§8.6,
/// §10): only the simulation's newest ended restart (`newest`, from
/// [`newest_ended`]), the one a requeue concerns, only for a simulation of
/// this machine, and only within the scheduler's window after its end
/// (`Scheduler::asks_after_end`: a batch scheduler, and 14 days). An older
/// restart's job id may by now name an unrelated job (ids wrap around, a
/// cluster is reinstalled, two clusters share a home), which would refuse a
/// delete or be stopped by its `-f`.
fn asks_after_end(
    sched: &Scheduler,
    sim: &Simulation,
    id: u32,
    finished: Option<chrono::DateTime<chrono::Utc>>,
    newest: Option<u32>,
) -> bool {
    newest == Some(id) && sched.asks_after_end(&sim.meta.machine, finished)
}

/// What the scheduler says of ended restart `r`'s job (§8.6, §10): `None`
/// where it is not asked (no job a scheduler knows, a pids machine, past
/// the requeue window, a simulation of another machine).
pub(crate) fn ask_ended(sim: &Simulation, sched: &Scheduler, r: &Restart) -> Res<Option<AfterEnd>> {
    let Some(job) = r.meta.scheduler_job(sched) else { return Ok(None) };
    if !sched.asks_after_end(&sim.meta.machine, r.meta.finished) {
        return Ok(None);
    }
    sched.ask_after_end(job).map(Some)
}

/// The newest ended restart of `sim` ([`newest_ended`]) and what the
/// scheduler says of its job ([`ask_ended`]). Where it is still listed, or
/// the query failed, `sim stop` stops it, `sim submit` chains behind it, and
/// `sim run` refuses to start beside it (the safe reading for each, §8.6).
pub(crate) fn ended_newest(sim: &Simulation, sched: &Scheduler) -> Res<Option<(Restart, Option<AfterEnd>)>> {
    let Some(id) = newest_ended(sim, &restart::list_ids(&sim.dir)?) else { return Ok(None) };
    let Ok(r) = Restart::load(&sim.dir, id) else { return Ok(None) };
    let answer = ask_ended(sim, sched, &r)?;
    Ok(Some((r, answer)))
}

/// Say which restarts after `id` were submitted chained (§8.3.2) and have
/// not run, each with the job it waits for (it runs after that job) and
/// the scheduler's command that stops it: `sim stop` never stops them
/// unasked.
fn note_chained_after(sim: &Simulation, sched: &Scheduler, id: u32) -> Res<()> {
    for later in restart::list_ids(&sim.dir)?.into_iter().filter(|&later| later > id) {
        let Ok(r) = Restart::load(&sim.dir, later) else { continue };
        let Some(after) = r.meta.chained_job_id.as_deref() else { continue };
        if r.meta.terminated || r.meta.job_id == NO_JOB_ID {
            continue;
        }
        let how = match sched.stop_command(&r.meta.job_id) {
            Some(cmd) => format!("`{cmd}`"),
            None => "the scheduler's own stop command".to_owned(),
        };
        // Not stopped unasked: it is the simulation's next run. Stopping it
        // is the scheduler's command; `sim stop` acts on the active (or
        // newest ended) restart only.
        println!(
            "Restart {} (job {}) is chained behind job {after} and runs after it; to stop it: {how}",
            restart::dir_name(later),
            r.meta.job_id
        );
    }
    Ok(())
}

/// A restart whose job is live, for `sim delete` (§8.7).
#[derive(Debug)]
struct LiveJob {
    id: u32,
    job: JobRecord,
    /// Its run recorded its end: a job the scheduler still lists (queued
    /// again, or leaving the queue).
    ended: bool,
    /// An older record's pid with a process here (§10): never stopped.
    unverified: bool,
    /// A pid that does not lead its own process group (§10): never stopped.
    shared_group: bool,
}

/// The restarts of `sim` whose jobs are live (§8.7), asked about in one
/// batch (`get_statuses`: the machine's listing, else the bounded pool of
/// per-job queries) under a progress line. Of the restarts whose run
/// recorded its end only the newest is asked about, within the requeue
/// window ([`asks_after_end`]), and is live then while the scheduler lists
/// it in any state (`is_live`: queued again, or leaving the queue). A job whose query failed is asked once more on its own:
/// before its run's end that is an error, and `delete` refuses with it;
/// after, it is taken as ended. Whatever `-f` says, `delete` is refused
/// while a restart runs where this host can neither ask about nor stop it:
/// a job recorded on another host (pids), or a foreground run on a batch
/// scheduler's machine, while the restart's `running.lock` is live.
fn live_jobs(sim: &Simulation, sched: &Scheduler) -> Res<Vec<LiveJob>> {
    let mut asked: Vec<Restart> = Vec::new();
    let ids = restart::list_ids(&sim.dir)?;
    let newest = newest_ended(sim, &ids);
    for id in ids {
        let Ok(r) = Restart::load(&sim.dir, id) else { continue };
        let held = || crate::lock::LinkLock::is_held_live(&r.running_lock_path());
        let Some(job) = r.meta.scheduler_job(sched) else {
            if r.meta.foreground && held()? {
                bail!(
                    "restart {} of simulation \"{}\" runs in the foreground (pid {}), which the \
                     scheduler knows nothing of: interrupt it where it runs (Ctrl-C), then delete \
                     the simulation (a lock left by a run that ended expires within {} minutes)",
                    restart::dir_name(id),
                    sim.name,
                    r.meta.job_id,
                    crate::lock::LOCK_STALE_SECS / 60
                );
            }
            continue;
        };
        if r.meta.terminated && !asks_after_end(sched, sim, id, r.meta.finished, newest) {
            continue;
        }
        if let Some(host) = sched.recorded_elsewhere(job) {
            if held()? {
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
        asked.push(r);
    }
    let jobs: Vec<JobRef> = asked.iter().map(|r| r.meta.job()).collect();
    let statuses = sched.get_statuses_shown(&jobs)?;

    let mut live = Vec::new();
    for r in &asked {
        let ended = r.meta.terminated;
        let status = match statuses.get(r.meta.job()) {
            Some(status) => *status,
            None => sched.get_status_of(r.meta.job(), ended)?,
        };
        if is_live(status, ended) {
            live.push(LiveJob {
                id: r.id,
                job: r.meta.job().to_record(),
                ended,
                unverified: sched.unverified(r.meta.job()),
                shared_group: sched.shares_group(r.meta.job()),
            });
        }
    }
    Ok(live)
}

/// `delete`'s refusal of `live` without `-f`, with advice that works:
/// `sim stop` stops one job, restart `stops`'s (see [`stop_sim`]: the
/// active restart's while it has not recorded its end, else the newest
/// ended restart's still-listed job, which waiting until it leaves the
/// queue also clears); any other (a pre-submitted chain's, an ended one
/// behind a running restart) only `-f` stops.
fn live_jobs_refusal(name: &str, live: &[LiveJob], stops: Option<u32>) -> String {
    let stoppable = live.iter().find(|l| Some(l.id) == stops);
    let advice = match stoppable {
        Some(_) if live.len() == 1 => format!("`cactup sim stop {name}` first, or use -f to stop it and delete"),
        Some(l) => format!(
            "`cactup sim stop {name}` stops {}'s job; use -f to stop them all and delete",
            restart::dir_name(l.id)
        ),
        None => "use -f to stop them and delete".to_owned(),
    };
    let jobs = live
        .iter()
        .map(|l| match l.ended {
            true => format!(
                "{} = job {}, still listed by the scheduler after the restart ended (leaving the \
                 queue, or starting again)",
                restart::dir_name(l.id),
                l.job.id
            ),
            false => format!("{} = job {}", restart::dir_name(l.id), l.job.id),
        })
        .collect::<Vec<_>>()
        .join("; ");
    let what = if live.len() == 1 { "a live job" } else { "live jobs" };
    format!("simulation \"{name}\" has {what} ({jobs}); {advice}")
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
    /// The subject is a foreground run (its pid is no batch job).
    foreground: bool,
    active: bool,
    chained: bool,
    terminated: bool,
    /// When the subject's run recorded its end.
    finished: Option<chrono::DateTime<chrono::Utc>>,
    /// The subject is the simulation's newest ended restart ([`newest_ended`]).
    newest: bool,
    /// The machine the simulation was made for.
    machine: String,
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
            foreground: false,
            active: false,
            chained: false,
            terminated: false,
            finished: None,
            newest: false,
            machine: String::new(),
            forced: Some(DisplayState::Inactive),
        }
    }

    /// The job whose live status still has to be queried, if any. Before
    /// its run's end, only an active restart — or a non-active one holding a
    /// pre-submitted chain (§8.6) — is sensitive to it; after the end, only
    /// the newest restart within the requeue window ([`asks_after_end`]),
    /// active or not (its job queued again). Anything else is INACTIVE or
    /// FINISHED whatever the queue says, so a finished history costs no
    /// scheduler calls once its window has passed. A foreground run on a
    /// batch scheduler's machine has no job to ask about
    /// (`RestartMeta::scheduler_job`).
    fn job_to_query(&self, sched: &Scheduler) -> Option<JobRef<'_>> {
        let job = self.job.as_ref().filter(|_| self.forced.is_none())?;
        if self.foreground && sched.job_ids() == crate::mdb::meta::JobIds::Scheduler {
            return None;
        }
        let asked = match self.terminated {
            true => self.newest && sched.asks_after_end(&self.machine, self.finished),
            false => self.active || self.chained,
        };
        asked.then(|| job.job())
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
/// state the query did not give. After the end, a failed query is taken as
/// the job ended; a job the scheduler still lists is shown for what is
/// known of it, whether the restart is still active or not (`sim stop`
/// stops it): QUEUED AGAIN where queued or held (`Requeued`), STILL LISTED
/// where running (`StillListed`: leaving the queue, or starting again). A
/// state no pattern reads after the end is the ended job (`is_live`).
fn shown(active: bool, chained: bool, terminated: bool, query: Option<Option<JobStatus>>) -> DisplayState {
    let status = match query {
        Some(None) if !terminated => return DisplayState::Unknown,
        Some(Some(JobStatus::Queued | JobStatus::Holding)) if terminated => return DisplayState::Requeued,
        Some(Some(JobStatus::Running)) if terminated => return DisplayState::StillListed,
        // A state no pattern reads after the end: the ended job.
        Some(Some(JobStatus::Error)) if terminated => Some(JobStatus::Unknown),
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
        foreground: r.meta.foreground,
        active: active.is_some(),
        chained: r.meta.chained_job_id.is_some(),
        terminated: r.meta.terminated,
        finished: r.meta.finished,
        newest: newest_ended(sim, &scan.ids) == Some(subject),
        machine: sim.meta.machine.clone(),
        forced: None,
    }
}

/// [`sim_state`] for a single simulation: scan and query it on the spot.
#[cfg(test)]
fn sim_state_now(
    sim: &Simulation,
    sched: &Scheduler,
) -> (DisplayState, Option<u32>, Option<String>) {
    let Ok(scan) = restart::scan(&sim.dir) else {
        return (DisplayState::Inactive, None, None);
    };
    let st = sim_state(sim, &scan);
    let query = st.job_to_query(sched).map(|job| sched.get_status(job).ok());
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
        DisplayState::Requeued => "QUEUED AGAIN".yellow(),
        DisplayState::StillListed => "STILL LISTED".yellow(),
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
    let sched = Scheduler::for_machine(&machine);
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
    fn job_to_query(&self, sched: &Scheduler) -> Option<JobRef<'_>> {
        match self {
            Row::Listed { state, .. } => state.job_to_query(sched),
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

fn print_row(name: &str, row: &Row, statuses: &Statuses, sched: &Scheduler, long: bool) {
    match row {
        Row::Missing(dir) => println!(
            "  {:24} {:12} {} (prune with `cactup sim delete {name}`)",
            name.bold(),
            "MISSING".red(),
            dir.display()
        ),
        Row::Broken(e) => println!("  {:24} {:12} {e}", name.bold(), "BROKEN".red()),
        Row::Listed { config, restarts, dir, state } => {
            let query = state.job_to_query(sched).map(|job| statuses.get(job).copied());
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
    let sched = Scheduler::for_machine(&machine);

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
    let pending: Vec<JobRef> = rows.iter().filter_map(|row| row.job_to_query(&sched)).collect();
    let statuses = sched.get_statuses_shown(&pending)?;

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
            print_row(name, row, &statuses, &sched, long);
        }
    }
    Ok(())
}

fn show_one(inst: &Installation, sched: &Scheduler, name: &str, long: bool) -> Res<()> {
    let sim = Simulation::locate(inst, name)?;
    let scan = restart::scan(&sim.dir)?;
    let active = scan.active_id(&sim.dir)?;
    // Load the whole chain first, so every restart's live status comes out of
    // one batched query instead of a scheduler round-trip each (§10), and the
    // header's state comes out of the same answer as its restart's row. Of
    // the restarts whose run recorded its end, only the newest is asked
    // about, within the requeue window (`asks_after_end`); a foreground run
    // on a batch scheduler's machine has no job to ask about.
    let loaded: Vec<Res<Restart>> =
        scan.ids.iter().map(|&id| Restart::load(&sim.dir, id)).collect();
    let newest = newest_ended(&sim, &scan.ids);
    let asked = |r: &Restart| {
        r.meta.scheduler_job(sched).is_some()
            && (!r.meta.terminated || asks_after_end(sched, &sim, r.id, r.meta.finished, newest))
    };
    let queries: Vec<JobRef> = loaded
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .filter(|r| asked(r))
        .map(|r| r.meta.job())
        .collect();
    let statuses = sched.get_statuses_shown(&queries)?;
    let st = sim_state(&sim, &scan);
    let state = st.state(st.job_to_query(sched).map(|job| statuses.get(job).copied()));

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

    if scan.ids.is_empty() {
        println!("  restarts:      none");
        return Ok(());
    }
    println!("  restarts:");

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
                if state == DisplayState::StillListed {
                    // What the scheduler's answer said (a state its running
                    // pattern reads; its own word for it is not kept), and
                    // what it may mean.
                    line.push_str(", listed running after the restart ended: leaving the queue, or starting again");
                }
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
            toml_text.push_str(&format!("terminated = true\nfinished = \"{}\"\n", chrono::Utc::now().to_rfc3339()));
        }
        fs::write(rdir.join(".cactup").join("restart.toml"), toml_text).unwrap();

        if active {
            restart::make_active(&dir, 0).unwrap();
        }

        SimEntry { dir, config: "sim".to_owned(), created: chrono::Utc::now() }
    }

    /// The newest ended restart's job, where the scheduler still lists it.
    fn listed_newest(sim: &Simulation, sched: &Scheduler) -> Res<Option<(u32, JobRecord, ())>> {
        Ok(ended_newest(sim, sched)?
            .and_then(|(r, answer)| matches!(answer, Some(AfterEnd::Listed(_))).then(|| (r.id, r.meta.job().to_record(), ()))))
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
        // Ended more than the requeue window ago: no query. (Within it, the
        // newest restart is asked about: `a_requeued_job_of_an_inactive_restart_is_named_and_shown_so`.)
        let long_ago = (chrono::Utc::now() - chrono::Duration::days(30)).to_rfc3339();
        for entry in [&hist1, &hist2] {
            let path = restart::restart_dir(&entry.dir, 0).join(".cactup/restart.toml");
            let text = fs::read_to_string(&path).unwrap();
            let text = text
                .lines()
                .map(|l| if l.starts_with("finished") { format!("finished = \"{long_ago}\"") } else { l.to_owned() })
                .collect::<Vec<_>>()
                .join("\n");
            fs::write(&path, text + "\n").unwrap();
        }
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

        let batch: crate::mdb::meta::Meta = toml::from_str(
            "[machine]\nnickname = \"fake\"\n[scheduler]\n[queues.local]\ndefault = true\n\
             [submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
        )
        .unwrap();
        let batch = Scheduler::new(&batch);
        let pending: Vec<&str> = rows.iter().filter_map(|row| row.job_to_query(&batch)).map(|job| job.id).collect();
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
        assert_eq!(live.iter().map(|l| (l.id, l.job.id.as_str())).collect::<Vec<_>>(), [(0, "4242")]);
        let (state, _, _) = sim_state_now(&sim, &batch);
        assert_eq!(state, DisplayState::Requeued);

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

        // Ended, and the scheduler still lists the job in a state its
        // patterns read: leaving the queue (`CG`, `TO`) or starting again
        // (`PD`, `CF`, `R`), live either way, shown QUEUED AGAIN or STILL
        // LISTED; in a state no pattern reads (`CD`), the ended job.
        let text = fs::read_to_string(&path).unwrap().replace("terminated = false", "terminated = true");
        fs::write(&path, text).unwrap();
        for (state_letter, live, shown) in [
            ("CF", true, DisplayState::StillListed),
            ("R", true, DisplayState::StillListed),
            ("CG", true, DisplayState::StillListed),
            ("TO", true, DisplayState::StillListed),
            ("CD", false, DisplayState::Finished),
            ("PD", true, DisplayState::Requeued),
        ] {
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
        assert_eq!(live.iter().map(|l| l.job.id.as_str()).collect::<Vec<_>>(), ["4242"]);

        let failing = fake(timeout);
        let failing = Scheduler::new(&failing);
        let err = format!("{:#}", live_jobs(&sim, &failing).unwrap_err());
        assert!(err.contains("the status query for job 4242"), "{err}");
        assert_eq!(sim_state_now(&sim, &failing).0, DisplayState::Unknown);
        let path = restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml");
        let ended = fs::read_to_string(&path).unwrap()
            + &format!("terminated = true\nfinished = \"{}\"\n", chrono::Utc::now().to_rfc3339());
        fs::write(&path, &ended).unwrap();
        assert!(live_jobs(&sim, &failing).unwrap().is_empty());

        // A newer restart: the older terminated one is no longer asked about.
        let newer = restart::restart_dir(&sim.dir, 1);
        fs::create_dir_all(newer.join(".cactup")).unwrap();
        fs::write(newer.join(".cactup/restart.toml"), ended.replace("job-id = \"4242\"", "job-id = \"4243\"")).unwrap();
        fs::remove_file(&log).unwrap();
        let queued = fake("echo '@JOB_ID@ PD'");
        let live = live_jobs(&sim, &Scheduler::new(&queued)).unwrap();
        assert_eq!(live.iter().map(|l| (l.id, l.job.id.as_str())).collect::<Vec<_>>(), [(1, "4243")]);
        assert_eq!(fs::read_to_string(&log).unwrap().trim(), "4243", "an older ended restart's job was asked about");
    }

    /// The newest restart, ended and no longer active (`sim clean`, a
    /// stop), whose job the scheduler queued again: `delete`'s refusal
    /// names only what works (`-f`; `sim stop` stops the active restart's
    /// job only), and `sim show`'s row says the job is queued again rather
    /// than plain INACTIVE. Past the requeue window, the job is not asked
    /// about at all.
    #[test]
    fn a_requeued_job_of_an_inactive_restart_is_named_and_shown_so() {
        use crate::mdb::meta::Meta;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let entry = fake_sim(tmp.path(), "requeued", "1003", false, None, true);
        let sim = Simulation::open("requeued", &entry.dir).unwrap();
        let m: Meta = toml::from_str(&format!(
            "[machine]\nnickname = \"fake\"\n[scheduler]\nget-status = \"echo @JOB_ID@ >> {}; echo '@JOB_ID@ PD'\"\n\
             status-pattern = \"^@JOB_ID@ \"\nqueued-pattern = \" PD\"\n[queues.local]\ndefault = true\n\
             [submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
            log.display()
        ))
        .unwrap();
        let sched = Scheduler::new(&m);
        let live = live_jobs(&sim, &sched).unwrap();
        assert_eq!(live.iter().map(|l| (l.id, l.ended)).collect::<Vec<_>>(), [(0, true)]);
        let refusal = live_jobs_refusal("requeued", &live, Some(0));
        assert!(refusal.contains("job 1003, still listed by the scheduler after the restart ended"), "{refusal}");
        assert!(refusal.ends_with("`cactup sim stop requeued` first, or use -f to stop it and delete"), "{refusal}");
        assert_eq!(shown(false, false, true, Some(Some(JobStatus::Queued))), DisplayState::Requeued);
        // `sim list` asks about it and says the same.
        let name = "requeued".to_owned();
        let entries = [(&name, &entry)];
        let rows = gather_rows(&entries);
        let job = rows[0].job_to_query(&sched).expect("the newest restart's requeued job is asked about");
        let statuses = sched.get_statuses(&[job]);
        assert_eq!(listed_state(&rows[0]).state(Some(statuses.get(job).copied())), DisplayState::Requeued);
        assert_eq!(shown(true, false, true, Some(Some(JobStatus::Queued))), DisplayState::Requeued);

        // Fifteen days after its end: not asked about, taken as ended.
        let path = restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml");
        let text = fs::read_to_string(&path).unwrap();
        let old = (chrono::Utc::now() - chrono::Duration::days(15)).to_rfc3339();
        let text = text.lines().map(|l| if l.starts_with("finished") { format!("finished = \"{old}\"") } else { l.to_owned() }).collect::<Vec<_>>().join("\n") + "\n";
        fs::write(&path, text).unwrap();
        fs::remove_file(&log).unwrap();
        assert!(live_jobs(&sim, &sched).unwrap().is_empty());
        assert!(!log.exists(), "a job asked about past the requeue window");
    }

    /// A foreground `sim run` records its own pid as the job id: on a batch
    /// scheduler's machine that pid is never asked about or stopped (its
    /// `running.lock` alone says whether it runs, and `delete` refuses while
    /// it is live, `-f` too); on a `job-ids = "pids"` machine it is asked of
    /// `ps` like a submitted job. A record without the key is a submitted
    /// restart, as older ones are.
    #[test]
    fn a_foreground_runs_pid_never_goes_to_a_batch_scheduler() {
        use crate::mdb::meta::Meta;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let entry = fake_sim(tmp.path(), "fg", &std::process::id().to_string(), true, None, false);
        let sim = Simulation::open("fg", &entry.dir).unwrap();
        let path = restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml");
        let fake = |job_ids: &str| -> Meta {
            toml::from_str(&format!(
                "[machine]\nnickname = \"fake\"\n[scheduler]\njob-ids = \"{job_ids}\"\n\
                 get-status = \"echo @JOB_ID@ >> {}; echo '@JOB_ID@ R'\"\nstatus-pattern = \"^@JOB_ID@ \"\n\
                 running-pattern = \" R\"\n[queues.local]\ndefault = true\n[submitscripts]\n\"default\" = [\"local\"]\n\
                 [runscripts]\n\"default\" = [\"local\"]\n",
                log.display()
            ))
            .unwrap()
        };
        let batch = fake("scheduler");
        let batch = Scheduler::new(&batch);
        // Without the key: a submitted restart, asked about.
        assert!(Restart::load(&sim.dir, 0).unwrap().meta.scheduler_job(&batch).is_some());

        let text = fs::read_to_string(&path).unwrap() + "foreground = true\n";
        fs::write(&path, text).unwrap();
        let r = Restart::load(&sim.dir, 0).unwrap();
        assert!(r.meta.foreground && r.meta.scheduler_job(&batch).is_none());
        let held = crate::lock::LinkLock::acquire(&r.running_lock_path()).unwrap();
        let err = live_jobs(&sim, &batch).unwrap_err().to_string();
        assert!(err.contains("runs in the foreground") && err.contains("interrupt it where it runs"), "{err}");
        assert!(run_is_live(&batch, &r).unwrap(), "its lock says it runs");
        assert_eq!(sim_state_now(&sim, &batch).0, DisplayState::Active);
        drop(held);
        assert!(live_jobs(&sim, &batch).unwrap().is_empty());
        assert!(!run_is_live(&batch, &r).unwrap());
        assert!(!log.exists(), "a foreground run's pid went to a batch scheduler");

        let pids = fake("pids");
        assert!(r.meta.scheduler_job(&Scheduler::new(&pids)).is_some(), "ps is asked about it where job ids are pids");
    }

    /// The newest restart has recorded its end and its job is still listed,
    /// in either shape (`CF`, `R`: starting again before its run clears the
    /// end; `CG`, `TO`: leaving the queue; `PD`: queued again), the restart
    /// still active with its `TERMINATE` file (as after any run), or
    /// deactivated (`sim clean`): `sim stop`, with or without `-f`, asks the
    /// scheduler to stop that job directly (no Cactus reads the trigger) and
    /// records nothing on the restart; `sim run` refuses to start beside it,
    /// `sim submit` chains behind it (`listed_newest`), and the reaper
    /// never reaps it. A restart that has not recorded its end keeps the
    /// graceful path.
    #[test]
    fn sim_stop_stops_the_newest_restarts_still_listed_job_directly() {
        use crate::mdb::meta::Meta;
        let tmp = tempfile::tempdir().unwrap();
        let stopped = tmp.path().join("stopped");
        let gone = tmp.path().join("gone");
        let letter = tmp.path().join("letter");
        let m: Meta = toml::from_str(&format!(
            "[machine]\nnickname = \"fake\"\n[scheduler]\n\
             get-status = \"test -e {gone} || echo \\\"@JOB_ID@ $(cat {letter}) \\\"\"\n\
             stop = \"echo @JOB_ID@ >> {stopped}; touch {gone}\"\n\
             status-pattern = \"^@JOB_ID@ \"\nqueued-pattern = \" PD \"\nrunning-pattern = \" (CF|CG|R|TO) \"\n\
             [queues.local]\ndefault = true\n\
             [submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
            gone = gone.display(),
            stopped = stopped.display(),
            letter = letter.display()
        ))
        .unwrap();
        let sched = Scheduler::new(&m);
        let shapes = ["CF", "R", "CG", "TO", "PD"];
        for (name, active) in shapes.iter().flat_map(|l| [(format!("active-{l}"), true), (format!("cleaned-{l}"), false)]).map(|(n, a)| (n, a)) {
            let name = name.as_str();
            fs::write(&letter, name.split('-').nth(1).unwrap()).unwrap();
            let _ = fs::remove_file(&gone);
            let entry = fake_sim(tmp.path(), name, "1003", active, None, true);
            let sim = Simulation::open(name, &entry.dir).unwrap();
            let terminate = restart::restart_dir(&sim.dir, 0).join("TERMINATE");
            fs::write(&terminate, b"0\n").unwrap();
            let before = fs::read_to_string(restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml")).unwrap();
            let refused = crate::sim::start::refuse_beside_listed_for_tests(&sim, &sched).unwrap_err().to_string();
            assert!(refused.contains("still lists its job 1003") && refused.contains("sim stop"), "{name}: {refused}");
            assert_eq!(listed_newest(&sim, &sched).unwrap().map(|(id, job, _)| (id, job.id)), Some((0, "1003".to_owned())));
            stop_sim(&sim, &sched, name, false).unwrap();
            assert_eq!(fs::read_to_string(&terminate).unwrap(), "0\n", "{name}: took the graceful path");
            let after = fs::read_to_string(restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml")).unwrap();
            assert_eq!(before, after, "{name}: recorded something on the restart");
            assert_eq!(restart::active_id(&sim.dir).unwrap().is_some(), active, "{name}");
            assert!(crate::sim::start::refuse_beside_listed_for_tests(&sim, &sched).is_ok());
        }
        assert_eq!(fs::read_to_string(&stopped).unwrap().lines().count(), 2 * shapes.len());

        // Not ended: the graceful path, as before (the queue no longer
        // lists the job, so the run has left).
        fs::remove_file(&stopped).unwrap();
        let entry = fake_sim(tmp.path(), "running", "1004", true, None, false);
        let sim = Simulation::open("running", &entry.dir).unwrap();
        let terminate = restart::restart_dir(&sim.dir, 0).join("TERMINATE");
        fs::write(&terminate, b"0\n").unwrap();
        stop_sim(&sim, &sched, "running", false).unwrap();
        assert_eq!(fs::read_to_string(&terminate).unwrap(), "1\n");
        assert!(!stopped.exists(), "the graceful path ran the stop command");
    }

    /// `sim submit` chains a new restart behind an ended restart's job the
    /// scheduler still lists: the newer restart has not run, so the ended
    /// one is still the one asked about (`newest_ended`), and `sim stop` and
    /// `sim delete` still see its job.
    #[test]
    fn a_chained_restart_leaves_the_ended_one_asked_about() {
        use crate::mdb::meta::Meta;
        let tmp = tempfile::tempdir().unwrap();
        let entry = fake_sim(tmp.path(), "chained", "4260", true, None, true);
        let sim = Simulation::open("chained", &entry.dir).unwrap();
        let first = fs::read_to_string(restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml")).unwrap();
        let newer = restart::restart_dir(&sim.dir, 1);
        fs::create_dir_all(newer.join(".cactup")).unwrap();
        let second = first
            .lines()
            .filter(|l| !l.starts_with("terminated") && !l.starts_with("finished"))
            .map(|l| if l.starts_with("job-id") { "job-id = \"4261\"\nchained-job-id = \"4260\"".to_owned() } else { l.to_owned() })
            .collect::<Vec<_>>()
            .join("\n");
        fs::write(newer.join(".cactup/restart.toml"), second + "\n").unwrap();
        let m: Meta = toml::from_str(
            "[machine]\nnickname = \"fake\"\n[scheduler]\nget-status = \"echo '@JOB_ID@ CF '\"\n\
             status-pattern = \"^@JOB_ID@ \"\nrunning-pattern = \" (CF|CG|R|TO) \"\n[queues.local]\ndefault = true\n\
             [submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
        )
        .unwrap();
        let sched = Scheduler::new(&m);
        assert_eq!(newest_ended(&sim, &[0, 1]), Some(0));
        assert_eq!(listed_newest(&sim, &sched).unwrap().map(|(id, job, _)| (id, job.id)), Some((0, "4260".to_owned())));
        let live = live_jobs(&sim, &sched).unwrap();
        assert_eq!(live.iter().map(|l| (l.id, l.ended)).collect::<Vec<_>>(), [(0, true), (1, false)]);
    }

    /// At a chain's handover restart 0 has ended, its job still listed
    /// while it leaves the queue (`TO`) or done (`CD`), and restart 1,
    /// chained behind it, is active and running: `sim stop` stops restart
    /// 1 (gracefully, or forced with `-f`), never restart 0's job, and
    /// `sim delete`'s advice names what `sim stop` stops.
    #[test]
    fn sim_stop_at_a_chains_handover_stops_the_running_restart() {
        use crate::mdb::meta::Meta;
        for zero in ["TO", "CD"] {
            let tmp = tempfile::tempdir().unwrap();
            let stopped = tmp.path().join("stopped");
            let entry = fake_sim(tmp.path(), "chain", "1000", false, None, true);
            let sim = Simulation::open("chain", &entry.dir).unwrap();
            let first = fs::read_to_string(restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml")).unwrap();
            let second = first
                .lines()
                .filter(|l| !l.starts_with("terminated") && !l.starts_with("finished"))
                .map(|l| if l.starts_with("job-id") { "job-id = \"1001\"\nchained-job-id = \"1000\"".to_owned() } else { l.to_owned() })
                .collect::<Vec<_>>()
                .join("\n");
            let one = restart::restart_dir(&sim.dir, 1);
            fs::create_dir_all(one.join(".cactup")).unwrap();
            fs::write(one.join(".cactup/restart.toml"), second + "\n").unwrap();
            restart::make_active(&sim.dir, 1).unwrap();
            let terminate = one.join("TERMINATE");
            let m: Meta = toml::from_str(&format!(
                "[machine]\nnickname = \"fake\"\n[scheduler]\n\
                 get-status = \"case @JOB_ID@ in 1000) echo '1000 {zero} ';; 1001) grep -q 1 {t} 2>/dev/null || echo '1001 R ';; esac\"\n\
                 stop = \"echo @JOB_ID@ >> {s}\"\nstatus-pattern = \"^@JOB_ID@ \"\nrunning-pattern = \" (CF|CG|R|TO) \"\n\
                 [queues.local]\ndefault = true\n[submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
                t = terminate.display(),
                s = stopped.display()
            ))
            .unwrap();
            let sched = Scheduler::new(&m);

            let live = live_jobs(&sim, &sched).unwrap();
            let stops = if active_runs(&sim).unwrap() { restart::active_id(&sim.dir).unwrap() } else { None };
            let refusal = live_jobs_refusal("chain", &live, stops);
            assert!(refusal.contains("output-0001 = job 1001"), "{zero}: {refusal}");
            match zero {
                "TO" => assert!(refusal.contains("`cactup sim stop chain` stops output-0001's job"), "{refusal}"),
                _ => assert!(refusal.ends_with("`cactup sim stop chain` first, or use -f to stop it and delete"), "{refusal}"),
            }

            // Forced: restart 1's job, not restart 0's.
            stop_sim(&sim, &sched, "chain", true).unwrap();
            assert_eq!(fs::read_to_string(&stopped).unwrap(), "1001\n", "{zero}");
            assert!(restart::active_id(&sim.dir).unwrap().is_none());

            // Graceful: restart 1 active again, its TERMINATE file there.
            fs::remove_file(&stopped).unwrap();
            fs::write(one.join(".cactup/restart.toml"), fs::read_to_string(one.join(".cactup/restart.toml")).unwrap().replace("terminated = true", "terminated = false")).unwrap();
            restart::make_active(&sim.dir, 1).unwrap();
            fs::write(&terminate, b"0\n").unwrap();
            stop_sim(&sim, &sched, "chain", false).unwrap();
            assert_eq!(fs::read_to_string(&terminate).unwrap(), "1\n", "{zero}: no graceful request");
            assert!(!stopped.exists(), "{zero}: restart 0's job stopped");
        }
    }

    /// A completed job a scheduler lists in a state no pattern reads after
    /// the restart's end (Torque's `C` within keep_completed, whose `qdel`
    /// refuses it): the ended job. `sim delete` goes ahead, and `sim stop`
    /// runs no stop command for it.
    #[test]
    fn a_completed_job_listed_after_the_end_is_the_ended_job() {
        use crate::mdb::meta::Meta;
        let tmp = tempfile::tempdir().unwrap();
        let stopped = tmp.path().join("stopped");
        let entry = fake_sim(tmp.path(), "torque", "4242.qb", true, None, true);
        let sim = Simulation::open("torque", &entry.dir).unwrap();
        let m: Meta = toml::from_str(&format!(
            "[machine]\nnickname = \"fake\"\n[scheduler]\nget-status = \"echo '@JOB_ID@ job x C batch'\"\n\
             stop = \"echo @JOB_ID@ >> {}; echo 'qdel: Invalid state for job - COMPLETE' >&2; exit 15\"\n\
             status-pattern = \"^@JOB_ID@ \"\nqueued-pattern = \" Q \"\nrunning-pattern = \" R \"\n\
             [queues.local]\ndefault = true\n[submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
            stopped.display()
        ))
        .unwrap();
        let sched = Scheduler::new(&m);
        assert!(live_jobs(&sim, &sched).unwrap().is_empty());
        assert!(listed_newest(&sim, &sched).unwrap().is_none());
        assert_eq!(sim_state_now(&sim, &sched).0, DisplayState::Finished);
        // Still active (as after any run): it has ended, nothing to stop,
        // and it is deactivated; then nothing is active, `-f` likewise.
        stop_sim(&sim, &sched, "torque", false).unwrap();
        assert!(restart::active_id(&sim.dir).unwrap().is_none());
        stop_sim(&sim, &sched, "torque", true).unwrap();
        assert!(!stopped.exists(), "qdel ran for a completed job");
        assert_eq!(sim_state_now(&sim, &sched).0, DisplayState::Inactive);
    }

    /// `sim stop` of an active restart that has recorded its end: the stop
    /// is sent to its job whenever the scheduler still lists it or could
    /// not be asked (no graceful path, nothing else recorded); only where
    /// it answered that the job is gone or over, or nothing was asked (past
    /// the requeue window, a simulation of another machine), is nothing
    /// stopped and the restart deactivated. A restart chained behind it is
    /// never stopped unasked.
    #[test]
    fn sim_stop_of_an_ended_restart_stops_its_job_unless_the_scheduler_says_it_is_gone() {
        use crate::mdb::meta::Meta;
        let shapes: [(&str, &str, bool); 5] = [
            ("pd", "echo '@JOB_ID@ PD '", true),
            ("failing", "echo 'slurm_load_jobs error: Socket timed out on send/recv operation' >&2; exit 1", true),
            ("unknown", "echo 'slurm_load_jobs error: Invalid job id specified' >&2; exit 1", false),
            ("cd", "echo '@JOB_ID@ CD '", false),
            ("old", "echo '@JOB_ID@ PD '", false),
        ];
        for (name, get_status, stops) in shapes {
            let tmp = tempfile::tempdir().unwrap();
            let stopped = tmp.path().join("stopped");
            let entry = fake_sim(tmp.path(), name, "1003", true, None, true);
            let sim = Simulation::open(name, &entry.dir).unwrap();
            if name == "old" {
                let path = restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml");
                let long_ago = (chrono::Utc::now() - chrono::Duration::days(15)).to_rfc3339();
                let text = fs::read_to_string(&path).unwrap();
                let text = text.lines().map(|l| if l.starts_with("finished") { format!("finished = \"{long_ago}\"") } else { l.to_owned() }).collect::<Vec<_>>().join("\n");
                fs::write(&path, text + "\n").unwrap();
            }
            // A later restart chained behind its job, queued.
            let first = fs::read_to_string(restart::restart_dir(&sim.dir, 0).join(".cactup/restart.toml")).unwrap();
            let second = first
                .lines()
                .filter(|l| !l.starts_with("terminated") && !l.starts_with("finished"))
                .map(|l| if l.starts_with("job-id") { "job-id = \"1004\"\nchained-job-id = \"1003\"".to_owned() } else { l.to_owned() })
                .collect::<Vec<_>>()
                .join("\n");
            let one = restart::restart_dir(&sim.dir, 1);
            fs::create_dir_all(one.join(".cactup")).unwrap();
            fs::write(one.join(".cactup/restart.toml"), second + "\n").unwrap();
            let m: Meta = toml::from_str(&format!(
                "[machine]\nnickname = \"fake\"\n[scheduler]\nget-status = \"{}\"\nstop = \"echo @JOB_ID@ >> {}\"\n\
                 unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'\n\
                 status-pattern = \"^@JOB_ID@ \"\nqueued-pattern = \" PD \"\nrunning-pattern = \" (CF|CG|R|TO) \"\n\
                 [queues.local]\ndefault = true\n[submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
                get_status.replace('"', "\\\""),
                stopped.display()
            ))
            .unwrap();
            let sched = Scheduler::new(&m);
            for force in [false, true] {
                let _ = fs::remove_file(&stopped);
                if restart::active_id(&sim.dir).unwrap().is_none() {
                    restart::make_active(&sim.dir, 0).unwrap();
                }
                stop_sim(&sim, &sched, name, force).unwrap();
                let sent = fs::read_to_string(&stopped).unwrap_or_default();
                assert_eq!(sent, if stops { "1003\n" } else { "" }, "{name} -f={force}: stops sent");
                assert_eq!(restart::active_id(&sim.dir).unwrap().is_none(), !stops, "{name}: deactivated");
            }
        }
    }

    /// A simulation made for another machine (a home two clusters share):
    /// its ended restart's job is not asked about, stopped, or refused for.
    #[test]
    fn another_machines_simulation_is_not_asked_about_after_its_end() {
        use crate::mdb::meta::Meta;
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let entry = fake_sim(tmp.path(), "away", "1003", true, None, true);
        let sim = Simulation::open("away", &entry.dir).unwrap();
        let m: Meta = toml::from_str(&format!(
            "[machine]\nnickname = \"qb\"\n[scheduler]\nget-status = \"echo @JOB_ID@ >> {}; echo '@JOB_ID@ PD '\"\n\
             status-pattern = \"^@JOB_ID@ \"\nqueued-pattern = \" PD \"\n[queues.local]\ndefault = true\n\
             [submitscripts]\n\"default\" = [\"local\"]\n[runscripts]\n\"default\" = [\"local\"]\n",
            log.display()
        ))
        .unwrap();
        // The simulation's machine is not "qb".
        assert_ne!(sim.meta.machine, "qb");
        let here = Scheduler::named(&m, "qb");
        assert!(live_jobs(&sim, &here).unwrap().is_empty());
        assert!(ended_newest(&sim, &here).unwrap().is_some_and(|(_, answer)| answer.is_none()));
        assert!(!log.exists(), "another machine's job was asked about");
        // Where the machine is its own, it is asked about.
        let own = Scheduler::named(&m, &sim.meta.machine);
        assert_eq!(live_jobs(&sim, &own).unwrap().len(), 1);
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
