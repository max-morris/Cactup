//! `sim stop` / `clean` (§8.6), `sim delete` (§8.7), `sim list` / `sim show`
//! (including `sim show --output-dir`), and `sim log`.

use crate::commands::Ctx;
use crate::installation::Installation;
use crate::scheduler::{display_state, DisplayState, JobStatus, Scheduler};
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

/// `sim stop` (§8.6): graceful via the `TERMINATE` trigger when present;
/// forced (or `-f`) via the machine `stop` command. Both finish the restart.
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
    } else {
        if r.meta.job_id != NO_JOB_ID {
            let sched = Scheduler::new(&machine.meta);
            sched.stop(&r.meta.job_id)?;
            println!("Stopped job {} via the scheduler", r.meta.job_id.bold());
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
    if let Ok(mut r) = Restart::load(&sim.dir, id) {
        if !r.meta.terminated {
            r.meta.terminated = true;
            r.meta.finished = Some(chrono::Utc::now());
            let _ = r.store();
        }
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
            if fs::hard_link(&theirs, &tmp).is_ok() {
                if fs::rename(&tmp, &ours).is_err() {
                    let _ = fs::remove_file(&tmp);
                }
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

    // Live-job guard (§8.7).
    let mut live: Vec<(u32, String)> = Vec::new();
    for id in restart::list_ids(&sim.dir)? {
        let Ok(r) = Restart::load(&sim.dir, id) else { continue };
        if r.meta.job_id == NO_JOB_ID || r.meta.terminated {
            continue;
        }
        if matches!(
            sched.get_status(&r.meta.job_id)?,
            JobStatus::Running | JobStatus::Queued | JobStatus::Holding
        ) {
            live.push((id, r.meta.job_id.clone()));
        }
    }
    if !live.is_empty() {
        if !force {
            bail!(
                "simulation \"{name}\" has live jobs ({}); `cactup sim stop {name}` first, \
                 or use -f to stop and delete",
                live.iter()
                    .map(|(id, job)| format!("{} = job {job}", restart::dir_name(*id)))
                    .collect::<Vec<_>>()
                    .join(", ")
            );
        }
        for (_, job) in &live {
            sched.stop(job).with_context(|| format!("stopping job {job}"))?;
        }
    }

    let _lock = sim.lock()?;
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

/// One simulation's derived display state (§8.6, §10).
fn sim_state(sim: &Simulation, sched: &Scheduler) -> (DisplayState, Option<u32>, Option<String>) {
    let Ok(ids) = restart::list_ids(&sim.dir) else {
        return (DisplayState::Inactive, None, None);
    };
    let active = restart::active_id(&sim.dir).ok().flatten();

    // The restart that determines the state: the active one, else the latest.
    let subject = active.or_else(|| ids.last().copied());
    let Some(subject) = subject else { return (DisplayState::Inactive, None, None) };
    let Ok(r) = Restart::load(&sim.dir, subject) else {
        return (DisplayState::Inactive, Some(subject), None);
    };

    let status = if r.meta.job_id == NO_JOB_ID {
        None
    } else {
        sched.get_status(&r.meta.job_id).ok()
    };
    let state = display_state(
        active.is_some(),
        status,
        r.meta.chained_job_id.is_some(),
        r.meta.terminated,
    );
    let job = (r.meta.job_id != NO_JOB_ID).then(|| r.meta.job_id.clone());
    (state, Some(subject), job)
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

/// `sim list` (§8.1, §8.6): list the registry; `--all` unions every
/// installation's registry.
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

    let mut any = false;
    for inst in &installations {
        let registry = inst.simulations()?;
        if registry.simulations.is_empty() {
            continue;
        }
        if all {
            println!("{}", format!("[{}]", inst.alias).bold());
        }
        for (name, entry) in &registry.simulations {
            any = true;
            if !entry.dir.is_dir() {
                println!(
                    "  {:24} {:12} {}",
                    name.bold(),
                    "MISSING".red(),
                    format!("{} (prune with `cactup sim delete {name}`)", entry.dir.display())
                );
                continue;
            }
            match Simulation::open(name, &entry.dir) {
                Ok(sim) => {
                    let (state, subject, job) = sim_state(&sim, &sched);
                    let restarts = restart::list_ids(&sim.dir).map(|v| v.len()).unwrap_or(0);
                    let mut extra = format!("config {}, {} restart(s)", entry.config, restarts);
                    if let Some(job) = job {
                        extra.push_str(&format!(", job {job}"));
                    }
                    if long {
                        if let Some(s) = subject {
                            extra.push_str(&format!(", latest {}", restart::dir_name(s)));
                        }
                        extra.push_str(&format!(", {}", sim.dir.display()));
                    }
                    println!("  {:24} {:12} {}", name.bold(), state_str(state), extra);
                }
                Err(e) => println!("  {:24} {:12} {e:#}", name.bold(), "BROKEN".red()),
            }
        }
    }
    if !any {
        println!("No simulations (create one with `cactup sim create <name> <parfile>`)");
    }
    Ok(())
}

fn show_one(inst: &Installation, sched: &Scheduler, name: &str, long: bool) -> Res<()> {
    let sim = Simulation::locate(inst, name)?;
    let (state, _, _) = sim_state(&sim, sched);

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

    let active = restart::active_id(&sim.dir)?;
    let ids = restart::list_ids(&sim.dir)?;
    if ids.is_empty() {
        println!("  restarts:      none");
        return Ok(());
    }
    println!("  restarts:");
    for id in ids {
        let marker = if active == Some(id) { " (active)" } else { "" };
        match Restart::load(&sim.dir, id) {
            Ok(r) => {
                let status = if r.meta.job_id == NO_JOB_ID {
                    None
                } else {
                    sched.get_status(&r.meta.job_id).ok()
                };
                let state = display_state(
                    active == Some(id),
                    status,
                    r.meta.chained_job_id.is_some(),
                    r.meta.terminated,
                );
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
                        if s == JobStatus::Running {
                            if let Ok(Some(host)) = sched.exec_host(&r.meta.job_id) {
                                line.push_str(&format!(", host {host}"));
                            }
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
/// With `follow`, keep streaming newly-appended bytes (`tail -f`) until Ctrl-C.
pub fn log_cmd(ctx: &Ctx, name: &str, follow: bool) -> Res<()> {
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
    crate::tail::tail_log(&sources, follow, &subject)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sim::SimulationMeta;

    fn sim_at(dir: &Path) -> Simulation {
        let mut meta = SimulationMeta::default();
        meta.parfile = "bbh.par".to_owned();
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
}
