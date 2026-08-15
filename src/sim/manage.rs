//! `sim stop` / `clean` (§8.6), `sim delete` (§8.7), `sim list` / `sim show`
//! (including `sim show --output-dir`), and `sim log`.

use crate::commands::Ctx;
use crate::installation::{Installation, SimEntry};
use crate::scheduler::{display_state, DisplayState, JobStatus, Scheduler};
use crate::sim::restart::{self, Restart, NO_JOB_ID};
use crate::sim::{cache, Simulation};
use crate::Res;
use anyhow::{bail, Context};
use colored::Colorize;
use std::collections::HashMap;
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

    // The lock comes BEFORE the live-job guard: submit holds it for the whole
    // submission (§2.3 item 3), so taking it first closes the window where a
    // concurrent submit queues a job between our scan and the removal.
    let _lock = sim.lock()?;

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

/// One simulation's derived display state (§8.6, §10) with the live scheduler
/// query held back: [`SimState::state`] is exact once the queried status is
/// folded in, and [`SimState::job_to_query`] names the only job whose status
/// can still change the answer. Splitting the query out is what lets `sim list`
/// resolve a whole registry in one batched round of scheduler calls instead of
/// one call per simulation.
struct SimState {
    /// The restart the state is derived from: the active one, else the latest.
    subject: Option<u32>,
    job: Option<String>,
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
    /// queue says, so a long finished history costs no scheduler calls.
    fn job_to_query(&self) -> Option<&str> {
        if self.forced.is_some() || !(self.active || self.chained) {
            return None;
        }
        self.job.as_deref()
    }

    /// The derived state, given the live status of [`SimState::job_to_query`]
    /// (`None` when there was nothing to query, or the query failed).
    fn state(&self, status: Option<JobStatus>) -> DisplayState {
        self.forced
            .unwrap_or_else(|| display_state(self.active, status, self.chained, self.terminated))
    }
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
        job: (r.meta.job_id != NO_JOB_ID).then(|| r.meta.job_id.clone()),
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
    let status = st.job_to_query().and_then(|job| sched.get_status(job).ok());
    (st.state(status), st.subject, st.job)
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
    fn job_to_query(&self) -> Option<&str> {
        match self {
            Row::Listed { state, .. } => state.job_to_query(),
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

fn print_row(name: &str, row: &Row, statuses: &HashMap<String, JobStatus>, long: bool) {
    match row {
        Row::Missing(dir) => println!(
            "  {:24} {:12} {}",
            name.bold(),
            "MISSING".red(),
            format!("{} (prune with `cactup sim delete {name}`)", dir.display())
        ),
        Row::Broken(e) => println!("  {:24} {:12} {e}", name.bold(), "BROKEN".red()),
        Row::Listed { config, restarts, dir, state } => {
            let status = state.job_to_query().and_then(|job| statuses.get(job).copied());
            let mut extra = format!("config {config}, {restarts} restart(s)");
            if let Some(job) = &state.job {
                extra.push_str(&format!(", job {job}"));
            }
            if long {
                if let Some(s) = state.subject {
                    extra.push_str(&format!(", latest {}", restart::dir_name(s)));
                }
                extra.push_str(&format!(", {}", dir.display()));
            }
            println!("  {:24} {:12} {}", name.bold(), state_str(state.state(status)), extra);
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
    let pending: Vec<&str> = rows.iter().filter_map(Row::job_to_query).collect();
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
            print_row(name, row, &statuses, long);
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
    let queries: Vec<&str> = loaded
        .iter()
        .filter_map(|r| r.as_ref().ok())
        .map(|r| r.meta.job_id.as_str())
        .filter(|job| *job != NO_JOB_ID)
        .collect();
    let statuses = sched.get_statuses(&queries);

    for (&id, loaded) in scan.ids.iter().zip(&loaded) {
        let marker = if active == Some(id) { " (active)" } else { "" };
        match loaded {
            Ok(r) => {
                let status = statuses.get(&r.meta.job_id).copied();
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
        let mut meta = SimulationMeta::default();
        meta.parfile = "bbh.par".to_owned();
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

        let pending: Vec<&str> = rows.iter().filter_map(Row::job_to_query).collect();
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
        assert_eq!(live_state.state(Some(JobStatus::Running)), DisplayState::Running);
        assert_eq!(
            live_state.state(None),
            DisplayState::Active,
            "query failed or never ran, but the restart is still active"
        );

        let hist_state = listed_state(&rows[1]);
        assert_eq!(
            hist_state.state(None),
            DisplayState::Inactive,
            "finished history is INACTIVE whatever the queue says"
        );

        let chain_state = listed_state(&rows[2]);
        assert_eq!(chain_state.state(Some(JobStatus::Queued)), DisplayState::Presubmitted);

        let nojob_state = listed_state(&rows[3]);
        assert_eq!(nojob_state.state(None), display_state(true, None, false, false));
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
                [variants.submitscript]
                "default" = ["local"]
                [variants.runscript]
                "default" = ["local"]
                [variants.optionlist]
                variants = ["default"]
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
        let pending = ["10", "20", "10", "30"];
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
