//! `test list` / `show` / `stop` / `delete` (§11.7): managing test runs.
//! Coarser than the sim counterparts — no restart chain, no clean, no reaper.

use crate::commands::Ctx;
use crate::installation::Installation;
use crate::scheduler::{JobStatus, Scheduler};
use crate::sim::cache;
use crate::testsuite::{active_results_id, list_results_ids, results_name, TestRun};
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::fs;
use std::path::PathBuf;

/// The §11.7 display state, coarser than a sim's. `status` is the
/// already-resolved live status of [`TestRun::pending_job`] (`None` for no
/// such job / a failed query) — callers resolve it themselves so `list` can
/// batch the query across every row instead of round-tripping the scheduler
/// once per row. `lock_live` is the run's `running.lock`, the one sign of an
/// interactive run, and of a submitted one the queue has forgotten.
fn state_line(run: &TestRun, status: Option<JobStatus>, lock_live: bool) -> colored::ColoredString {
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
    let sched = Scheduler::new(&machine.meta);
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
    let ids: Vec<&str> = rows
        .iter()
        .filter_map(|row| match row {
            Row::Listed { run, .. } => run.pending_job(),
            _ => None,
        })
        .collect();
    let statuses = sched.get_statuses(&ids);

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
                    let status = run.pending_job().and_then(|job| statuses.get(job).copied());
                    let lock_live = run.lock_live().unwrap_or(false);
                    println!("  {:24} {:24} {}", name.bold(), state_line(&run, status, lock_live), extra);
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
    let status = run.pending_job().and_then(|job| sched.get_status(job).ok());
    println!("{}", name.bold());
    println!("  state:       {}", state_line(&run, status, run.lock_live()?));
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
/// be another host's). `stop` and `delete` leave such a run alone, as
/// `build stop` leaves a foreground build: it is interrupted where it runs.
fn running_without_job(run: &TestRun) -> Res<bool> {
    Ok(run.pending_job().is_none() && run.lock_live()?)
}

/// What `stop` and `delete` say of a run [`running_without_job`]: an
/// interactive one is interrupted where it runs; a submitted one was
/// stopped (its lock goes when it has ended) or runs where this scheduler
/// cannot see it.
fn without_job(run: &TestRun, name: &str) -> String {
    if run.meta.timestamps.submitted.is_some() {
        format!(
            "test run \"{name}\" was submitted and its job was stopped, but its running.lock is \
             still held: it is ending, or it ended without letting go of the lock (which then \
             expires within {} minutes)",
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
/// held though the scheduler here does not list its job: no more than
/// that is known (on a batch scheduler, which sees every node, a query
/// that failed or a run that ended without letting go of its lock).
fn running_elsewhere(run: &TestRun, name: &str) -> String {
    format!(
        "test run \"{name}\" has its running.lock held, though the scheduler here does not list its \
         job {}: it runs where this scheduler cannot see it (another host), the scheduler could not \
         be asked, or it ended without letting go of the lock (which then expires within {} \
         minutes); cactup leaves it as it is: stop it where it runs, or try again later",
        run.meta.job_id,
        crate::lock::LOCK_STALE_SECS / 60
    )
}

/// `test stop` (§11.7): the §8.6 stop semantics for the active result
/// set's job; no clean beyond removing the active symlink.
pub fn stop(ctx: &Ctx, name: &str, _force: bool) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let inst = Installation::resolve(ctx)?;
    let mut run = TestRun::locate(&inst, name)?;
    let _lock = run.lock()?;
    stop_run(&mut run, &Scheduler::new(&machine.meta), name)
}

/// [`stop`], on `run` (locked), by `sched`.
fn stop_run(run: &mut TestRun, sched: &Scheduler, name: &str) -> Res<()> {
    if running_without_job(run)? {
        println!("{}", without_job(run, name));
        return Ok(());
    }
    let Some(job_id) = run.pending_job().map(str::to_owned) else {
        println!("Test run {} has no live job; nothing to stop", name.bold());
        return Ok(());
    };
    match sched.get_status(&job_id)? {
        // Listed: in a state cactup reads, or in another (`Error`: Torque's
        // exiting `E`, Slurm's suspended `S`), which is the scheduler's job
        // all the same and stops as one.
        JobStatus::Running | JobStatus::Queued | JobStatus::Holding | JobStatus::Error => {
            sched.stop(&job_id)?;
            println!("Stopped test job {}", job_id.bold());
            // Over, as far as cactup is concerned (as `build stop` records
            // an outcome): its job id is never asked of the scheduler
            // again, where it may be a pid (the generic machine's submit)
            // that another process gets. Only once the stop was sent: a
            // query that failed reads as "not in the queue" too.
            run.meta.timestamps.finished.get_or_insert_with(chrono::Utc::now);
            run.log("test-stop", &format!("stopped job {job_id}"));
        }
        _ if run.lock_live()? => {
            println!("{}", running_elsewhere(run, name));
            return Ok(());
        }
        _ => println!("Test job {job_id} is not in the queue; nothing to stop"),
    }
    run.meta.status = Some("U".to_owned());
    run.store_meta()?;
    Ok(())
}

/// Refuse to delete `run` while it runs, unless `force` and it is a job the
/// scheduler lists, which is then stopped.
fn clear_to_delete(run: &TestRun, sched: &Scheduler, name: &str, force: bool) -> Res<()> {
    if running_without_job(run)? {
        bail!("{}; delete it once it has ended", without_job(run, name));
    }
    let job_live = match run.pending_job() {
        // Listed, in whatever state (as `stop_run`).
        Some(job_id) => !matches!(sched.get_status(job_id)?, JobStatus::Unknown),
        None => false,
    };
    // Held, but not a job the scheduler here lists: not stopped or removed
    // from here, `-f` neither.
    if !job_live && run.lock_live()? {
        bail!("{}", running_elsewhere(run, name));
    }
    if let Some(job_id) = run.pending_job().filter(|_| job_live) {
        if !force {
            bail!(
                "test run \"{name}\" has a live job ({job_id}); `cactup test stop {name}` first, \
                 or use -f to stop and delete"
            );
        }
        sched.stop(job_id)?;
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
    clear_to_delete(&run, &Scheduler::new(&machine.meta), name, force)?;
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

    /// An interactive run's job id is the pid of the cactup that ran it:
    /// only while its `running.lock` is held is that pid still the run's.
    /// Finished or killed, it is never handed to the scheduler — its stop
    /// command would stop whatever process has the pid now. A submitted
    /// run's job is, until the run records its end.
    #[test]
    fn an_interactive_runs_pid_is_a_job_only_while_its_lock_is_held() {
        let tmp = tempfile::tempdir().unwrap();
        let open = |submitted: bool, finished: bool, job_id: &str| {
            let dir = tmp.path().join(format!("run-{submitted}-{finished}-{job_id}"));
            fs::create_dir_all(dir.join(".cactup")).unwrap();
            let mut timestamps = String::new();
            if submitted {
                timestamps.push_str("submitted = \"2026-10-16T00:00:00Z\"\n");
            }
            if finished {
                timestamps.push_str("finished = \"2026-10-16T01:00:00Z\"\n");
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
            assert_eq!(run.pending_job(), None);
            assert!(!running_without_job(&run).unwrap());
        }
        // Running now, interactively: never handed to the scheduler (its pid
        // is no job the scheduler knows, and the lock may be another
        // host's); `stop` stops nothing and leaves the run as it is.
        let mut live = open(false, false, &pid);
        let _held = crate::lock::LinkLock::acquire(&live.running_lock_path()).unwrap();
        assert_eq!(live.pending_job(), None);
        assert!(running_without_job(&live).unwrap());

        assert_eq!(open(true, false, "JOB-1").pending_job(), Some("JOB-1"));
        assert_eq!(open(true, true, "JOB-1").pending_job(), None);
        assert_eq!(open(true, false, NO_JOB_ID).pending_job(), None);

        // A submitted run `test stop` stopped is over: its job id (on the
        // generic machine, a pid) is not asked of the scheduler again.
        let stopped = tmp.path().join("stopped");
        let meta: Meta = toml::from_str(&format!(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            get-status = "echo '@JOB_ID@ R'"
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
        assert_eq!(run.pending_job(), None);
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
        assert!(!running_without_job(&elsewhere).unwrap());
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
        assert_eq!(TestRun::open(&queried.dir).unwrap().pending_job(), Some("JOB-4"));

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
        assert!(clear_to_delete(&ending, &exiting, "t", false).unwrap_err().to_string().contains("has a live job (JOB-6)"));
        stop_run(&mut ending, &exiting, "t").unwrap();
        assert!(stopped.exists());
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
        let statuses = Scheduler::new(&meta).get_statuses(&["1", "2", "1"]);
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
