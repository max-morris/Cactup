//! `test sim show` / `stop` / `delete` (§11.7): managing test runs. Coarser
//! than the sim analogues — no restart chain, no clean, no reaper.

use crate::commands::Ctx;
use crate::installation::Installation;
use crate::scheduler::{JobStatus, Scheduler};
use crate::sim::cache;
use crate::sim::restart::NO_JOB_ID;
use crate::testsuite::{active_results_id, list_results_ids, results_name, TestRun};
use crate::Res;
use anyhow::bail;
use colored::Colorize;
use std::fs;

/// The §11.7 display state, coarser than a sim's.
fn state_line(run: &TestRun, sched: &Scheduler) -> colored::ColoredString {
    let status = if run.meta.job_id == NO_JOB_ID {
        None
    } else {
        sched.get_status(&run.meta.job_id).ok()
    };
    match status {
        Some(JobStatus::Running) => "RUNNING".green().bold(),
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

pub fn show(ctx: &Ctx, name: Option<&str>, long: bool, all: bool) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let sched = Scheduler::new(&machine.meta);

    if let Some(name) = name {
        let inst = Installation::resolve(ctx)?;
        return show_one(&inst, &sched, name);
    }

    let installations: Vec<Installation> = if all {
        let db = ctx.db.read()?;
        db.installations
            .values()
            .map(|i| Installation::new(i.alias.clone(), i.path.clone()))
            .collect()
    } else {
        vec![Installation::resolve(ctx)?]
    };

    let mut any = false;
    for inst in &installations {
        let registry = inst.tests()?;
        if registry.tests.is_empty() {
            continue;
        }
        if all {
            println!("{}", format!("[{}]", inst.alias).bold());
        }
        for (name, entry) in &registry.tests {
            any = true;
            if !entry.dir.is_dir() {
                println!(
                    "  {:24} {:12} {} (prune with `cactup test sim delete {name}`)",
                    name.bold(),
                    "MISSING".red(),
                    entry.dir.display()
                );
                continue;
            }
            match TestRun::open(&entry.dir) {
                Ok(run) => {
                    let sets = list_results_ids(&run.dir).map(|v| v.len()).unwrap_or(0);
                    let mut extra =
                        format!("test-config {}, {} result set(s)", entry.test_config, sets);
                    if long {
                        extra.push_str(&format!(", job {}, {}", run.meta.job_id, run.dir.display()));
                    }
                    println!("  {:24} {:24} {}", name.bold(), state_line(&run, &sched), extra);
                }
                Err(e) => println!("  {:24} {:12} {e:#}", name.bold(), "BROKEN".red()),
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
    println!("{}", name.bold());
    println!("  state:       {}", state_line(&run, sched));
    println!("  directory:   {}", run.dir.display());
    println!("  test-config: {} (build-id {})", run.meta.test_config, run.meta.build_id);
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

/// `test sim stop` (§11.7): the §8.6 stop semantics for the active result
/// set's job; no clean beyond removing the active symlink.
pub fn stop(ctx: &Ctx, name: &str, _force: bool) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let inst = Installation::resolve(ctx)?;
    let mut run = TestRun::locate(&inst, name)?;
    let _lock = run.lock()?;

    if run.meta.job_id == NO_JOB_ID {
        println!("Test run {} has no job; nothing to stop", name.bold());
        return Ok(());
    }
    let sched = Scheduler::new(&machine.meta);
    match sched.get_status(&run.meta.job_id)? {
        JobStatus::Running | JobStatus::Queued | JobStatus::Holding => {
            sched.stop(&run.meta.job_id)?;
            println!("Stopped test job {}", run.meta.job_id.bold());
        }
        _ => println!("Test job {} is not in the queue; nothing to stop", run.meta.job_id),
    }
    run.meta.status = Some("U".to_owned());
    run.store_meta()?;
    run.log("test-stop", &format!("stopped job {}", run.meta.job_id));
    Ok(())
}

/// `test sim delete` (§11.7): §8.7 semantics against test-home. `--purge`
/// (or `-f`) removes outright; the default trashes into
/// `<test-home>/TRASH/<test-run-id>/`.
pub fn delete(ctx: &Ctx, name: &str, force: bool, purge: bool) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let inst = Installation::resolve(ctx)?;
    let test_home = inst.meta()?.test_home()?.to_owned();

    let registry = inst.tests()?;
    let Some(entry) = registry.tests.get(name) else {
        bail!("no test run named \"{name}\" (see `cactup test sim show`)");
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
    let sched = Scheduler::new(&machine.meta);
    if run.meta.job_id != NO_JOB_ID
        && matches!(
            sched.get_status(&run.meta.job_id)?,
            JobStatus::Running | JobStatus::Queued | JobStatus::Holding
        )
    {
        if !force {
            bail!(
                "test run \"{name}\" has a live job ({}); `cactup test sim stop {name}` first, \
                 or use -f to stop and delete",
                run.meta.job_id
            );
        }
        sched.stop(&run.meta.job_id)?;
    }

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
