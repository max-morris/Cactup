//! `test list` / `show` / `stop` / `delete` (§11.7): managing test runs.
//! Coarser than the sim counterparts — no restart chain, no clean, no reaper.

use crate::commands::Ctx;
use crate::installation::Installation;
use crate::scheduler::{AfterEnd, EndedStop, JobRef, JobStatus, Scheduler, SharedGroup, Stopped, UnverifiedPid};
use crate::sim::cache;
use crate::testsuite::{active_results_id, list_results_ids, results_name, TestRun};
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::fs;
use std::path::PathBuf;

/// The §11.7 display state, coarser than a sim's. `query` is the live
/// status of [`TestRun::pending_job`]: `None` when there was no such job to
/// ask about, `Some(None)` when its query failed — callers resolve it
/// themselves so `list` can batch the query across every row instead of
/// round-tripping the scheduler once per row. `lock_live` is the run's
/// `running.lock`, the one sign of an interactive run, and of a submitted
/// one the queue has forgotten. Before the run records its end a failed
/// query is UNKNOWN, never a state the query did not give; after it, a
/// job the scheduler still lists in a state its patterns read is QUEUED
/// AGAIN (queued, held) or STILL LISTED (running: leaving the queue, or
/// starting again), and a failed query, or a job listed in a state no
/// pattern reads, is the ended job (`is_live`).
fn state_line(run: &TestRun, query: Option<Option<JobStatus>>, lock_live: bool) -> colored::ColoredString {
    let finished = run.meta.timestamps.finished.is_some();
    let status = match query {
        Some(None) if !finished && !lock_live => return "UNKNOWN (status query failed)".yellow(),
        // After the run's end, a job the scheduler still lists: queued again,
        // or (running) leaving the queue or starting again.
        Some(Some(JobStatus::Queued | JobStatus::Holding)) if finished => return "QUEUED AGAIN".yellow(),
        Some(Some(JobStatus::Running)) if finished => return "STILL LISTED".yellow(),
        // A state no pattern reads after the end: the ended job.
        Some(Some(JobStatus::Error)) if finished => Some(JobStatus::Unknown),
        Some(status) => status,
        None => None,
    };
    match status {
        Some(JobStatus::Running) => "RUNNING".green().bold(),
        _ if lock_live => "RUNNING".green().bold(),
        Some(JobStatus::Queued) => "QUEUED".cyan(),
        Some(JobStatus::Holding) => "HOLDING".yellow(),
        Some(JobStatus::Error) => "ERROR".red().bold(),
        Some(JobStatus::Unknown) | None => match &run.meta.results {
            Some(r) if r.failed > 0 => format!("DONE ({} passed, {} failed)", r.passed, r.failed).red(),
            Some(r) => format!("DONE ({} passed, 0 failed)", r.passed).green(),
            None => "INACTIVE".normal(),
        },
    }
}

/// `test show`: one test run in detail.
pub fn show(ctx: &Ctx, name: &str) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let sched = Scheduler::for_machine(&machine);
    let inst = Installation::resolve(ctx)?;
    show_one(&inst, &sched, name)
}

/// One registry entry, resolved from disk ahead of printing.
// `Listed` is the overwhelmingly common variant and the Vec holds one row per
// registered test run (tens, not thousands), so boxing `TestRun` would add an
// allocation per row to save padding on the rare Missing/Broken one.
#[allow(clippy::large_enum_variant)]
enum Row {
    Missing(PathBuf),
    Broken(String),
    Listed { run: TestRun, sets: usize, config: String },
}

/// `test list`: list test runs; `--all` unions every installation.
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
        .map(|inst| inst.tests())
        .collect::<Res<Vec<_>>>()?;

    let rows: Vec<Row> = registries
        .iter()
        .flat_map(|reg| reg.tests.values())
        .map(|entry| {
            if !entry.dir.is_dir() {
                return Row::Missing(entry.dir.clone());
            }
            match TestRun::open(&entry.dir) {
                Ok(run) => {
                    let sets = list_results_ids(&run.dir).map(|v| v.len()).unwrap_or(0);
                    Row::Listed { run, sets, config: entry.config.clone() }
                }
                Err(e) => Row::Broken(format!("{e:#}")),
            }
        })
        .collect();

    // One batched query for every row's job, instead of the scheduler
    // round-trip per row this used to cost (each `get_status` is its own
    // `squeue`/`qstat` call) — the same fix as `sim list`.
    let ids: Vec<JobRef> = rows
        .iter()
        .filter_map(|row| match row {
            Row::Listed { run, .. } => run.pending_job(&sched),
            _ => None,
        })
        .collect();
    let statuses = sched.get_statuses_shown(&ids)?;

    let mut any = false;
    let mut rows = rows.into_iter();
    for (inst, registry) in installations.iter().zip(&registries) {
        if registry.tests.is_empty() {
            continue;
        }
        if all {
            println!("{}", format!("[{}]", inst.alias).bold());
        }
        for name in registry.tests.keys() {
            any = true;
            match rows.next().expect("one row per registry entry") {
                Row::Missing(dir) => println!(
                    "  {:24} {:12} {} (prune with `cactup test delete {name}`)",
                    name.bold(),
                    "MISSING".red(),
                    dir.display()
                ),
                Row::Broken(e) => println!("  {:24} {:12} {e}", name.bold(), "BROKEN".red()),
                Row::Listed { run, sets, config } => {
                    let mut extra = format!("config {config}, {sets} result set(s)");
                    if long {
                        extra.push_str(&format!(", job {}, {}", run.meta.job_id, run.dir.display()));
                    }
                    let query = run.pending_job(&sched).map(|job| statuses.get(job).copied());
                    let lock_live = run.lock_live().unwrap_or(false);
                    println!("  {:24} {:24} {}", name.bold(), state_line(&run, query, lock_live), extra);
                }
            }
        }
    }
    if !any {
        println!("No test runs (start one with `cactup test run`)");
    }
    Ok(())
}

/// Reprint the last run's results (§11.6 step 7) and the result-set history.
fn show_one(inst: &Installation, sched: &Scheduler, name: &str) -> Res<()> {
    let run = TestRun::locate(inst, name)?;
    // One query, as `list` asks: under a progress line, Ctrl-C failing it.
    let pending: Vec<JobRef> = run.pending_job(sched).into_iter().collect();
    let statuses = sched.get_statuses_shown(&pending)?;
    let query = run.pending_job(sched).map(|job| statuses.get(job).copied());
    println!("{}", name.bold());
    println!("  state:       {}", state_line(&run, query, run.lock_live()?));
    println!("  directory:   {}", run.dir.display());
    println!("  config:      {} (build-id {})", run.meta.config, run.meta.build_id);
    println!("  machine:     {}", run.meta.machine);
    println!("  selection:   {}", run.meta.select);
    println!("  job-id:      {}  queue: {}  walltime: {}", run.meta.job_id, run.meta.queue, run.meta.walltime.canonical());
    if let Some(r) = &run.meta.results {
        println!(
            "  results:     {} passed, {} failed ({})",
            r.passed,
            r.failed,
            results_name(r.results_id)
        );
    }
    let active = active_results_id(&run.dir)?;
    let ids = list_results_ids(&run.dir)?;
    if !ids.is_empty() {
        println!("  result sets:");
        for id in ids {
            let marker = if active == Some(id) { " (active)" } else { "" };
            println!("    {}{marker}", results_name(id));
        }
    }
    Ok(())
}

