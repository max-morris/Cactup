//! `test list` / `show` / `stop` / `delete` (§11.7): managing test runs.
//! Coarser than the sim analogues — no restart chain, no clean, no reaper.

use crate::commands::Ctx;
use crate::installation::Installation;
use crate::scheduler::{JobStatus, Scheduler};
use crate::sim::cache;
use crate::sim::restart::NO_JOB_ID;
use crate::testsuite::{active_results_id, list_results_ids, results_name, TestRun};
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::fs;
use std::path::PathBuf;

/// The §11.7 display state, coarser than a sim's. `status` is the
/// already-resolved live status (`None` for no job / a failed query) —
/// callers resolve it themselves so `list` can batch the query across every
/// row instead of round-tripping the scheduler once per row.
fn state_line(run: &TestRun, status: Option<JobStatus>) -> colored::ColoredString {
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
            Row::Listed { run, .. } if run.meta.job_id != NO_JOB_ID => Some(run.meta.job_id.as_str()),
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
                    let status = if run.meta.job_id != NO_JOB_ID {
                        statuses.get(&run.meta.job_id).copied()
                    } else {
                        None
                    };
                    println!("  {:24} {:24} {}", name.bold(), state_line(&run, status), extra);
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
    let status = if run.meta.job_id != NO_JOB_ID {
        sched.get_status(&run.meta.job_id).ok()
    } else {
        None
    };
    println!("{}", name.bold());
    println!("  state:       {}", state_line(&run, status));
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

/// `test stop` (§11.7): the §8.6 stop semantics for the active result
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
    let sched = Scheduler::new(&machine.meta);
    if run.meta.job_id != NO_JOB_ID
        && matches!(
            sched.get_status(&run.meta.job_id)?,
            JobStatus::Running | JobStatus::Queued | JobStatus::Holding
        )
    {
        if !force {
            bail!(
                "test run \"{name}\" has a live job ({}); `cactup test stop {name}` first, \
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
    use crate::mdb::meta::Meta;
    use std::path::Path;

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
            [variants.submitscript]
            "default" = ["local"]
            [variants.runscript]
            "default" = ["local"]
            [variants.optionlist]
            variants = ["default"]
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