/// `test log`: print the tail of the run's stdout/stderr — paths from the
/// frozen `@STDOUT_FILE@`/`@STDERR_FILE@` vars when present, else
/// `<run-dir>/test.{out,err}` (§11.5). Per `mode`, keep streaming
/// newly-appended bytes until Ctrl-C. Mirrors `sim log` (§8), but against the
/// single per-run output pair — there is no restart chain (§11.6).
pub fn log_cmd(ctx: &Ctx, name: &str, mode: crate::tail::FollowMode) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let run = TestRun::locate(&inst, name)?;

    let mut out = run.dir.join("test.out");
    let mut err = run.dir.join("test.err");
    if let Some(toml::Value::String(s)) = run.meta.vars.get("STDOUT_FILE") {
        out = PathBuf::from(s);
    }
    if let Some(toml::Value::String(s)) = run.meta.vars.get("STDERR_FILE") {
        err = PathBuf::from(s);
    }

    let sources = [("stdout", out), ("stderr", err)];
    let id = active_results_id(&run.dir)?.or(list_results_ids(&run.dir)?.last().copied());
    let subject = match id {
        Some(id) => format!("{} {}", name.bold(), results_name(id)),
        None => name.bold().to_string(),
    };
    crate::tail::tail_log(&sources, mode, &subject)
}

/// Whether the run executes now with no job the scheduler can stop: its
/// `running.lock` is held, and it has no [`TestRun::pending_job`] (an
/// interactive run, whose job id is a pid no scheduler knows, and which may
/// be another host's; or, where job ids are pids, a submitted run that
/// recorded its end). `stop` and `delete` leave such a run alone, as `build
/// stop` leaves a foreground build: it is interrupted where it runs.
fn running_without_job(run: &TestRun, sched: &Scheduler) -> Res<bool> {
    Ok(run.pending_job(sched).is_none() && run.lock_live()?)
}

/// What `stop` and `delete` say of a run [`running_without_job`], and of a
/// submitted one that recorded its end whose lock is still held: an
/// interactive one is interrupted where it runs; a submitted one was
/// stopped or is over (its lock goes when it has ended).
fn without_job(run: &TestRun, name: &str) -> String {
    if run.meta.timestamps.submitted.is_some() {
        format!(
            "test run \"{name}\" was submitted and its job was asked to stop or is over, but its \
             running.lock is still held: it is ending, or it ended without letting go of the lock \
             (which then expires within {} minutes)",
            crate::lock::LOCK_STALE_SECS / 60
        )
    } else {
        format!(
            "test run \"{name}\" ran in the foreground and its running.lock is held, with no queued \
             job cactup can stop: it is running (interrupt it with Ctrl-C where it runs), or it \
             ended without letting go of the lock (a host lost, a hangup; the lock then expires \
             within {} minutes)",
            crate::lock::LOCK_STALE_SECS / 60
        )
    }
}

/// What `stop` and `delete` say of a submitted run whose `running.lock` is
/// held though the scheduler here does not list its job, and that has not
/// recorded its end: no more than that is known (on a batch scheduler,
/// which sees every node, a run that ended without letting go of its lock,
/// or, where the machine cannot tell a failed query from a job it does not
/// know, a query that failed).
fn running_elsewhere(run: &TestRun, sched: &Scheduler, name: &str) -> String {
    // §10: `unknown-job-pattern` makes a failed query an error, never this;
    // where job ids are pids, `ps` is what answers.
    let failed = match sched.tells_failed_queries() || !sched.may_requeue() {
        true => "",
        false => " the scheduler could not be asked,",
    };
    format!(
        "test run \"{name}\" has its running.lock held, though the scheduler here does not list its \
         job {}: it runs where this scheduler cannot see it (another host),{failed} or it ended \
         without letting go of the lock (which then expires within {} minutes); cactup leaves it \
         as it is: stop it where it runs, or try again later",
        run.meta.job_id,
        crate::lock::LOCK_STALE_SECS / 60
    )
}

/// What `stop` and `delete` say of a run whose lock is held though its job
/// is not listed live: [`without_job`] once the run recorded its end,
/// [`running_elsewhere`] before.
fn held_without_listed_job(run: &TestRun, sched: &Scheduler, name: &str) -> String {
    match run.meta.timestamps.finished {
        Some(_) => without_job(run, name),
        None => running_elsewhere(run, sched, name),
    }
}

/// `test stop` (§11.7): the §8.6 stop semantics for the active result
/// set's job; no clean beyond removing the active symlink.
pub fn stop(ctx: &Ctx, name: &str, _force: bool) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let inst = Installation::resolve(ctx)?;
    let mut run = TestRun::locate(&inst, name)?;
    let _lock = run.lock()?;
    stop_run(&mut run, &Scheduler::for_machine(&machine), name)
}

/// [`stop`], on `run` (locked), by `sched`.
fn stop_run(run: &mut TestRun, sched: &Scheduler, name: &str) -> Res<()> {
    if running_without_job(run, sched)? {
        println!("{}", sentence(without_job(run, name)));
        return Ok(());
    }
    let Some(job) = run.pending_job(sched).map(|job| job.to_record()) else {
        println!("Test run {} has no live job; nothing to stop", name.bold());
        return Ok(());
    };
    let job_id = job.id.as_str();
    match job_state(sched, run, job.job())? {
        Job::Unread => {
            if let Err(e) = send_stop(sched, job.job()) {
                // A stop command Ctrl-C ended is no refusal (§2.4), and
                // neither is a pid cactup cannot vouch for (§10).
                if gix::interrupt::is_triggered() {
                    bail!("interrupted");
                }
                if e.downcast_ref::<UnverifiedPid>().is_some() || e.downcast_ref::<SharedGroup>().is_some() {
                    return Err(e);
                }
                let refused = refusal(&e);
                if run.lock_live()? {
                    println!(
                        "{}, and would not stop it ({refused}), but the run's running.lock is held: nothing \
                         recorded. Once the run lets go of it (a lock left by a run that ended expires \
                         within {} minutes), run `cactup test stop {name}` again.",
                        sentence(unread(job_id)),
                        crate::lock::LOCK_STALE_SECS / 60
                    );
                    return Ok(());
                }
                // Not stopped, and nothing of the run holds its lock:
                // over (Torque's `qdel` refuses a completed job), so
                // `delete` and a new run go ahead.
                println!(
                    "{}, and would not stop it ({refused}); nothing of the run holds its lock, so cactup \
                     takes it as ended",
                    sentence(unread(job_id))
                );
                run.meta.timestamps.finished.get_or_insert_with(chrono::Utc::now);
                run.meta.status = Some("U".to_owned());
                run.store_meta()?;
                run.log("test-stop", &format!("job {job_id}: the scheduler would not stop it, and nothing held the lock; taken as ended"));
                return Ok(());
            }
            // Taken: stopped, or over before (Slurm's `scancel` takes an
            // ended job without a word); over either way.
            println!("{}; asked the scheduler to stop it (if it had not ended already)", sentence(unread(job_id)));
            run.meta.timestamps.finished.get_or_insert_with(chrono::Utc::now);
            run.log("test-stop", &format!("asked the scheduler to stop job {job_id}"));
        }
        // Still listed after the run's end (queued again, or leaving the
        // queue): the scheduler is asked to stop it (harmless if it is
        // leaving), and the run's end stands.
        state @ (Job::Listed | Job::Unanswered) => {
            // A stop the scheduler took for a job it could not be asked
            // about, or refused saying it does not know the job, settles the
            // job: recorded (`job-stopped`), so the next command takes it as
            // ended rather than ask a query that may keep failing. Any other
            // failure of the stop command is an error, nothing recorded.
            let mut settled = state == Job::Unanswered;
            match sched.stop_after_end(job.job(), state == Job::Listed)? {
                EndedStop::Sent if state == Job::Unanswered => println!(
                    "Could not ask the scheduler about test job {} (the run has ended); asked it to stop \
                     the job in case it was queued again",
                    job_id.bold()
                ),
                EndedStop::Sent => println!(
                    "Asked the scheduler to stop test job {}, still listed after the run ended (queued \
                     again, or leaving the queue)",
                    job_id.bold()
                ),
                // After a failed query, a stop the scheduler refuses as a job
                // it does not know (by `unknown-job-pattern`) is taken as the
                // job ended; the run's end stands.
                EndedStop::NotKnown(said) => {
                    settled = true;
                    println!(
                        "The scheduler does not know test job {job_id}, whose run has ended (its stop \
                         command said: {said}): taken as ended"
                    );
                }
            }
            run.log("test-stop", &format!("asked the scheduler to stop job {job_id}, still listed after the end"));
            if settled {
                run.meta.timestamps.job_stopped = Some(chrono::Utc::now());
                run.store_meta()?;
            }
            return Ok(());
        }
        Job::Live => {
            match send_stop(sched, job.job())? {
                Stopped::Sent => {
                    println!("Asked the scheduler to stop test job {}", job_id.bold());
                    run.log("test-stop", &format!("stopped job {job_id}"));
                }
                // Its pid's process went between the query and the stop.
                Stopped::Gone => println!("Test job {job_id} has ended; nothing to stop"),
                // `job_state` says `OnHost` of such a job: not reached.
                Stopped::Elsewhere(host) => bail!("test job {job_id} runs on host {host}: stop it there"),
            }
            // Over, as far as cactup is concerned (as `build stop` records
            // an outcome). Only once the stop was sent: a query that failed
            // may read as "not in the queue" too. A batch scheduler's job is
            // still asked about after it (it may be requeued); where job ids
            // are pids, never again (`TestRun::pending_job`).
            run.meta.timestamps.finished.get_or_insert_with(chrono::Utc::now);
        }
        Job::OnHost if run.lock_live()? => {
            println!("{}. Nothing recorded.", sentence(on_host(job.job(), name)));
            return Ok(());
        }
        Job::OnHost => println!(
            "Test job {job_id} was submitted on host {}, and nothing of the run holds its lock: taken as ended",
            job.host.as_deref().unwrap_or_default()
        ),
        Job::Ended | Job::Absent if run.lock_live()? => {
            println!("{}", sentence(held_without_listed_job(run, sched, name)));
            return Ok(());
        }
        Job::Ended => println!("The scheduler lists test job {job_id} as over (the run has ended); nothing to stop"),
        // After the run's end, unlisted or a query that failed: ended.
        Job::Absent if run.meta.timestamps.finished.is_some() => {
            println!("The scheduler does not know test job {job_id} (the run has ended); nothing to stop")
        }
        Job::Absent => println!("Test job {job_id} is not in the queue; nothing to stop"),
    }
    run.meta.status = Some("U".to_owned());
    run.store_meta()?;
    Ok(())
}

/// What the scheduler says of a submitted run's job.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Job {
    /// Running, queued or held.
    Live,
    /// Listed in a state no pattern of the machine's reads
    /// (`JobStatus::Error`), before the run recorded its end: suspended
    /// (Slurm's `S`), exiting (Torque's `E`), or over (Slurm and Torque list
    /// an ended job for a while: `CD`, `F`, `CA`, `C`). Which is not known:
    /// a stop the scheduler takes ends it either way (Slurm's `scancel`
    /// takes an ended job too); one it refuses (Torque's `qdel` of a
    /// completed job) is over where nothing of the run holds its lock.
    Unread,
    /// Listed in a state the patterns read after the run recorded its end
    /// (within the requeue window): queued again, or leaving the queue, which
    /// cactup cannot tell apart (Slurm lists a requeued job `CF` and `R`
    /// before its run clears the end, an ended one `CG`, `TO`). `stop` asks
    /// the scheduler to stop it, `delete` refuses it without `-f`, a new run
    /// refuses to start beside it.
    Listed,
    /// Listed in a state no pattern reads after the run recorded its end:
    /// the ended job (Torque's `C`, Slurm's `CD`, `F`, `CA`): a requeued job
    /// passes through read states. Nothing to stop; `delete` goes ahead.
    Ended,
    /// The status query failed after the run recorded its end: no answer.
    /// `stop` sends the stop (harmless if the job is gone), `delete` takes
    /// it as ended, with a note.
    Unanswered,
    /// Not listed (the scheduler does not know it; where the machine cannot
    /// tell, a query that failed reads so too; after the run's end, a query
    /// that failed is taken so).
    Absent,
    /// A pid recorded on another host (`job-ids = "pids"`, §10): this host
    /// can neither ask about it nor stop it, and only the run's
    /// `running.lock` says whether it runs.
    OnHost,
}

/// What the scheduler says of `run`'s job. Before the run records its end,
/// running, queued or held is live; after it, the same states are
/// `Listed`, a state no pattern reads is `Ended` (`is_live`), and a query
/// that failed is `Unanswered` (`Scheduler::ask_after_end`): `stop` sends
/// the stop, `delete` takes it as ended, a new run refuses beside it.
fn job_state(sched: &Scheduler, run: &TestRun, job: JobRef<'_>) -> Res<Job> {
    if sched.recorded_elsewhere(job).is_some() {
        return Ok(Job::OnHost);
    }
    if run.meta.timestamps.finished.is_some() {
        return Ok(match sched.ask_after_end(job)? {
            AfterEnd::Listed(_) => Job::Listed,
            AfterEnd::Over(JobStatus::Unknown) => Job::Absent,
            AfterEnd::Over(_) => Job::Ended,
            AfterEnd::Unanswered(_) => Job::Unanswered,
        });
    }
    Ok(match sched.get_status(job)? {
        JobStatus::Running | JobStatus::Queued | JobStatus::Holding => Job::Live,
        JobStatus::Unknown => Job::Absent,
        JobStatus::Error => Job::Unread,
    })
}

/// What `stop` and `delete` say of a [`Job::OnHost`] job whose run's lock
/// is live.
fn on_host(job: JobRef<'_>, name: &str) -> String {
    let host = job.host.unwrap_or_default();
    format!(
        "test job {} runs on host {host}, where it was submitted, and this host cannot stop it: stop \
         it there (`cactup test stop {name}` on {host})",
        job.id
    )
}

/// The scheduler's stop of `job`, with what failed named.
fn send_stop(sched: &Scheduler, job: JobRef<'_>) -> Res<Stopped> {
    sched.stop(job).map_err(|e| {
        // These say themselves what happened: no command ran.
        if e.downcast_ref::<UnverifiedPid>().is_some() || e.downcast_ref::<SharedGroup>().is_some() {
            return e;
        }
        e.context(format!("the scheduler's stop command failed for test job {}", job.id))
    })
}

/// A stop command's failure on one line, for inside a sentence.
fn refusal(e: &anyhow::Error) -> String {
    let lines: Vec<String> = format!("{e:#}").lines().map(str::trim).filter(|line| !line.is_empty()).map(|line| line.trim_end_matches(':').to_owned()).collect();
    match lines.is_empty() {
        true => "it said nothing".to_owned(),
        false => lines.join("; "),
    }
}

/// `text` with its first letter capitalized, for a line of its own.
fn sentence(text: String) -> String {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => text,
    }
}

/// What `stop` and `delete` say of a [`Job::Unread`] job.
fn unread(job_id: &str) -> String {
    format!(
        "the scheduler lists test job {job_id} in a state cactup does not read (suspended, exiting, or \
         ended)"
    )
}

/// Refuse to delete `run` while it runs, unless `force` and it is a job the
/// scheduler lists, which is then stopped.
fn clear_to_delete(run: &TestRun, sched: &Scheduler, name: &str, force: bool) -> Res<()> {
    if running_without_job(run, sched)? {
        bail!("{}; delete it once it has ended", without_job(run, name));
    }
    let Some(job) = run.pending_job(sched) else { return Ok(()) };
    let job_id = job.id;
    match job_state(sched, run, job)? {
        // Held, but not a job the scheduler here lists live: not stopped or
        // removed from here, `-f` neither.
        Job::OnHost if run.lock_live()? => bail!("{}; delete it once it has ended", on_host(job, name)),
        // Unanswered after the run's end: what only removes a record takes
        // it as ended (§10), with the note.
        Job::Unanswered => {
            crate::scheduler::note_taken_as_ended(job_id);
            if run.lock_live()? {
                bail!("{}", held_without_listed_job(run, sched, name));
            }
        }
        Job::Ended | Job::Absent if run.lock_live()? => bail!("{}", held_without_listed_job(run, sched, name)),
        Job::Ended | Job::Absent | Job::OnHost => {}
        // An older record's pid, or one sharing its process group (§10):
        // never stopped, `-f` neither.
        Job::Live | Job::Listed if sched.unverified(job) => {
            bail!("{}; then delete the test run", UnverifiedPid::of(job_id))
        }
        Job::Live | Job::Listed if sched.shares_group(job) => {
            bail!("{}; then delete the test run", SharedGroup(job_id.to_owned()))
        }
        Job::Listed if !force => bail!(
            "test run \"{name}\" ended, but the scheduler still lists its job {job_id} (leaving the \
             queue, or starting again): wait until it leaves, or `cactup test stop {name}` to stop it; \
             or use -f to stop it and delete"
        ),
        Job::Live if !force => bail!(
            "test run \"{name}\" has a live job ({job_id}); `cactup test stop {name}` first, or use \
             -f to stop and delete"
        ),
        Job::Live => {
            send_stop(sched, job)?;
        }
        Job::Unread if !force => bail!(
            "{}: `cactup test stop {name}` first, or use -f to stop it and delete",
            unread(job_id)
        ),
        state @ (Job::Unread | Job::Listed) => {
            let what = match state {
                Job::Listed => format!("the scheduler still lists test job {job_id} after the run ended"),
                _ => unread(job_id),
            };
            if let Err(e) = send_stop(sched, job) {
                if gix::interrupt::is_triggered() {
                    bail!("interrupted");
                }
                if e.downcast_ref::<UnverifiedPid>().is_some() || e.downcast_ref::<SharedGroup>().is_some() {
                    return Err(e);
                }
                // Not stopped: over, unless something still holds the lock.
                if run.lock_live()? {
                    bail!(
                        "{}, would not stop it ({}), and the run's running.lock is held: delete it once \
                         the run has let go of the lock (a lock left by a run that ended expires \
                         within {} minutes)",
                        what,
                        refusal(&e),
                        crate::lock::LOCK_STALE_SECS / 60
                    );
                }
                println!("{}, and would not stop it; nothing of the run holds its lock, so cactup takes it as ended: deleting it", sentence(what));
            }
        }
    }
    Ok(())
}

/// `test delete` (§11.7): §8.7 semantics against test-home. `--purge`
/// (or `-f`) removes outright; the default trashes into
/// `<test-home>/TRASH/<test-run-id>/`.
pub fn delete(ctx: &Ctx, name: &str, force: bool, purge: bool) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let inst = Installation::resolve(ctx)?;
    let test_home = inst.meta()?.test_home()?.to_owned();

    let registry = inst.tests()?;
    let Some(entry) = registry.tests.get(name) else {
        bail!("no test run named \"{name}\" (see `cactup test list`)");
    };
    // A stale entry (dir vanished) is pruned rather than fataled.
    if !entry.dir.is_dir() {
        let locked = inst.locked()?;
        let mut reg = locked.tests()?;
        reg.tests.shift_remove(name);
        locked.set_tests(&reg)?;
        println!(
            "Test run {} directory {} was already gone; pruned the stale registry entry",
            name.bold(),
            entry.dir.display()
        );
        return Ok(());
    }

    let run = TestRun::locate(&inst, name)?;
    // Held while it looks: a run being submitted (`test submit` holds this
    // until its job id is recorded) is not taken for one with no job. Let
    // go just before the directory goes (the lock is in it).
    let lock = run.lock()?;
    clear_to_delete(&run, &Scheduler::for_machine(&machine), name, force)?;
    drop(lock);
    if purge || force {
        fs::remove_dir_all(&run.dir)?;
        println!("Deleted test run {} permanently", name.bold());
    } else {
        let dst = cache::move_to_trash(&test_home, &run.dir, &run.meta.test_run_id)?;
        println!("Moved test run {} to {}", name.bold(), dst.display());
    }

    let locked = inst.locked()?;
    let mut reg = locked.tests()?;
    reg.tests.shift_remove(name);
    locked.set_tests(&reg)?;
    drop(locked);

    // Executable-cache GC (§11.7); the test-home cache exists for symmetry.
    let _ = cache::gc(&test_home, None)?;
    Ok(())
}

/// `test clean`: remove in-tree testsuite output from the Cactus source tree —
/// `TEST/` at the Cactus root (the flesh's default output dir when TESTS_DIR
/// is not exported) and any `configs/<cfg>/TEST` leftover. cactup-driven runs
/// redirect results to test-home (§11.5), so anything found here is a dropping
/// from a manual `make <cfg>-testsuite` or a pre-fix cactup run.
pub fn clean(ctx: &Ctx) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let removed = clean_tree(&inst.cactus_root())?;
    if removed.is_empty() {
        println!("Nothing to clean: the source tree has no in-tree testsuite output.");
    } else {
        for path in &removed {
            println!("Removed {}", path.display());
        }
        println!("{}", format!("Cleaned {} in-tree testsuite path(s).", removed.len()).bright_green());
    }
    Ok(())
}

fn clean_tree(cactus_root: &std::path::Path) -> Res<Vec<std::path::PathBuf>> {
    let mut removed = Vec::new();

    let mut remove_entry = |path: std::path::PathBuf| -> Res<()> {
        // symlink_metadata so a symlink is removed as a link — never follow
        // it into the results dir it may point at.
        match fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e).with_context(|| format!("Failed to inspect {}", path.display())),
            Ok(meta) => {
                if meta.file_type().is_dir() {
                    fs::remove_dir_all(&path)
                        .with_context(|| format!("Failed to remove {}", path.display()))?;
                } else {
                    fs::remove_file(&path)
                        .with_context(|| format!("Failed to remove {}", path.display()))?;
                }
                removed.push(path);
                Ok(())
            }
        }
    };

    remove_entry(cactus_root.join("TEST"))?;

    let configs_dir = cactus_root.join("configs");
    match fs::read_dir(&configs_dir) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        entries => {
            for entry in entries.with_context(|| format!("Failed to list {}", configs_dir.display()))? {
                let entry = entry?;
                if entry.file_type()?.is_dir() {
                    remove_entry(entry.path().join("TEST"))?;
                }
            }
        }
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::restart::NO_JOB_ID;
    use crate::mdb::meta::Meta;
    use std::path::Path;

    /// A machine of the fake scheduler `scheduler_toml` describes.
    fn fake_meta(scheduler_toml: &str) -> Meta {
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

    /// An interactive run's job id is the pid of the cactup that ran it:
    /// only while its `running.lock` is held is that pid still the run's.
    /// Finished or killed, it is never handed to the scheduler — its stop
    /// command would stop whatever process has the pid now. A submitted
    /// run's job is; once the run records its end, only where a batch
    /// scheduler may requeue it.
    #[test]
    fn an_interactive_runs_pid_is_a_job_only_while_its_lock_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let batch_meta = fake_meta("");
        let batch = Scheduler::new(&batch_meta);
        let pids_meta = fake_meta(r#"job-ids = "pids""#);
        let pids = Scheduler::new(&pids_meta);
        let pending = |run: &TestRun, sched: &Scheduler| run.pending_job(sched).map(|job| job.id.to_owned());
        let open = |submitted: bool, finished: bool, job_id: &str| {
            let dir = tmp.path().join(format!("run-{submitted}-{finished}-{job_id}"));
            fs::create_dir_all(dir.join(".cactup")).unwrap();
            let mut timestamps = String::new();
            if submitted {
                timestamps.push_str("submitted = \"2026-10-16T00:00:00Z\"\n");
            }
            if finished {
                // Within the requeue window: now.
                timestamps.push_str(&format!("finished = \"{}\"\n", chrono::Utc::now().to_rfc3339()));
            }
            fs::write(
                TestRun::meta_path(&dir),
                format!(
                    r#"
                    name = "t"
                    config = "c"
                    config-id = "c"
                    build-id = "b"
                    machine = "m"
                    alias = "a"
                    test-run-id = "t"
                    select = "all"
                    queue = "q"
                    nodes = 1
                    tasks = 1
                    tpn = 1
                    cpus = 1
                    walltime = "1:00:00"
                    job-id = "{job_id}"
                    [timestamps]
                    {timestamps}
                    "#
                ),
            )
            .unwrap();
            TestRun::open(&dir).unwrap()
        };
        let pid = std::process::id().to_string();

        for finished in [false, true] {
            let run = open(false, finished, &pid);
            for sched in [&batch, &pids] {
                assert_eq!(run.pending_job(sched), None);
                assert!(!running_without_job(&run, sched).unwrap());
            }
        }
        // Running now, interactively: never handed to the scheduler (its pid
        // is no job the scheduler knows, and the lock may be another
        // host's); `stop` stops nothing and leaves the run as it is.
        let mut live = open(false, false, &pid);
        let _held = crate::lock::LinkLock::acquire(&live.running_lock_path()).unwrap();
        for sched in [&batch, &pids] {
            assert_eq!(live.pending_job(sched), None);
            assert!(running_without_job(&live, sched).unwrap());
        }

        for sched in [&batch, &pids] {
            assert_eq!(pending(&open(true, false, "JOB-1"), sched).as_deref(), Some("JOB-1"));
            assert_eq!(pending(&open(true, false, NO_JOB_ID), sched), None);
        }
        // Ended: a batch scheduler's job is still asked about (it may be
        // requeued); a pid is not (it may be another process's by now).
        assert_eq!(pending(&open(true, true, "JOB-1"), &batch).as_deref(), Some("JOB-1"));
        assert_eq!(pending(&open(true, true, "JOB-1"), &pids), None);

        // A submitted run `test stop` stopped: asked about again on a batch
        // scheduler, which no longer lists it, so nothing is stopped twice.
        let stopped = tmp.path().join("stopped");
        let meta = fake_meta(&format!(
            r#"
            get-status = "test -e {gone}-@JOB_ID@ || echo '@JOB_ID@ R'"
            status-pattern = "^@JOB_ID@ "
            running-pattern = " R"
            stop = "touch {stopped} {gone}-@JOB_ID@"
            "#,
            stopped = stopped.display(),
            gone = tmp.path().join("gone").display()
        ));
        let sched = Scheduler::new(&meta);
        stop_run(&mut live, &sched, "t").unwrap();
        for force in [false, true] {
            assert!(clear_to_delete(&live, &sched, "t", force).unwrap_err().to_string().contains("ran in the foreground"));
        }
        assert!(!stopped.exists(), "an interactive run's pid went to the scheduler");
        assert!(TestRun::open(&live.dir).unwrap().meta.timestamps.finished.is_none());
        let mut run = open(true, false, "JOB-2");
        stop_run(&mut run, &sched, "t").unwrap();
        assert!(stopped.exists());
        let mut run = TestRun::open(&run.dir).unwrap();
        assert!(run.meta.timestamps.finished.is_some());
        assert_eq!(pending(&run, &sched).as_deref(), Some("JOB-2"));
        fs::remove_file(&stopped).unwrap();
        stop_run(&mut run, &sched, "t").unwrap();
        assert!(!stopped.exists(), "stopped twice");

        // Submitted, its lock held, but its job unknown to the scheduler
        // here (it runs on another host): not stopped, not marked ended.
        let unknown: Meta = toml::from_str(&format!(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            get-status = "true"
            status-pattern = "^@JOB_ID@ "
            stop = "touch {}"
            [queues.local]
            default = true
            [submitscripts]
            "default" = ["local"]
            [runscripts]
            "default" = ["local"]
            "#,
            stopped.display()
        ))
        .unwrap();
        let mut elsewhere = open(true, false, "JOB-3");
        let _held_elsewhere = crate::lock::LinkLock::acquire(&elsewhere.running_lock_path()).unwrap();
        assert!(!running_without_job(&elsewhere, &batch).unwrap());
        stop_run(&mut elsewhere, &Scheduler::new(&unknown), "t").unwrap();
        assert!(!stopped.exists());
        assert!(TestRun::open(&elsewhere.dir).unwrap().meta.timestamps.finished.is_none());

        // `delete` refuses a run held without a job the scheduler lists
        // (`-f` too), and stops nothing.
        for force in [false, true] {
            let refused = clear_to_delete(&elsewhere, &Scheduler::new(&unknown), "t", force).unwrap_err().to_string();
            assert!(refused.contains("does not list its job JOB-3"), "{refused}");
        }
        assert!(!stopped.exists());

        // A query that found nothing (it failed, or the job is gone): no
        // stop was sent, so the run is not taken for ended; the next
        // `stop` or `delete` asks again.
        let mut queried = open(true, false, "JOB-4");
        stop_run(&mut queried, &Scheduler::new(&unknown), "t").unwrap();
        assert!(!stopped.exists());
        assert_eq!(pending(&TestRun::open(&queried.dir).unwrap(), &batch).as_deref(), Some("JOB-4"));

        // A listed job: refused without `-f`, stopped with it.
        let listed = open(true, false, "JOB-5");
        assert!(clear_to_delete(&listed, &sched, "t", false).unwrap_err().to_string().contains("has a live job (JOB-5)"));
        assert!(!stopped.exists());
        clear_to_delete(&listed, &sched, "t", true).unwrap();
        assert!(stopped.exists());

        // Listed in a state cactup does not read (Torque's exiting `E`):
        // the scheduler's job all the same, refused by `delete` without
        // `-f`, stopped by `stop`, with its lock held or not.
        fs::remove_file(&stopped).unwrap();
        let exiting: Meta = toml::from_str(&format!(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            get-status = "echo '@JOB_ID@ E'"
            status-pattern = "^@JOB_ID@ "
            running-pattern = " R"
            stop = "touch {}"
            [queues.local]
            default = true
            [submitscripts]
            "default" = ["local"]
            [runscripts]
            "default" = ["local"]
            "#,
            stopped.display()
        ))
        .unwrap();
        let exiting = Scheduler::new(&exiting);
        let mut ending = open(true, false, "JOB-6");
        let _held_ending = crate::lock::LinkLock::acquire(&ending.running_lock_path()).unwrap();
        assert!(clear_to_delete(&ending, &exiting, "t", false).unwrap_err().to_string().contains("in a state cactup does not read"));
        assert!(!stopped.exists());
        stop_run(&mut ending, &exiting, "t").unwrap();
        assert!(stopped.exists());
        assert!(TestRun::open(&ending.dir).unwrap().meta.timestamps.finished.is_some());

        // Listed so, and the scheduler will not stop it (Torque's `qdel` of
        // a completed `C` job): `delete -f` goes ahead without a lock held,
        // and not with one; `stop` records nothing with the lock held, and
        // takes the run as ended without it.
        let refusing: Meta = toml::from_str(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            get-status = "echo '@JOB_ID@ C'"
            status-pattern = "^@JOB_ID@ "
            running-pattern = " R"
            stop = "echo 'qdel: Invalid state for job - COMPLETE' >&2; exit 170"
            [queues.local]
            default = true
            [submitscripts]
            "default" = ["local"]
            [runscripts]
            "default" = ["local"]
            "#,
        )
        .unwrap();
        let refusing = Scheduler::new(&refusing);
        let ended = open(true, false, "JOB-7");
        assert!(clear_to_delete(&ended, &refusing, "t", false).unwrap_err().to_string().contains("use -f"));
        clear_to_delete(&ended, &refusing, "t", true).unwrap();
        let held_ended = crate::lock::LinkLock::acquire(&ended.running_lock_path()).unwrap();
        assert!(clear_to_delete(&ended, &refusing, "t", true).unwrap_err().to_string().contains("running.lock is held"));
        // With the lock held, `stop` records nothing; without it, the run
        // has ended, and `delete` then goes ahead without `-f`.
        let mut ended = TestRun::open(&ended.dir).unwrap();
        stop_run(&mut ended, &refusing, "t").unwrap();
        assert!(TestRun::open(&ended.dir).unwrap().meta.timestamps.finished.is_none());
        drop(held_ended);
        stop_run(&mut ended, &refusing, "t").unwrap();
        let ended = TestRun::open(&ended.dir).unwrap();
        assert!(ended.meta.timestamps.finished.is_some());
        // Still listed `C` (Torque's completed state, within
        // keep_completed), now after the run's end: a state no pattern
        // reads, the ended job (a requeued one passes through `Q`, `R`).
        // `stop` says it has ended and runs no `qdel`; `delete` goes ahead.
        assert_eq!(job_state(&refusing, &ended, ended.pending_job(&refusing).unwrap()).unwrap(), Job::Ended);
        let mut again = TestRun::open(&ended.dir).unwrap();
        stop_run(&mut again, &refusing, "t").unwrap();
        clear_to_delete(&ended, &refusing, "t", false).unwrap();
        clear_to_delete(&ended, &refusing, "t", true).unwrap();
    }

    /// A submitted test run at `dir`, its job `job_id` recorded with
    /// `origin` (`job-host`, `job-start`), `finished` or not.
    fn submitted_run(dir: &Path, job_id: &str, origin: Option<(&str, Option<u64>)>, finished: bool) -> TestRun {
        fs::create_dir_all(dir.join(".cactup")).unwrap();
        let mut keys = String::new();
        if let Some((host, start)) = origin {
            keys.push_str(&format!("job-host = \"{host}\"\n"));
            if let Some(start) = start {
                keys.push_str(&format!("job-start = {start}\n"));
            }
        }
        // Within the requeue window: now.
        let finished = match finished {
            true => format!("finished = \"{}\"", chrono::Utc::now().to_rfc3339()),
            false => String::new(),
        };
        fs::write(
            TestRun::meta_path(dir),
            format!(
                "name = \"t\"\nconfig = \"c\"\nconfig-id = \"c\"\nbuild-id = \"b\"\nmachine = \"m\"\n\
                 alias = \"a\"\ntest-run-id = \"t\"\nselect = \"all\"\nqueue = \"q\"\nnodes = 1\n\
                 tasks = 1\ntpn = 1\ncpus = 1\nwalltime = \"1:00:00\"\njob-id = \"{job_id}\"\n{keys}\
                 [timestamps]\nsubmitted = \"2026-10-16T00:00:00Z\"\n{finished}\n"
            ),
        )
        .unwrap();
        TestRun::open(dir).unwrap()
    }

    /// A job a batch scheduler requeued after its run recorded `finished`
    /// (preemption, `scontrol requeue`): waiting in the queue, it is seen
    /// (`list` shows it queued, `delete` refuses it without `-f`, `stop`
    /// stops it); once it has left the queue the run is as it was.
    #[test]
    fn a_requeued_test_job_is_seen_after_its_run_recorded_its_end() {
        let tmp = tempfile::tempdir().unwrap();
        let gone = tmp.path().join("gone");
        let meta = fake_meta(&format!(
            r#"
            get-status = "test -e {gone} || echo '@JOB_ID@ PD'"
            stop = "touch {gone}"
            status-pattern = "^@JOB_ID@ "
            queued-pattern = " PD"
            running-pattern = " R"
            "#,
            gone = gone.display()
        ));
        let sched = Scheduler::new(&meta);
        let mut run = submitted_run(&tmp.path().join("run"), "4242", None, true);
        let job = run.pending_job(&sched).expect("a batch scheduler's job is asked about after the end");
        assert_eq!(sched.get_status(job).unwrap(), JobStatus::Queued);
        assert_eq!(state_line(&run, Some(Some(JobStatus::Queued)), false).to_string(), "QUEUED AGAIN".yellow().to_string());
        let refused = clear_to_delete(&run, &sched, "t", false).unwrap_err().to_string();
        assert!(refused.contains("still lists its job 4242"), "{refused}");
        stop_run(&mut run, &sched, "t").unwrap();
        assert!(gone.exists(), "the requeued job was not stopped");
        clear_to_delete(&run, &sched, "t", false).unwrap();
    }

    /// A status query that failed (where the machine can tell it from a job
    /// the scheduler does not know) is no "not in the queue": `stop` and
    /// `delete`, `-f` too, refuse with it, and nothing is recorded or
    /// stopped.
    #[test]
    fn a_failed_status_query_is_refused_not_taken_for_a_job_gone() {
        let tmp = tempfile::tempdir().unwrap();
        let stopped = tmp.path().join("stopped");
        let meta = fake_meta(&format!(
            r#"
            get-status = "echo 'slurm_load_jobs error: Socket timed out on send/recv operation' >&2; exit 1"
            unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'
            stop = "touch {}"
            status-pattern = "^ *@JOB_ID@ "
            running-pattern = " R "
            "#,
            stopped.display()
        ));
        let sched = Scheduler::new(&meta);
        let mut run = submitted_run(&tmp.path().join("run"), "4242", None, false);
        let err = format!("{:#}", stop_run(&mut run, &sched, "t").unwrap_err());
        assert!(err.contains("the status query for job 4242") && err.contains("Socket timed out"), "{err}");
        for force in [false, true] {
            let err = format!("{:#}", clear_to_delete(&run, &sched, "t", force).unwrap_err());
            assert!(err.contains("the status query for job 4242"), "{err}");
        }
        let run = TestRun::open(&run.dir).unwrap();
        assert!(run.meta.timestamps.finished.is_none() && run.meta.status.is_none());
        assert!(!stopped.exists());
    }

    /// Where job ids are pids (`job-ids = "pids"`): a submitted run whose
    /// pid now names another process (a start time that differs), or one
    /// on another host, is never stopped (`stop`, `delete -f`); one an
    /// older cactup recorded is refused while its pid has a process here.
    #[test]
    fn a_test_runs_pid_is_stopped_only_while_it_is_the_jobs_process() {
        let tmp = tempfile::tempdir().unwrap();
        let stopped = tmp.path().join("stopped");
        let meta = fake_meta(&format!(
            r#"
            job-ids = "pids"
            get-status = "ps @JOB_ID@"
            stop = "echo @JOB_ID@ >> {}"
            status-pattern = "^ *@JOB_ID@ "
            running-pattern = "^"
            "#,
            stopped.display()
        ));
        let sched = Scheduler::new(&meta);
        // Leading its own process group, as a job `setsid` starts.
        let mut sleeper = {
            use std::os::unix::process::CommandExt;
            std::process::Command::new("sleep").arg("30").process_group(0).spawn().unwrap()
        };
        let pid = sleeper.id().to_string();
        let start = crate::lock::proc_starttime(sleeper.id()).unwrap();
        let here = crate::lock::our_hostname();

        for (name, origin) in [("reused", (here.as_str(), Some(start + 1))), ("elsewhere", ("some-other-host", Some(start)))] {
            let mut run = submitted_run(&tmp.path().join(name), &pid, Some(origin), false);
            stop_run(&mut run, &sched, "t").unwrap();
            clear_to_delete(&run, &sched, "t", true).unwrap();
        }
        assert!(!stopped.exists(), "a pid that is not the job's process was stopped");

        // Another host's job whose run holds its lock: it runs there, never
        // taken for gone; `stop` records nothing, `delete` refuses, `-f` too.
        let mut away = submitted_run(&tmp.path().join("away"), &pid, Some(("some-other-host", Some(start))), false);
        let held = crate::lock::LinkLock::acquire(&away.running_lock_path()).unwrap();
        assert_eq!(job_state(&sched, &away, away.meta.job()).unwrap(), Job::OnHost);
        stop_run(&mut away, &sched, "t").unwrap();
        let after = TestRun::open(&away.dir).unwrap();
        assert!(after.meta.timestamps.finished.is_none() && after.meta.status.is_none());
        for force in [false, true] {
            let refused = clear_to_delete(&away, &sched, "t", force).unwrap_err().to_string();
            assert!(refused.contains("runs on host some-other-host") && refused.contains("stop it there"), "{refused}");
        }
        drop(held);
        clear_to_delete(&away, &sched, "t", false).unwrap();
        assert!(!stopped.exists());

        // An older record (no host or start time), made long before the
        // process with its pid started: not the job.
        let mut stale = submitted_run(&tmp.path().join("stale"), &pid, None, false);
        stop_run(&mut stale, &sched, "t").unwrap();
        clear_to_delete(&stale, &sched, "t", true).unwrap();
        assert!(!stopped.exists(), "an older record's reused pid was stopped");
        // One made after that process started: it may be the job.
        let mut old = submitted_run(&tmp.path().join("old"), &pid, None, false);
        old.meta.timestamps.submitted = Some(chrono::Utc::now());
        old.store_meta().unwrap();
        let err = format!("{:#}", stop_run(&mut old, &sched, "t").unwrap_err());
        assert!(err.contains("recorded by an older cactup"), "{err}");
        assert!(!err.contains("stop command failed"), "no command ran: {err}");
        let refused = format!("{:#}", clear_to_delete(&old, &sched, "t", false).unwrap_err());
        assert!(refused.contains("recorded by an older cactup") && refused.contains(&format!("kill -- -{pid}")), "{refused}");
        assert!(format!("{:#}", clear_to_delete(&old, &sched, "t", true).unwrap_err()).contains("recorded by an older cactup"));
        assert!(!stopped.exists(), "an older record's pid was stopped");
        assert!(TestRun::open(&old.dir).unwrap().meta.timestamps.finished.is_none());

        let mut own = submitted_run(&tmp.path().join("own"), &pid, Some((here.as_str(), Some(start))), false);
        stop_run(&mut own, &sched, "t").unwrap();
        assert_eq!(fs::read_to_string(&stopped).unwrap().trim(), pid);
        // Ended where job ids are pids: not asked about again.
        assert_eq!(TestRun::open(&own.dir).unwrap().pending_job(&sched), None);
        let _ = sleeper.kill();
        let _ = sleeper.wait();
    }

    /// After a run recorded `finished`, a job the scheduler still lists, in
    /// any state, is either leaving the queue (Slurm's `CG`, `TO`, `CD`) or
    /// starting again (`PD`, `CF`, `R` before its run clears the end), which
    /// cactup cannot tell apart: `Job::Listed`. `delete` refuses it without
    /// `-f`, a new run refuses beside it, `stop` asks the scheduler to stop
    /// it and records nothing else; `list` says QUEUED AGAIN or STILL
    /// LISTED. A status query that fails after the end is taken as the job
    /// ended; before the end it shows UNKNOWN, never a state.
    #[test]
    fn after_its_end_a_test_runs_job_still_listed_in_any_state_is_live() {
        let tmp = tempfile::tempdir().unwrap();
        let state = tmp.path().join("state");
        let stopped = tmp.path().join("stopped");
        let meta = fake_meta(&format!(
            r#"
            get-status = "echo \"@JOB_ID@ $(cat {})\" "
            stop = "echo @JOB_ID@ >> {}"
            status-pattern = "^@JOB_ID@ "
            queued-pattern = " PD"
            running-pattern = " (CF|CG|R|TO)"
            "#,
            state.display(),
            stopped.display()
        ));
        let sched = Scheduler::new(&meta);
        let mut run = submitted_run(&tmp.path().join("run"), "4242", None, true);
        let before = fs::read_to_string(TestRun::meta_path(&run.dir)).unwrap();
        for letter in ["CF", "R", "CG", "TO", "PD"] {
            fs::write(&state, letter).unwrap();
            assert_eq!(job_state(&sched, &run, run.meta.job()).unwrap(), Job::Listed, "{letter}");
            let refused = clear_to_delete(&run, &sched, "t", false).unwrap_err().to_string();
            assert!(refused.contains("still lists its job 4242") && refused.contains("test stop t"), "{letter}: {refused}");
            stop_run(&mut run, &sched, "t").unwrap();
            assert_eq!(fs::read_to_string(TestRun::meta_path(&run.dir)).unwrap(), before, "{letter}: recorded something");
        }
        assert_eq!(fs::read_to_string(&stopped).unwrap().lines().count(), 5);
        // `CD`, a state no pattern reads: the ended job; nothing to stop,
        // `delete` goes ahead.
        fs::write(&state, "CD").unwrap();
        assert_eq!(job_state(&sched, &run, run.meta.job()).unwrap(), Job::Ended);
        stop_run(&mut run, &sched, "t").unwrap();
        clear_to_delete(&run, &sched, "t", false).unwrap();
        assert_eq!(fs::read_to_string(&stopped).unwrap().lines().count(), 5);
        fs::write(&state, "R").unwrap();
        assert_eq!(state_line(&run, Some(Some(JobStatus::Running)), false).to_string(), "STILL LISTED".yellow().to_string());
        assert_eq!(state_line(&run, Some(Some(JobStatus::Error)), false).to_string(), "INACTIVE".normal().to_string());
        assert_eq!(state_line(&run, Some(Some(JobStatus::Queued)), false).to_string(), "QUEUED AGAIN".yellow().to_string());
        assert_eq!(state_line(&run, Some(None), false).to_string(), "INACTIVE".normal().to_string());
        // With -f, delete stops it and goes ahead.
        clear_to_delete(&run, &sched, "t", true).unwrap();
        assert_eq!(fs::read_to_string(&stopped).unwrap().lines().count(), 6);

        let failing = fake_meta(
            r#"
            get-status = "echo 'slurm_load_jobs error: Socket timed out on send/recv operation' >&2; exit 1"
            unknown-job-pattern = 'slurm_load_jobs error: Invalid job id specified'
            stop = "true"
            status-pattern = "^@JOB_ID@ "
            "#,
        );
        let failing = Scheduler::new(&failing);
        // After the end, a failed query is no answer: `stop` sends the stop
        // (harmless if the job is gone), `delete` takes it as ended.
        let mut ended = submitted_run(&tmp.path().join("ended"), "4243", None, true);
        assert_eq!(job_state(&failing, &ended, ended.meta.job()).unwrap(), Job::Unanswered);
        stop_run(&mut ended, &failing, "t").unwrap();
        // The stop settles the job: recorded, and not asked about again.
        let reread = TestRun::open(&ended.dir).unwrap();
        assert!(reread.meta.timestamps.job_stopped.is_some());
        assert!(reread.pending_job(&failing).is_none());
        // A job the scheduler has forgotten (its message one the pattern
        // misses): the stop refused, `test stop` still succeeds, taken as
        // ended and settled.
        let forgetting = fake_meta(
            r#"
            get-status = "echo 'error: no such job @JOB_ID@' >&2; exit 1"
            unknown-job-pattern = 'Invalid job id specified'
            stop = "echo 'scancel: error: Kill job error on job id @JOB_ID@: Invalid job id specified' >&2; exit 1"
            status-pattern = "^@JOB_ID@ "
            "#,
        );
        let forgetting = Scheduler::new(&forgetting);
        let mut forgotten = submitted_run(&tmp.path().join("forgotten"), "4245", None, true);
        stop_run(&mut forgotten, &forgetting, "t").unwrap();
        assert!(TestRun::open(&forgotten.dir).unwrap().pending_job(&forgetting).is_none());
        // A stop that fails transiently (the controller not reached), after
        // a failed query or after the scheduler listed the job: an error,
        // nothing recorded.
        for get_status in ["echo 'Socket timed out' >&2; exit 1", "echo '@JOB_ID@ PD'"] {
            let transient = fake_meta(&format!(
                r#"
                get-status = "{get_status}"
                unknown-job-pattern = 'Invalid job id specified'
                stop = "echo 'scancel: error: Unable to contact slurm controller (connect failure)' >&2; exit 1"
                status-pattern = "^@JOB_ID@ "
                queued-pattern = " PD"
                "#
            ));
            let transient = Scheduler::new(&transient);
            let mut run = submitted_run(&tmp.path().join(format!("t{}", get_status.len())), "4246", None, true);
            let before = fs::read_to_string(TestRun::meta_path(&run.dir)).unwrap();
            let err = stop_run(&mut run, &transient, "t").unwrap_err().to_string();
            assert!(err.contains("Unable to contact slurm controller"), "{err}");
            assert_eq!(fs::read_to_string(TestRun::meta_path(&run.dir)).unwrap(), before, "recorded something");
        }
        clear_to_delete(&ended, &failing, "t", false).unwrap();
        clear_to_delete(&ended, &failing, "t", true).unwrap();
        let open = submitted_run(&tmp.path().join("open"), "4244", None, false);
        assert!(clear_to_delete(&open, &failing, "t", false).is_err());
        assert_eq!(state_line(&open, Some(None), false).to_string(), "UNKNOWN (status query failed)".yellow().to_string());
    }

    /// A test run's job is asked about after its `finished` only within
    /// the requeue window: fifteen days on, it is done.
    #[test]
    fn a_finished_test_runs_job_is_asked_about_only_within_the_requeue_window() {
        let tmp = tempfile::tempdir().unwrap();
        let batch = fake_meta("");
        let batch = Scheduler::new(&batch);
        let mut run = submitted_run(&tmp.path().join("run"), "4242", None, true);
        assert!(run.pending_job(&batch).is_some());
        run.meta.timestamps.finished = Some(chrono::Utc::now() - chrono::Duration::days(15));
        assert!(run.pending_job(&batch).is_none());
    }

    /// A test run made for another machine (a home two clusters share):
    /// after its end its job is not asked about; before it, as ever.
    #[test]
    fn another_machines_finished_test_run_is_not_asked_about() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = fake_meta("");
        let other = Scheduler::named(&meta, "other");
        let own = Scheduler::named(&meta, "m");
        let ended = submitted_run(&tmp.path().join("ended"), "4242", None, true);
        assert_eq!(ended.meta.machine, "m");
        assert!(ended.pending_job(&other).is_none());
        assert!(ended.pending_job(&own).is_some());
        let open = submitted_run(&tmp.path().join("open"), "4243", None, false);
        assert!(open.pending_job(&other).is_some());
    }

    /// `list`'s whole point is one `get_statuses` call instead of one
    /// `get_status` per row (§10); pin that a duplicate job id in the batch
    /// still only invokes the `get-status` command once per distinct id.
    #[test]
    fn get_statuses_queries_each_distinct_job_once() {
        let tmp = tempfile::tempdir().unwrap();
        let log = tmp.path().join("calls.log");
        let meta: Meta = toml::from_str(&format!(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            get-status = "echo @JOB_ID@ >> {}; echo '@JOB_ID@ R'"
            status-pattern = "^@JOB_ID@ "
            running-pattern = " R"
            [queues.local]
            default = true
            [submitscripts]
            "default" = ["local"]
            [runscripts]
            "default" = ["local"]
            "#,
            log.display()
        ))
        .unwrap();

        // "1" appears twice; batching must still resolve it to one call.
        let jobs: Vec<JobRef> = ["1", "2", "1"].into_iter().map(JobRef::bare).collect();
        let statuses = Scheduler::new(&meta).get_statuses(&jobs);
        assert_eq!(statuses.len(), 2);

        let mut calls: Vec<String> = fs::read_to_string(&log).unwrap().lines().map(str::to_owned).collect();
        calls.sort();
        assert_eq!(calls, vec!["1", "2"]);
    }

    #[test]
    fn clean_tree_removes_in_tree_test_output_only() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("Cactus");

        // The flesh's default in-tree output dir.
        fs::create_dir_all(cactus.join("TEST/tests/thorn")).unwrap();
        fs::write(cactus.join("TEST/tests/summary.log"), "Number failed -> 0").unwrap();

        // A pre-fix cactup symlink at configs/<cfg>/TEST, pointing into
        // test-home — the link must go, its target must survive.
        let results = tmp.path().join("testhome/results-0000");
        fs::create_dir_all(&results).unwrap();
        fs::write(results.join("keep.log"), "precious").unwrap();
        fs::create_dir_all(cactus.join("configs/tests/config-data")).unwrap();
        std::os::unix::fs::symlink(&results, cactus.join("configs/tests/TEST")).unwrap();

        // A config without droppings stays untouched.
        fs::create_dir_all(cactus.join("configs/other/config-data")).unwrap();

        let removed = clean_tree(&cactus).unwrap();
        assert_eq!(removed.len(), 2, "{removed:?}");
        assert!(!cactus.join("TEST").exists());
        assert!(fs::symlink_metadata(cactus.join("configs/tests/TEST")).is_err(), "link removed");
        assert!(results.join("keep.log").is_file(), "symlink target untouched");
        assert!(cactus.join("configs/tests/config-data").is_dir(), "config itself untouched");
        assert!(cactus.join("configs/other/config-data").is_dir());

        // Idempotent: a second clean finds nothing.
        assert!(clean_tree(&cactus).unwrap().is_empty());
        // A tree with no configs dir at all is fine too.
        assert!(clean_tree(Path::new("/nonexistent-cactus")).unwrap().is_empty());
    }
}
