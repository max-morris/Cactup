//! `sim submit` (§8.3) and `sim run` (§8.4): restart preparation, script
//! generation, universes (§4.8), auto-recovery + walltime chaining (§8.8),
//! and the compute-node path (§8.3.1).

use crate::args::{SimRunArgs, SimStartArgs, UniverseFlags};
use crate::build::ConfigMeta;
use crate::commands::build as build_cmd;
use crate::commands::Ctx;
use crate::database::Database;
use crate::installation::Installation;
use crate::lock::{LinkLock, HEARTBEAT_SECS};
use crate::mdb::{discover, Machine, Phase, ScriptKind, Universe, WrappedCommand, HOST_UNIVERSE};
use crate::scheduler::Scheduler;
use crate::sim::restart::{self, Restart, RestartMeta, UniverseSpec, NO_JOB_ID};
use crate::sim::{vars, Simulation};
use crate::template::{Syntax, VarSet};
use crate::Res;
use anyhow::{anyhow, bail, Context};
use chrono::Utc;
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Locate `<sim>`, or create it when a parfile makes that meaningful — the
/// `create-submit` fusion (§8.3). An existing sim + a parfile is an error
/// unless `--overwrite`/`-f` (never silently ignore a contradicting parfile).
fn obtain_sim(
    ctx: &Ctx,
    machine: &Machine,
    inst: &Installation,
    args: &SimStartArgs,
) -> Res<Simulation> {
    let exists = inst.simulations()?.simulations.contains_key(&args.sim);
    match (&args.parfile, exists) {
        (None, true) => Simulation::locate(inst, &args.sim),
        // §8.3
        (None, false) => bail!(
            "No simulation named \"{}\"; pass a parfile to create it",
            args.sim
        ),
        (Some(par), false) => crate::sim::create(
            ctx,
            machine,
            inst,
            &crate::sim::CreateRequest {
                force: false,
                ignore_machine: args.start_flags.ignore_machine || args.force,
                name: &args.sim,
                parfile: par,
                config: args.config.as_deref(),
                sim_dir: None,
            },
        ),
        (Some(par), true) => {
            if !(args.overwrite || args.force) {
                bail!(
                    "Simulation \"{}\" already exists but a parfile was given; \
                     use --overwrite (or -f) to replace it",
                    args.sim
                );
            }
            crate::sim::create(
                ctx,
                machine,
                inst,
                &crate::sim::CreateRequest {
                    force: true,
                    ignore_machine: args.start_flags.ignore_machine || args.force,
                    name: &args.sim,
                    parfile: par,
                    config: args.config.as_deref(),
                    sim_dir: None,
                },
            )
        }
    }
}

/// Resolve the RUN universe per the §4.8 precedence:
/// CLI → coerced build universe → the runscript's own `universe` →
/// declared host → none (a machine without `[universes.host]` keeps
/// resolving to none: implicit host ≡ identity ≡ bare execution).
pub fn resolve_run_universe<'m>(
    machine: &'m Machine,
    cfg: &ConfigMeta,
    cli: &UniverseFlags,
    script_universe: Option<&str>,
    verbose: bool,
) -> Res<Option<(String, &'m Universe)>> {
    if cli.no_universe {
        return Ok(None);
    }
    if let Some(name) = &cli.universe {
        return Ok(Some((name.clone(), machine.meta.universe(name)?)));
    }
    if let Some(build_uni) = &cfg.universe && cfg.coerce_run_universe {
        // §4.8
        let u = machine.meta.universe(build_uni).with_context(|| {
            format!(
                "config \"{}\" was built in universe \"{build_uni}\", which this machine no \
                 longer defines; pass --no-universe or rebuild",
                cfg.name
            )
        })?;
        if verbose && let Some(v) = script_universe && v != build_uni {
            // §4.8
            eprintln!(
                "{} runscript names universe \"{v}\" but the config's build \
                 universe \"{build_uni}\" takes precedence",
                "note:".yellow()
            );
        }
        return Ok(Some((build_uni.clone(), u)));
    }
    match script_universe {
        Some(n) => Ok(Some((n.to_owned(), machine.meta.universe(n)?))),
        None => Ok(machine.meta.declared_host().map(|u| (HOST_UNIVERSE.to_owned(), u))),
    }
}

/// Resolve the SUBMIT universe: the submitscript's own `universe` →
/// declared host → none (the CLI flag targets the run universe — §8.3).
pub(crate) fn resolve_submit_universe<'m>(
    machine: &'m Machine,
    script_universe: Option<&str>,
) -> Res<Option<(String, &'m Universe)>> {
    match script_universe {
        Some(n) => Ok(Some((n.to_owned(), machine.meta.universe(n)?))),
        None => Ok(machine.meta.declared_host().map(|u| (HOST_UNIVERSE.to_owned(), u))),
    }
}

/// `ConfigMeta::load` found nothing for `config` — almost always because it
/// was deleted, but check for an in-flight build first (§7.9): a config
/// delete guard closes the ordinary race, but this stays the accurate thing
/// to say if metadata is ever unreadable for some other reason while a
/// build is running. Shared by `submit_impl`/`run_interactive` below.
fn config_missing_error(cactus_root: &Path, machine: &Machine, config: &str) -> anyhow::Error {
    let config_dir = cactus_root.join("configs").join(config);
    if let Some((phrase, stop)) = build_cmd::in_flight_build_or_stop(&config_dir, config, Some(machine)) {
        return anyhow!("{phrase}; wait for it to finish{stop} (see `cactup build show {config}`)");
    }
    anyhow!("Config \"{config}\" no longer exists")
}

/// Insert the phase's effective env-setup into a substituted `.sh` script
/// (§6.1 auto-prepend). Runscripts: right after the shebang. Submitscripts:
/// after the leading run of `#`-or-blank lines (shebang + #SBATCH/#PBS
/// directives; blank lines count as header so a directive block with a blank
/// line in it is not split — scheduler directives must not be preceded by
/// executable lines), appending when the whole script is header.
fn prepend_env(script: &str, env: &str, kind: ScriptKind) -> String {
    if env.is_empty() {
        return script.to_owned();
    }
    match kind {
        ScriptKind::Run => match script.strip_prefix("#!") {
            Some(rest) => match rest.split_once('\n') {
                Some((shebang_rest, body)) => format!("#!{shebang_rest}\n{env}\n{body}"),
                None => format!("{script}\n{env}\n"),
            },
            None => format!("{env}\n{script}"),
        },
        ScriptKind::Submit | ScriptKind::BuildSubmit => {
            let mut idx = 0; // byte offset of the first substantive line
            for line in script.split_inclusive('\n') {
                let t = line.trim();
                if t.is_empty() || t.starts_with('#') {
                    idx += line.len();
                } else {
                    break;
                }
            }
            if idx == script.len() {
                let sep = if script.is_empty() || script.ends_with('\n') { "" } else { "\n" };
                format!("{script}{sep}{env}\n")
            } else {
                format!("{}{env}\n{}", &script[..idx], &script[idx..])
            }
        }
    }
}

/// Generate one script artifact for a restart or test run (§6.1/§6.2): `.sh`
/// scripts are `@NAME@`-substituted with the phase env — under the phase's
/// resolved `universe`, whose env keys override the machine's (§6.1) —
/// auto-prepended; `.py` scripts run per the calling convention and own
/// their env placement.
pub(crate) fn generate_script(
    machine: &Machine,
    kind: ScriptKind,
    script_name: &str,
    vars: &VarSet,
    phase: Phase,
    universe: Option<&str>,
) -> Res<String> {
    let script = machine.script_path(kind, script_name)?;
    if script.python {
        vars.run_py_script(&script.path)
    } else {
        let template = fs::read_to_string(&script.path)
            .with_context(|| format!("Failed to read '{}'", script.path.display()))?;
        let substituted = vars
            .substitute(&template, Syntax::Shell)
            .with_context(|| format!("Substituting '{}'", script.path.display()))?;
        Ok(prepend_env(&substituted, &machine.meta.phase_env(universe, phase), kind))
    }
}

pub(crate) fn write_executable(path: &Path, content: &str) -> Res<()> {
    fs::write(path, content).with_context(|| format!("Failed to write '{}'", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
}

/// Dry-run a `.par` parfile against the variable set at submit time (§6.2):
/// a stray `@`, an unknown variable or a required `@KNOB(…)@` that is unset
/// is reported now, before the job is queued. `@ENV(…)@` is exempt — only
/// the compute node's environment can judge it — and a `.py` parfile is not
/// checked (its logic runs at run time). The real substitution happens in
/// `resolve_parfile`, on the compute node.
fn check_parfile(sim: &Simulation, vars: &VarSet) -> Res<()> {
    if sim.is_python_parfile() {
        return Ok(());
    }
    let master = sim.master_parfile();
    let template = fs::read_to_string(&master)
        .with_context(|| format!("Failed to read parfile master copy '{}'", master.display()))?;
    vars.check(&template, Syntax::Parfile)
        .with_context(|| format!("Parfile '{}' cannot be substituted", master.display()))
}

/// Resolve the parfile into the restart (§6.2/§8.4): `.par` is
/// `@NAME@`-substituted; `.py` is invoked per §6.1 and its stdout used as-is.
/// Returns the ready-to-run parfile path.
fn resolve_parfile(sim: &Simulation, restart_dir: &Path, vars: &VarSet) -> Res<PathBuf> {
    let master = sim.master_parfile();
    let out = restart_dir.join(format!("{}.par", sim.parfile_stem()));
    let content = if sim.is_python_parfile() {
        vars.run_py_script(&master)?
    } else {
        let template = fs::read_to_string(&master)
            .with_context(|| format!("Failed to read parfile master copy '{}'", master.display()))?;
        vars.substitute(&template, Syntax::Parfile)
            .with_context(|| format!("Substituting parfile '{}'", master.display()))?
    };
    fs::write(&out, content).with_context(|| format!("Failed to write '{}'", out.display()))?;
    Ok(out)
}

/// Identity inputs shared by every segment of one submit/run invocation
/// (also used by the test stream).
pub(crate) struct Identity {
    pub(crate) hostname: String,
    pub(crate) user: String,
    pub(crate) email: String,
}

impl Identity {
    pub(crate) fn resolve(db: &Database, hostname_override: Option<&str>) -> Identity {
        Identity {
            hostname: discover::resolve_hostname(hostname_override),
            user: db.knob_or_default("user").unwrap_or_else(|| "unknown".to_owned()),
            email: db.knob_or_default("email").unwrap_or_default(),
        }
    }
}

/// `sim submit` (§8.3).
pub fn submit(ctx: &Ctx, args: SimStartArgs) -> Res<()> {
    let machine = crate::commands::machine::resolve(ctx)?;
    let inst = Installation::resolve(ctx)?;
    let db = ctx.db.read()?;
    let sim = obtain_sim(ctx, &machine, &inst, &args)?;
    submit_impl(
        &inst,
        &machine,
        &db,
        &sim,
        &args,
        ctx.globals.verbose,
        ctx.globals.hostname.as_deref(),
    )
}

fn submit_impl(
    inst: &Installation,
    machine: &Machine,
    db: &Database,
    sim: &Simulation,
    args: &SimStartArgs,
    verbose: bool,
    hostname_override: Option<&str>,
) -> Res<()> {
    let cactus_root = inst.cactus_root();
    let cfg = ConfigMeta::load(&cactus_root, &sim.meta.configuration)?
        .ok_or_else(|| config_missing_error(&cactus_root, machine, &sim.meta.configuration))?;
    check_sim_machine(machine, sim, &cfg, args.start_flags.ignore_machine || args.force)?;
    crate::commands::delta::warn_if_sources_diverged(inst, &cfg, args.silent)?;
    let force_queue = args.start_flags.force_queue || args.force;
    let fit = vars::BinaryFit::for_config(machine, &cfg)?;
    let placement = vars::Placement::submit(machine);
    let mut topo =
        vars::resolve_topology(&args.topology, machine, db, Some(&fit), force_queue, placement)?;
    // The effective knobs — global, then the simulation's own, then the
    // command line's (`-K`, and `-a` as `-K allocation=`) — frozen with the
    // vars so the compute node resolves @KNOB(…)@ without the DB (§5, D11).
    // @ALLOCATION@ is an alias of the allocation knob in it.
    let knobs = sim.knob_snapshot(db);
    topo.allocation = knobs.get("allocation").filter(|a| !a.is_empty()).cloned();
    let sim_home = inst.meta()?.sim_home()?.to_owned();
    let sched = Scheduler::for_machine(machine);

    // Per-simulation mutual exclusion for everything below (§2.3 item 3).
    let _lock = sim.lock()?;

    // Reap a stale active restart (§8.3); a live one triggers chaining.
    restart::reap_stale(sim, &sched)?;
    let mut prev_job: Option<String> = None;
    let mut activate_first = true;
    if let Some(active) = restart::active_id(&sim.dir)? {
        let r = Restart::load(&sim.dir, active)?;
        // A foreground run's pid is no job a batch scheduler could chain
        // behind (§8.6): wait for it, or interrupt it.
        if r.meta.foreground && r.meta.scheduler_job(&sched).is_none() {
            match LinkLock::is_held_live(&r.running_lock_path())? {
                true => bail!(
                    "Restart {} of simulation \"{}\" runs in the foreground (pid {}) and cannot be \
                     chained behind; wait for it to end or interrupt it (Ctrl-C), then submit",
                    restart::dir_name(active),
                    sim.name,
                    r.meta.job_id
                ),
                false => bail!(
                    "Restart {} of simulation \"{}\" ran in the foreground and is still active, but \
                     no run holds its lock; run `cactup sim clean {}`, then submit",
                    restart::dir_name(active),
                    sim.name,
                    sim.name
                ),
            }
        }
        if r.meta.job_id == NO_JOB_ID {
            bail!(
                "Restart {} is active but has no job and is not reapable yet; \
                 try again shortly or `cactup sim stop {}`",
                restart::dir_name(active),
                sim.name
            );
        }
        let chain = match r.meta.terminated {
            // Not reaped: its job is listed, or the reaper's throttles kept
            // it from asking; either way the run has not recorded its end.
            false => {
                println!(
                    "Restart {} has not recorded its end (job {}); chaining the new submission behind it",
                    restart::dir_name(active),
                    r.meta.job_id
                );
                true
            }
            // An ended restart still active: the reaper left it, because the
            // scheduler still lists its job (§8.6), or under its throttles,
            // without asking. Asked here, so what is said is known; the safe
            // reading for new work: chained behind a job still listed;
            // refused beside one the scheduler could not be asked about
            // (chaining behind a job it has forgotten fails), unless `-f`
            // stops it first.
            true => {
                use crate::scheduler::AfterEnd;
                let dir = restart::dir_name(active);
                let job_id = &r.meta.job_id;
                let answer = crate::sim::manage::ask_ended(sim, &sched, &r)?;
                if let Some(AfterEnd::Unanswered(failed)) = &answer {
                    if !args.force {
                        return Err(crate::sim::manage::unanswered_refusal(sim, &r, failed, "submit"));
                    }
                    crate::sim::manage::stop_unanswered(sim, &sched, &r, failed)?;
                }
                match answer {
                    Some(AfterEnd::Listed(status)) => {
                        println!(
                            "Restart {dir} has ended but the scheduler still lists its job {job_id} ({}); \
                             chaining the new submission behind it",
                            crate::scheduler::listed_state_words(status)
                        );
                        true
                    }
                    // Stopped by `-f`, but its run still ending: no chaining
                    // behind a job the scheduler may have forgotten.
                    Some(AfterEnd::Unanswered(_)) if LinkLock::is_held_live(&r.running_lock_path())? => bail!(
                        "Restart {dir} of simulation \"{}\" is still shutting down; wait for it, then submit",
                        sim.name
                    ),
                    _ if LinkLock::is_held_live(&r.running_lock_path())? => {
                        println!(
                            "Restart {dir} is still shutting down; chaining the new submission behind its \
                             job {job_id}"
                        );
                        true
                    }
                    answer => {
                        // What the reaper does, but for its throttles.
                        crate::sim::manage::clean_active(sim)?;
                        match answer {
                            Some(AfterEnd::Over(crate::scheduler::JobStatus::Unknown)) => println!(
                                "Restart {dir} has ended and the scheduler no longer knows its job \
                                 {job_id}; cleaned it"
                            ),
                            Some(AfterEnd::Over(_)) => {
                                println!("Restart {dir} and its job {job_id} have ended; cleaned it")
                            }
                            _ => println!("Restart {dir} has ended; cleaned it"),
                        }
                        false
                    }
                }
            }
        };
        if chain {
            prev_job = Some(r.meta.job_id.clone());
            activate_first = false;
        }
    } else if let Some((ended, answer)) = crate::sim::manage::ended_newest(sim, &sched)? {
        // The newest ended restart (deactivated: `sim clean`, a stop): its
        // job still listed (queued again, or leaving the queue), the new
        // job is chained behind it, which waits for it either way (behind a
        // job that is just leaving, a moment). One the scheduler could not
        // be asked about refuses the submission (chaining behind a job it
        // has forgotten fails), unless `-f` stops it first.
        use crate::scheduler::AfterEnd;
        let (dir, job_id) = (restart::dir_name(ended.id), &ended.meta.job_id);
        match answer {
            Some(AfterEnd::Listed(status)) => {
                println!(
                    "Restart {dir} has ended but the scheduler still lists its job {job_id} ({}); \
                     chaining the new submission behind it",
                    crate::scheduler::listed_state_words(status)
                );
                prev_job = Some(ended.meta.job_id.clone());
                activate_first = false;
            }
            Some(AfterEnd::Unanswered(failed)) if !args.force => {
                return Err(crate::sim::manage::unanswered_refusal(sim, &ended, &failed, "submit"));
            }
            Some(AfterEnd::Unanswered(failed)) => crate::sim::manage::stop_unanswered(sim, &sched, &ended, &failed)?,
            _ => {}
        }
    }

    // Scripts for the chosen queue (§4.4) and universes (§4.8);
    // selection is driven by the config's BUILD universe (§4.4).
    let submit_scripts = machine.meta.scripts(ScriptKind::Submit);
    let run_scripts = machine.meta.scripts(ScriptKind::Run);
    let cfg_universe = cfg.universe.as_deref().unwrap_or(HOST_UNIVERSE);
    let (sub_script_name, sub_entry) = submit_scripts.route(&topo.queue, cfg_universe, false)?;
    let (run_script_name, run_entry) = run_scripts.route(&topo.queue, cfg_universe, false)?;
    // Script default tasks (§4.2): submitscript first for a submit.
    vars::apply_tasks_default(&mut topo, &args.topology, sub_entry.tasks.or(run_entry.tasks), None);
    if let Some(note) = topo.nodes_note() {
        println!("{note}");
    }
    let submit_uni = resolve_submit_universe(
        machine,
        sub_entry.universe.as_deref(),
    )?;
    let run_uni = resolve_run_universe(
        machine,
        &cfg,
        &args.universe,
        run_entry.universe.as_deref(),
        verbose,
    )?;
    let run_uni_spec = run_uni.as_ref().map(|(name, u)| UniverseSpec::from_universe(name, u));
    let run_uni_name = run_uni.as_ref().map(|(name, _)| name.as_str());
    let submit_uni_name = submit_uni.as_ref().map(|(name, _)| name.as_str());

    // Automatic walltime chaining (§8.8).
    let ceiling = machine.meta.effective_max_walltime(&topo.queue)?;
    let (segments, job_wall) = vars::chain_segments(topo.total_wall, ceiling);
    let buffer =
        args.start_flags.checkpt_buffer.unwrap_or_else(|| vars::default_checkpt_buffer(job_wall));
    if segments > 1 {
        // §8.8
        println!(
            "Total walltime {} exceeds the {} limit of queue \"{}\"; submitting {} chained jobs",
            topo.total_wall.canonical(),
            ceiling.canonical(),
            topo.queue,
            segments
        );
    }

    let identity = Identity::resolve(db, hostname_override);
    let scratch_home = machine.meta.path_for(db, "scratch-home")?.unwrap_or_default();
    let (sub_script_name, run_script_name) = (sub_script_name.to_owned(), run_script_name.to_owned());

    for seg in 0..segments {
        // Every restart gets a fresh id: the reaper (above) cleared the stale
        // active restart, so `next_id` advances the chain.
        let id = restart::next_id(&sim.dir)?;
        let rdir = restart::restart_dir(&sim.dir, id);

        let chained = prev_job.clone();
        let mut vset = vars::assemble(&vars::RestartVarsInput {
            sim,
            machine,
            topo: &topo,
            sim_home: &sim_home,
            restart_id: id,
            job_wall,
            checkpt_buffer: buffer,
            chained_job_id: chained.as_deref().unwrap_or(""),
            hostname: &identity.hostname,
            user: &identity.user,
            email: &identity.email,
            run_universe: run_uni_name,
            debug: false,
            scratch_home: &scratch_home,
        })?;
        vset.set_knobs(knobs.clone());
        // Before the restart dir exists: a parfile that cannot substitute
        // (a required knob unset, a typo'd token) or a script that refuses
        // (a required allocation unset) fails the submit here, leaving
        // nothing behind, not the queued job hours from now — nor an empty
        // restart dir that would take this id.
        check_parfile(sim, &vset)?;

        // Submit-phase artifact: same vars, submit-phase ENV_SETUP under the
        // submit universe (§6.1).
        let mut submit_vars = vset.clone();
        submit_vars.set("ENV_SETUP", machine.meta.phase_env(submit_uni_name, Phase::Submit));
        let submit_script = generate_script(machine, ScriptKind::Submit, &sub_script_name, &submit_vars, Phase::Submit, submit_uni_name)?;
        // Run-phase artifact, frozen now so the compute node never touches
        // the MDB (§8.3.1).
        let run_script = generate_script(machine, ScriptKind::Run, &run_script_name, &vset, Phase::Run, run_uni_name)?;

        fs::create_dir_all(rdir.join(".cactup"))
            .with_context(|| format!("Failed to create '{}'", rdir.display()))?;
        write_executable(&rdir.join(".cactup").join("submit-script"), &submit_script)?;
        write_executable(&rdir.join(".cactup").join("run-script"), &run_script)?;

        let mut r = Restart {
            id,
            dir: rdir.clone(),
            meta: RestartMeta {
                created: Utc::now(),
                started: None,
                finished: None,
                job_stopped: None,
                nodes: topo.nodes,
                tasks: topo.tasks,
                tpn: topo.tpn,
                cpus: topo.cpus,
                gpus_per_task: topo.gpus_per_task,
                queue: topo.queue.clone(),
                allocation: topo.allocation.clone(),
                walltime: job_wall,
                checkpt_buffer: buffer,
                job_id: NO_JOB_ID.to_owned(),
                job_host: None,
                job_start: None,
                foreground: false,
                chained_job_id: chained,
                status: None,
                terminated: false,
                universe: run_uni_spec.clone(),
                vars: restart::freeze_vars(&vset),
                knobs: restart::freeze_knobs(&vset),
            },
        };
        r.store()?;

        // makeActive only for the restart that runs first (§8.3/§8.3.2);
        // chained pre-submissions stay un-activated until their handoff.
        if seg == 0 && activate_first {
            restart::make_active(&sim.dir, id)?;
        }

        let submitted = sched
            .submit(&submit_vars, submit_uni.as_ref().map(|(n, u)| (n.as_str(), *u)))
            .with_context(|| format!("Submitting restart {}", restart::dir_name(id)))?;
        r.meta.set_job(&submitted);
        let job_id = submitted.id;
        r.meta.status = None;
        r.store()?;

        sim.log(
            "submit",
            &format!(
                "submitted {} as job {job_id} (queue {}, wall {}{})",
                restart::dir_name(id),
                topo.queue,
                job_wall.canonical(),
                r.meta
                    .chained_job_id
                    .as_deref()
                    .map(|c| format!(", after job {c}"))
                    .unwrap_or_default()
            ),
        );
        println!(
            "Submitted {} restart {} as job {}{}",
            sim.name.bold(),
            restart::dir_name(id),
            job_id.bold(),
            r.meta
                .chained_job_id
                .as_deref()
                .map(|c| format!(" (chained after job {c})"))
                .unwrap_or_default()
        );

        prev_job = Some(job_id);
    }
    Ok(())
}

#[cfg(test)]
pub(crate) fn refuse_beside_listed_for_tests(sim: &Simulation, sched: &Scheduler, force: bool) -> Res<bool> {
    refuse_beside_listed(sim, sched, force)
}

/// `sim run`'s refusal beside `sim`'s active restart `active`. A job listed
/// in a state no pattern reads is `sim stop -f`'s: a graceful `sim stop`
/// only writes the trigger and says so. Asked only on the way to the
/// refusal; a query that fails leaves the usual advice.
fn active_refusal(sim: &Simulation, sched: &Scheduler, active: u32) -> anyhow::Error {
    let unread = Restart::load(&sim.dir, active)
        .ok()
        .filter(|r| crate::sim::manage::listed_unread(sim, sched, r).unwrap_or(false));
    let (job, stop) = match unread {
        Some(r) => (
            format!(", and {}", crate::scheduler::unread_state(&format!("its job {}", r.meta.job_id))),
            format!("cactup sim stop -f {}", sim.name),
        ),
        None => (String::new(), format!("cactup sim stop {}", sim.name)),
    };
    anyhow!(
        "Simulation \"{}\" already has an active restart ({}){job}; wait for it, or run `{stop}` first",
        sim.name,
        restart::dir_name(active)
    )
}

#[cfg(test)]
pub(crate) fn active_refusal_for_tests(sim: &Simulation, sched: &Scheduler, active: u32) -> String {
    active_refusal(sim, sched, active).to_string()
}

/// Refuse to start a foreground run beside the newest ended restart's job,
/// which the scheduler still lists after the restart ended (§8.6: leaving
/// the queue, or starting again), or could not be asked about: both might
/// run. Called once no running active restart refuses first (whose own
/// message stands), so `sim stop` acts on this job, as the advice says.
/// With `force`, a job the scheduler could not be asked about is stopped
/// instead (taken, or refused and taken as ended), and `true` returned.
fn refuse_beside_listed(sim: &Simulation, sched: &Scheduler, force: bool) -> Res<bool> {
    use crate::scheduler::AfterEnd;
    let Some((ended, answer)) = crate::sim::manage::ended_newest(sim, sched)? else { return Ok(false) };
    let (dir, job_id, name) = (restart::dir_name(ended.id), &ended.meta.job_id, &sim.name);
    match answer {
        Some(AfterEnd::Listed(status)) => bail!(
            "Restart {dir} of simulation \"{name}\" ended, but the scheduler still lists its job {job_id} {}",
            crate::scheduler::listed_after_end(status, &format!("cactup sim stop {name}"))
        ),
        Some(AfterEnd::Unanswered(failed)) if !force => {
            Err(crate::sim::manage::unanswered_refusal(sim, &ended, &failed, "run"))
        }
        Some(AfterEnd::Unanswered(failed)) => {
            crate::sim::manage::stop_unanswered(sim, sched, &ended, &failed)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

/// `sim run` (§8.4): dispatches to the compute-node path (`--sim-dir`,
/// §8.3.1) or the interactive foreground path.
pub fn run(ctx: &Ctx, args: SimRunArgs) -> Res<()> {
    if let Some(sim_dir) = args.sim_dir.clone() {
        // clap enforces `--sim-dir requires --restart-id`.
        let id = args.restart_id.expect("clap: --sim-dir requires --restart-id");
        return run_compute(&args, &sim_dir, id);
    }
    let machine = crate::commands::machine::resolve(ctx)?;
    let inst = Installation::resolve(ctx)?;
    let db = ctx.db.read()?;
    let sim = obtain_sim(ctx, &machine, &inst, &args.start)?;
    run_interactive(
        &inst,
        &machine,
        &db,
        &sim,
        &args,
        ctx.globals.verbose,
        ctx.globals.hostname.as_deref(),
    )
}

/// The interactive foreground path (§8.4): fresh restart, run universe
/// resolved on the spot, stdout/stderr teed to `<SimName>.{out,err}`.
/// §7.4 for a start: both the config and the simulation itself (created on
/// one machine, submitted from another) must be this machine's.
fn check_sim_machine(machine: &Machine, sim: &Simulation, cfg: &ConfigMeta, ignore: bool) -> Res<()> {
    crate::commands::delta::check_machine(machine, &cfg.machine, &format!("config \"{}\"", cfg.name), ignore)?;
    crate::commands::delta::check_machine(machine, &sim.meta.machine, &format!("simulation \"{}\"", sim.name), ignore)
}

fn run_interactive(
    inst: &Installation,
    machine: &Machine,
    db: &Database,
    sim: &Simulation,
    args: &SimRunArgs,
    verbose: bool,
    hostname_override: Option<&str>,
) -> Res<()> {
    let cactus_root = inst.cactus_root();
    let cfg = ConfigMeta::load(&cactus_root, &sim.meta.configuration)?
        .ok_or_else(|| config_missing_error(&cactus_root, machine, &sim.meta.configuration))?;
    check_sim_machine(machine, sim, &cfg, args.start.start_flags.ignore_machine || args.start.force)?;
    crate::commands::delta::warn_if_sources_diverged(inst, &cfg, args.start.silent)?;
    let force_queue = args.start.start_flags.force_queue || args.start.force;
    let fit = vars::BinaryFit::for_config(machine, &cfg)?;
    let placement = vars::Placement::foreground(machine);
    let mut topo =
        vars::resolve_topology(&args.start.topology, machine, db, Some(&fit), force_queue, placement)?;
    // As in submit: the effective knobs, @ALLOCATION@ their allocation.
    let knobs = sim.knob_snapshot(db);
    topo.allocation = knobs.get("allocation").filter(|a| !a.is_empty()).cloned();
    let sim_home = inst.meta()?.sim_home()?.to_owned();
    let sched = Scheduler::for_machine(machine);

    let lock = sim.lock()?;
    restart::reap_stale(sim, &sched)?;
    // A running active restart refuses first, with its own message (`sim
    // stop` acts on it before anything else); then a job still listed after
    // its restart's end (or unanswered), which `sim stop` then acts on.
    let mut active = restart::active_id(&sim.dir)?;
    let running = active.is_some_and(|id| Restart::load(&sim.dir, id).is_ok_and(|r| !r.meta.terminated));
    // `-f` stopped the ended restart's job: an active restart that ended is
    // finished with it (what the reaper does).
    if !running && refuse_beside_listed(sim, &sched, args.start.force)? && active.is_some() {
        crate::sim::manage::clean_active(sim)?;
        active = None;
    }
    if let Some(active) = active {
        return Err(active_refusal(sim, &sched, active));
    }

    let run_scripts = machine.meta.scripts(ScriptKind::Run);
    // Selection is driven by the config's BUILD universe (§4.4).
    let cfg_universe = cfg.universe.as_deref().unwrap_or(HOST_UNIVERSE);
    let (run_script_name, run_entry) = run_scripts.route(&topo.queue, cfg_universe, false)?;
    // Script default tasks (§4.2).
    vars::apply_tasks_default(&mut topo, &args.start.topology, run_entry.tasks, None);
    if let Some(note) = topo.nodes_note() {
        println!("{note}");
    }
    let run_uni = resolve_run_universe(
        machine,
        &cfg,
        &args.start.universe,
        run_entry.universe.as_deref(),
        verbose,
    )?;
    let run_uni_spec = run_uni.as_ref().map(|(name, u)| UniverseSpec::from_universe(name, u));
    let run_uni_name = run_uni.as_ref().map(|(name, _)| name.as_str());

    let job_wall = topo.total_wall.min(machine.meta.effective_max_walltime(&topo.queue)?);
    let buffer = args
        .start
        .start_flags
        .checkpt_buffer
        .unwrap_or_else(|| vars::default_checkpt_buffer(job_wall));

    let identity = Identity::resolve(db, hostname_override);
    let scratch_home = machine.meta.path_for(db, "scratch-home")?.unwrap_or_default();
    // A fresh interactive run always gets a new id; reaping (above) cleared any
    // stale active restart.
    let id = restart::next_id(&sim.dir)?;
    let rdir = restart::restart_dir(&sim.dir, id);

    let mut vset = vars::assemble(&vars::RestartVarsInput {
        sim,
        machine,
        topo: &topo,
        sim_home: &sim_home,
        restart_id: id,
        job_wall,
        checkpt_buffer: buffer,
        chained_job_id: "",
        hostname: &identity.hostname,
        user: &identity.user,
        email: &identity.email,
        run_universe: run_uni_name,
        debug: args.debug,
        scratch_home: &scratch_home,
    })?;
    vset.set_knobs(knobs);
    // As in submit: a parfile that cannot substitute, or a run script that
    // refuses, fails before any restart directory exists.
    check_parfile(sim, &vset)?;
    let run_script = generate_script(machine, ScriptKind::Run, run_script_name, &vset, Phase::Run, run_uni_name)?;
    fs::create_dir_all(rdir.join(".cactup"))
        .with_context(|| format!("Failed to create '{}'", rdir.display()))?;
    write_executable(&rdir.join(".cactup").join("run-script"), &run_script)?;

    let mut r = Restart {
        id,
        dir: rdir.clone(),
        meta: RestartMeta {
            created: Utc::now(),
            started: None,
            finished: None,
            job_stopped: None,
            nodes: topo.nodes,
            tasks: topo.tasks,
            tpn: topo.tpn,
            cpus: topo.cpus,
            gpus_per_task: topo.gpus_per_task,
            queue: topo.queue.clone(),
            allocation: topo.allocation.clone(),
            walltime: job_wall,
            checkpt_buffer: buffer,
            // Set from `own_job` below.
            job_id: NO_JOB_ID.to_owned(),
            job_host: None,
            job_start: None,
            foreground: true,
            chained_job_id: None,
            status: None,
            terminated: false,
            universe: run_uni_spec,
            vars: restart::freeze_vars(&vset),
            knobs: restart::freeze_knobs(&vset),
        },
    };
    // The interactive run IS the job; our pid is what `ps`-style
    // get-status commands can see (matches the generic machine), recorded
    // with its host and start time where the machine's job ids are pids.
    r.meta.set_job(&sched.own_job());
    r.store()?;
    restart::make_active(&sim.dir, id)?;
    // A fresh heartbeat before the sim lock drops: between here and
    // execute_restart acquiring running.lock there is a window where a
    // concurrent submit's reaper would otherwise see an active restart with
    // no job, no lock, and no heartbeat — i.e. "never started = dead" —
    // and reap the restart that is about to run.
    r.touch_heartbeat();
    drop(lock);

    sim.log("run", &format!("running {} in the foreground", restart::dir_name(id)));
    println!("Running {} restart {}", sim.name.bold(), restart::dir_name(id));
    execute_restart(sim, &mut r, true)
}

/// The compute-node path (§8.3.1): locate the restart purely from
/// `--sim-dir`/`--restart-id` and its on-disk metadata; never touch the
/// global DB, the registry, the MDB, or knobs (D11).
fn run_compute(args: &SimRunArgs, sim_dir: &Path, id: u32) -> Res<()> {
    let sim = Simulation::open(&args.start.sim, sim_dir)?;

    // Chain handoff (§8.3.2), under the per-simulation lock — waiting for the
    // submitter to let go of it if the job started that quickly. The restart
    // is read under it too: the submitter records the job id (and on a
    // `job-ids = "pids"` machine its host and start time) holding the lock,
    // and a copy read before that would be written back without them.
    let mut r = {
        let _lock = sim.lock_wait()?;
        restart::handoff_active(&sim.dir, id)?;
        Restart::load(&sim.dir, id)?
    };

    // No recovery step here: the parfile points Cactus at its recovery dir and
    // Cactus loads the newest checkpoint itself (§8.8).
    sim.log("run", &format!("compute-node run of {}", restart::dir_name(id)));
    execute_restart(&sim, &mut r, false)
}

/// Single-quote a string for `/bin/sh`.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the command that executes a stored script, optionally wrapped in a
/// universe (§4.8), with `cwd` as the working directory.
///
/// The script is run by its interpreter, as the kernel would run it, never
/// exec'd itself ([`interpreter_command`]): cactup writes a script and runs
/// it moments later, and a thread of this process that forks in between
/// hands its child an open write handle on the file, until that child execs.
/// The kernel refuses to exec a file open for writing ("Text file busy");
/// an interpreter only reads it.
pub(crate) fn script_command(
    script: &Path,
    universe: Option<&Universe>,
    vars: &VarSet,
    cwd: &Path,
) -> Res<Command> {
    // The first line is all that is read (a bounded piece of it; the
    // kernel reads less), as bytes: the rest of a script need not be text.
    let mut head = Vec::new();
    if let Ok(file) = fs::File::open(script) {
        use std::io::BufRead;
        let _ = std::io::BufReader::new(std::io::Read::take(file, 4096)).read_until(b'\n', &mut head);
    }
    let first = head.split(|b| *b == b'\n').next().unwrap_or_default();
    let inner = interpreter_command(&String::from_utf8_lossy(first), &shell_quote(&script.display().to_string()));
    let mut cmd = match universe {
        None => {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", &inner]);
            c
        }
        Some(u) => match u.wrap(vars, &inner)? {
            WrappedCommand::Argv(argv) => {
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
            WrappedCommand::Shell(s) => {
                let mut c = Command::new("/bin/sh");
                c.args(["-c", &s]);
                c
            }
        },
    };
    cmd.current_dir(cwd);
    Ok(cmd)
}

/// The shell text that runs the script `quoted` (already quoted) whose text
/// is `text`: its `#!` line's interpreter, and the one argument the kernel
/// would give it (whatever follows the interpreter on that line, as one
/// word), before the script; `/bin/sh` for a script without one.
fn interpreter_command(text: &str, quoted: &str) -> String {
    // The kernel's reading: spaces and tabs around the line, and between the
    // interpreter and its one argument, are dropped; nothing else is (a
    // carriage return stays, and fails as it always did). A relative
    // interpreter is found from the working directory, not by `PATH`.
    let blank = [' ', '\t'];
    let line = text.strip_prefix("#!").map(|rest| rest.split('\n').next().unwrap_or_default().trim_matches(blank));
    let interpreter = |name: &str| match name.contains('/') {
        true => shell_quote(name),
        false => shell_quote(&format!("./{name}")),
    };
    match line.filter(|line| !line.is_empty()) {
        Some(line) => match line.split_once(blank) {
            Some((name, arg)) => format!("{} {} {quoted}", interpreter(name), shell_quote(arg.trim_matches(blank))),
            None => format!("{} {quoted}", interpreter(line)),
        },
        None => format!("/bin/sh {quoted}"),
    }
}

/// Execute a prepared restart: resolve the parfile, create the working dir
/// and `TERMINATE`, hold the liveness marker + heartbeat (§9.3), run the
/// stored run-script wrapped in the frozen run universe (§4.8), and record
/// completion. `tee` echoes to the terminal while writing
/// `<SimName>.{out,err}` (the §8.4 foreground behavior).
fn execute_restart(sim: &Simulation, r: &mut Restart, tee: bool) -> Res<()> {
    // The frozen variable set drives parfile resolution and universe wrapping.
    let vset = restart::thaw_vars(&r.meta.vars, &r.meta.knobs)?;

    let parfile = resolve_parfile(sim, &r.dir, &vset)?;
    let workdir = restart::workdir(sim, r.id);
    fs::create_dir_all(&workdir).with_context(|| format!("Failed to create '{}'", workdir.display()))?;
    // TERMINATE is deliberately NOT created here. It is the running Cactus
    // job's file (TerminationTrigger's create_termination_file), and `sim stop`
    // treats its existence as the evidence that this run is actually watching
    // it — a file we minted ourselves would send every stop down the graceful
    // path, writing 1 into something nobody reads while the job runs on (§8.6).

    // Build the command: run-script, wrapped in the run universe if any.
    let script = r.cactup_dir().join("run-script");
    let universe = r.meta.universe.as_ref().map(|u| u.to_universe());
    let cmd = script_command(&script, universe.as_ref(), &vset, &r.dir)?;

    // Liveness (§2.3/§9.3): running.lock with heartbeat + the heartbeat file.
    let running = LinkLock::acquire(&r.running_lock_path())?.with_heartbeat();
    r.touch_heartbeat();
    update_restart(sim, r, |meta| {
        meta.started = Some(Utc::now());
        meta.status = Some("R".to_owned());
        // Running, so not ended: a job the scheduler requeued after its run
        // recorded its end (preemption, `scontrol requeue`) runs again here,
        // and records its end again when it is over (§8.6).
        meta.terminated = false;
        meta.finished = None;
    })?;

    let tee_files = tee.then(|| {
        (r.dir.join(format!("{}.out", sim.name)), r.dir.join(format!("{}.err", sim.name)))
    });
    let status = spawn_and_wait(cmd, &r.heartbeat_path(), tee_files);
    drop(running);

    // Record completion whatever happened; the parfile decides its own
    // checkpoint/termination behavior — cactup only bookkeeps (§8.8).
    update_restart(sim, r, |meta| {
        meta.finished = Some(Utc::now());
        meta.terminated = true;
        meta.status = Some("U".to_owned());
    })?;
    let _ = parfile; // the ready-to-run parfile stays in the restart (§9.1)

    match status {
        Ok(st) if st.success() => {
            sim.log("run", &format!("{} finished", restart::dir_name(r.id)));
            println!("Simulation {} restart {} finished", sim.name.bold(), restart::dir_name(r.id));
            Ok(())
        }
        Ok(st) => {
            sim.log("run", &format!("{} failed ({st})", restart::dir_name(r.id)));
            bail!("The run-script exited unsuccessfully ({st})");
        }
        Err(e) => {
            sim.log("run", &format!("{} failed to start: {e:#}", restart::dir_name(r.id)));
            Err(e)
        }
    }
}

/// Change restart `r` on disk by `edit` and nothing else: read afresh,
/// edited, stored, and `r` left holding what was stored. A run keeps its
/// restart for days, and meanwhile login-side commands write to the same
/// file (`sim stop`, the `job-stopped` record, the reaper); storing the copy
/// read at the start would silently undo them. The per-simulation lock is
/// taken when it is free, but never waited for: `sim stop` holds it while it
/// waits for this very run to end, so waiting would stall the stop (and the
/// end of a job near its walltime). Without it the file is still read
/// afresh, so only a write landing in the same instant could be lost. A file
/// that cannot be read at all gets the run's own copy, with a warning.
fn update_restart(sim: &Simulation, r: &mut Restart, edit: impl FnOnce(&mut restart::RestartMeta)) -> Res<()> {
    let _lock = match sim.try_lock() {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("{}", format!("Could not take the simulation's lock ({e:#}); recording without it").yellow());
            None
        }
    };
    match Restart::load(&sim.dir, r.id) {
        Ok(fresh) => r.meta = fresh.meta,
        // Unreadable now: the run's own copy is recorded rather than nothing,
        // and said so, since whatever was written meanwhile is lost.
        Err(e) => eprintln!(
            "{}",
            format!("Could not re-read restart {} ({e:#}); recording the run's own copy", restart::dir_name(r.id))
                .yellow()
        ),
    }
    edit(&mut r.meta);
    r.store()
}

/// Spawn the run and wait for it, touching the heartbeat file every
/// HEARTBEAT_SECS (§9.3). With `tee = Some((out, err))`, stdout/stderr are
/// piped and copied to both the terminal and those files (§8.4); without it
/// (the compute-node path) stdio is inherited — the submit template's
/// `@STDOUT_FILE@`/`@STDERR_FILE@` redirection owns the output.
pub(crate) fn spawn_and_wait(
    mut cmd: Command,
    heartbeat: &Path,
    tee: Option<(PathBuf, PathBuf)>,
) -> Res<std::process::ExitStatus> {
    if tee.is_some() {
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    crate::shell::trace_command(&cmd);
    let mut child = cmd.spawn().with_context(|| "Failed to spawn the run-script")?;

    fn tee_thread(
        mut src: impl std::io::Read + Send + 'static,
        file_path: PathBuf,
        to_stderr: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
            let mut file = fs::File::create(&file_path).ok();
            let mut buf = [0u8; 8192];
            loop {
                match src.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let chunk = &buf[..n];
                        if to_stderr {
                            let _ = std::io::stderr().write_all(chunk);
                        } else {
                            let _ = std::io::stdout().write_all(chunk);
                        }
                        if let Some(f) = file.as_mut() {
                            let _ = f.write_all(chunk);
                        }
                    }
                }
            }
        })
    }

    let mut copiers = Vec::new();
    if let Some((out_file, err_file)) = tee {
        copiers.push(tee_thread(child.stdout.take().expect("stdout piped"), out_file, false));
        copiers.push(tee_thread(child.stderr.take().expect("stderr piped"), err_file, true));
    }

    // Wait, touching the heartbeat file each HEARTBEAT_SECS (§9.3).
    let started = std::time::Instant::now();
    let mut last_beat = 0u64;
    let mut interrupted_at: Option<std::time::Instant> = None;
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        // Ctrl-C: a foreground child gets the terminal's SIGINT itself; a
        // child that has not exited a couple of seconds later (it ignores
        // SIGINT, or the signal came as a lone SIGTERM to cactup) is killed,
        // so the interrupt always resolves promptly.
        if gix::interrupt::is_triggered() {
            let since = interrupted_at.get_or_insert_with(std::time::Instant::now);
            if since.elapsed() > std::time::Duration::from_secs(2) {
                let _ = child.kill();
            }
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed / HEARTBEAT_SECS > last_beat {
            last_beat = elapsed / HEARTBEAT_SECS;
            // A plain write stamps the mtime with the *fileserver's* clock —
            // the same domain the reaper's age check reads — where
            // set_modified(now) would inject this compute node's local
            // clock, skewed on NFS.
            let _ = fs::write(heartbeat, b"");
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    crate::par::join_with_deadline(
        copiers,
        std::time::Duration::from_secs(2),
        "note: a background process of the run-script still holds the output pipe; \
         its further output is not captured",
    );
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::OptionlistSource;
    use crate::args::TopologyFlags;
    use crate::mdb::Layer;
    use crate::walltime::Walltime;

    /// A workstation-like machine with a fake echo scheduler and a 24 h queue
    /// ceiling, written to disk so script selection/loading is real.
    fn fake_machine(dir: &Path) -> Machine {
        fs::create_dir_all(dir.join("submitscripts")).unwrap();
        fs::create_dir_all(dir.join("runscripts")).unwrap();
        fs::write(
            dir.join("submitscripts/default.sh"),
            "#!/bin/sh\n#SBATCH --dependency=afterany:@CHAINED_JOB_ID@\n# chained (a comment: not substituted): @CHAINED_JOB_ID@\n\
             exec @CACTUP@ sim run @SIMULATION_NAME@ --installation=@ALIAS@ \
             --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ --restart-id=@RESTART_ID@\n",
        )
        .unwrap();
        fs::write(
            dir.join("runscripts/default.sh"),
            "#!/bin/sh\ncd @RUNDIR@-active\n\
             echo \"ran @SIMULATION_NAME@ tasks=@TASKS@\" > ran.txt\n\
             grep -q . @PARFILE@ || exit 3\n",
        )
        .unwrap();
        let meta: crate::mdb::Meta = toml::from_str(
            r#"
            [machine]
            name = "fake"

            [hardware]
            max-cpus-per-node = 8

            [environment]
            env-setup = "export CACTUP_TEST_ENV=1"

            [scheduler]
            submit = "echo submitted as [JOB-@RESTART_ID@]"
            submit-pattern = '\[(JOB-[^\]]*)\]'
            get-status = "true"
            status-pattern = "$^"
            queued-pattern = "$^"
            running-pattern = "$^"
            holding-pattern = "$^"
            stop = "true"

            [queues.batch]
            default = true
            max-walltime = "24:00:00"

            [submitscripts]
            "default" = { queues = ["batch"] }

            [runscripts]
            "default" = { queues = ["batch"] }

            "#,
        )
        .unwrap();
        Machine { name: "fake".to_owned(), dir: dir.to_owned(), layer: Layer::System, meta }
    }

    /// A minimal installation: installation.toml (sim-home, active config),
    /// a built config's metadata + executable.
    fn fake_installation(root: &Path) -> Installation {
        let inst = Installation::new("et", root);
        let sim_home = root.join("simhome");
        fs::create_dir_all(&sim_home).unwrap();
        {
            let locked = inst.locked().unwrap();
            let mut meta = locked.meta().unwrap();
            meta.active_config = Some("sim".to_owned());
            meta.sim_home = Some(sim_home);
            locked.set_meta(&meta).unwrap();
        }
        let cfg_dir = inst.cactus_root().join("configs").join("sim");
        fs::create_dir_all(&cfg_dir).unwrap();
        fs::write(
            cfg_dir.join("cactup-config.toml"),
            r#"
            name = "sim"
            flavor = "default"
            thornlist = "installation-default.th"
            machine = "fake"
            config-id = "config-sim-1"
            build-id = "build-sim-1"
            "#,
        )
        .unwrap();
        // The build artifacts `sim create` snapshots into a simulation for
        // provenance (§8.2): the optionlist pair and the thornlist pair.
        for (file, body) in [
            ("cactup-optionlist.cfg", "VERSION = 1\nCC = gcc\n"),
            ("cactup-optionlist.toml", "[options]\nVERSION = \"1\"\n"),
            ("cactup-thornlist.th", "A/B\n#DISABLED C/D\n"),
            ("cactup-thornlist.src.th", "A/B\nC/D\n"),
        ] {
            fs::write(cfg_dir.join(file), body).unwrap();
        }
        let exe_dir = inst.cactus_root().join("exe");
        fs::create_dir_all(&exe_dir).unwrap();
        fs::write(exe_dir.join("cactus_sim"), "#!/bin/sh\necho cactus\n").unwrap();
        inst
    }

    fn create_req(parfile: &Path) -> crate::sim::CreateRequest<'_> {
        crate::sim::CreateRequest {
            force: false,
            ignore_machine: false,
            name: "bbh",
            parfile,
            config: None,
            sim_dir: None,
        }
    }

    fn fake_ctx(dir: &Path) -> Ctx {
        Ctx {
            globals: crate::args::GlobalOpts {
                verbose: false,
                trace: false,
                manifest_url: String::new(),
                mdb_path: None,
                machine: Some("fake".to_owned()),
                installation: Some("et".to_owned()),
                hostname: Some("testhost".to_owned()),
                knob: Vec::new(),
                risky_allow_untrusted_artifacts: false,
            },
            db: crate::database::Db::in_dir(dir),
        }
    }

    fn start_args(sim: &str, wall: &str) -> SimStartArgs {
        SimStartArgs {
            silent: false,
            sim: sim.to_owned(),
            parfile: None,
            config: None,
            force: false,
            overwrite: false,
            start_flags: crate::args::StartFlags::default(),
            universe: UniverseFlags { universe: None, no_universe: false },
            topology: TopologyFlags {
                allocation: None,
                queue: None,
                mail: None,
                mail_type: None,
                nodes: None,
                tasks: None,
                tpn: None,
                cpus: None,
                gpu: false,
                gpus_per_task: None,
                job_name: None,
                wall_time: Some(Walltime::parse(wall).unwrap()),
                out: None,
                err: None,
            },
        }
    }


    /// A run's start and finish writes change only their own fields: what a
    /// login-side command wrote meanwhile (`sim stop`'s job-stopped record)
    /// survives the finish, though the run's copy was read before it.
    #[test]
    fn a_runs_writes_keep_what_was_recorded_during_the_run() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("bbh");
        let meta = crate::sim::SimulationMeta { parfile: "bbh.par".to_owned(), ..Default::default() };
        let record = dir.join(".cactup").join("simulation.toml");
        crate::format::write(&record, &meta, &crate::sim::SIMULATION).unwrap();
        let rdir = restart::restart_dir(&dir, 0);
        fs::create_dir_all(rdir.join(".cactup")).unwrap();
        fs::write(
            rdir.join(".cactup").join("restart.toml"),
            "created = \"2024-01-01T00:00:00Z\"\nnodes = 1\ntasks = 1\ntpn = 1\ncpus = 1\nqueue = \"debug\"\n\
             walltime = \"24:00:00\"\ncheckpt-buffer = \"00:10:00\"\njob-id = \"42\"\n",
        )
        .unwrap();
        let sim = Simulation::open("bbh", &dir).unwrap();
        let mut run_copy = Restart::load(&dir, 0).unwrap();
        update_restart(&sim, &mut run_copy, |m| m.status = Some("R".to_owned())).unwrap();

        // A login-side command writes during the run.
        let mut login = Restart::load(&dir, 0).unwrap();
        let stopped = Utc::now();
        login.meta.job_stopped = Some(stopped);
        login.store().unwrap();

        update_restart(&sim, &mut run_copy, |m| {
            m.terminated = true;
            m.status = Some("U".to_owned());
        })
        .unwrap();
        // `sim stop` holds the lock while it waits for the run to end: the
        // run's write must not wait for it (that stalled every graceful stop).
        let held = sim.lock().unwrap();
        let started = std::time::Instant::now();
        update_restart(&sim, &mut run_copy, |m| m.terminated = true).unwrap();
        assert!(started.elapsed() < std::time::Duration::from_secs(2), "the finish did not wait for the lock");
        drop(held);
        let on_disk = Restart::load(&dir, 0).unwrap();
        assert_eq!(on_disk.meta.job_stopped, Some(stopped), "the login-side record survives the finish");
        assert!(on_disk.meta.terminated);
        assert_eq!(on_disk.meta.status.as_deref(), Some("U"));
        assert_eq!(run_copy.meta.job_stopped, Some(stopped), "the run's copy is what was stored");
    }
    #[test]
    fn submit_chains_and_compute_run_executes() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));

        // Create via the real path (registry + cache + metadata). The parfile
        // reads a custom knob (required) and a standard one (optional, with
        // a default) — §6.1/§6.2.
        let parfile = tmp.path().join("bbh.par");
        fs::write(
            &parfile,
            "ActiveThorns = \"IOUtil\"\n\
             IO::out_dir = \"sim @SIMULATION_NAME@ t=@TASKS@ lit=@@\" # @SIMULATION_NAME@ me@host\n\
             kadathimporter::filename = \"@KNOB(kadath-initial-data)@\"\n\
             IO::checkpoint_dir = \"@KNOB-OPTIONAL(mail, none)@\"\n",
        )
        .unwrap();
        let sim = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();

        // The required knob is unset: submit refuses up front, naming the
        // knob and how to set it, and leaves no restart directory behind.
        let db = ctx.db.read().unwrap();
        let args = start_args("bbh", "50:00:00");
        let err = submit_impl(&inst, &machine, &db, &sim, &args, false, Some("testhost")).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("knob kadath-initial-data is unset"), "{msg}");
        assert!(msg.contains("-K kadath-initial-data=VALUE"), "{msg}");
        assert!(restart::list_ids(&sim.dir).unwrap().is_empty(), "no restart dir left behind");
        ctx.db
            .update(|db| {
                db.set_knob("kadath-initial-data", "/scratch/id/bhns.info".to_owned());
                Ok(())
            })
            .unwrap();
        assert!(sim.exe().is_file(), "frozen executable linked");
        assert!(inst.simulations().unwrap().simulations.contains_key("bbh"));

        // Build provenance travels with the simulation (§8.2): both the
        // optionlist and the thornlist pair are copied in, and the metadata
        // names the fed-to-Cactus copy of each.
        let cfg = sim.dir.join(".cactup/cfg");
        assert_eq!(sim.meta.optionlist, "cactup-optionlist.cfg");
        assert_eq!(sim.meta.thornlist, "cactup-thornlist.th");
        assert_eq!(
            fs::read_to_string(cfg.join("cactup-thornlist.th")).unwrap(),
            "A/B\n#DISABLED C/D\n",
            "the processed thornlist records which thorns the frozen binary has"
        );
        for f in ["cactup-optionlist.cfg", "cactup-optionlist.toml", "cactup-thornlist.src.th"] {
            assert!(cfg.join(f).is_file(), "{f} should be snapshotted into the sim");
        }

        // A submit script that refuses (a machine requiring an allocation,
        // none set) also leaves no restart directory behind: no husk to
        // take id 0, which the good submit below must still get.
        let scripts = machine.dir.join("submitscripts");
        fs::rename(scripts.join("default.sh"), scripts.join("default.sh.off")).unwrap();
        fs::write(scripts.join("default.py"), "print(ALLOCATION or knob(\"allocation\"))\n").unwrap();
        let db = ctx.db.read().unwrap();
        let err = submit_impl(&inst, &machine, &db, &sim, &start_args("bbh", "50:00:00"), false, Some("testhost"))
            .unwrap_err();
        assert!(format!("{err:#}").contains("knob allocation is unset"), "{err:#}");
        assert!(restart::list_ids(&sim.dir).unwrap().is_empty(), "no restart dir left behind");
        assert!(!restart::restart_dir(&sim.dir, 0).exists(), "not even an empty one");
        fs::remove_file(scripts.join("default.py")).unwrap();
        fs::rename(scripts.join("default.sh.off"), scripts.join("default.sh")).unwrap();

        // 50 h on a 24 h ceiling → 3 chained segments (§8.8).
        submit_impl(&inst, &machine, &db, &sim, &start_args("bbh", "50:00:00"), false, Some("testhost"))
            .unwrap();

        let ids = restart::list_ids(&sim.dir).unwrap();
        assert_eq!(ids, vec![0, 1, 2]);
        // Only the first restart of the chain is active (§8.3.2).
        assert_eq!(restart::active_id(&sim.dir).unwrap(), Some(0));

        let r0 = Restart::load(&sim.dir, 0).unwrap();
        let r1 = Restart::load(&sim.dir, 1).unwrap();
        let r2 = Restart::load(&sim.dir, 2).unwrap();
        assert_eq!(r0.meta.job_id, "JOB-0");
        // The chain is wired by scheduler dependency alone — no recovery
        // lineage is recorded, because cactup does not steer recovery (§8.8).
        assert_eq!(r0.meta.chained_job_id.as_deref(), None);
        assert_eq!(r1.meta.chained_job_id.as_deref(), Some("JOB-0"));
        assert_eq!(r2.meta.chained_job_id.as_deref(), Some("JOB-1"));
        // Each segment reserves the ceiling (§8.8).
        assert_eq!(r1.meta.walltime, Walltime::parse("24:00:00").unwrap());
        // Buffer default: 24 h / 24 = 1 h.
        assert_eq!(r0.meta.checkpt_buffer, Walltime(3600));

        // Generated submit-script: substituted, env-prepended, executable.
        let script = fs::read_to_string(r1.dir.join(".cactup/submit-script")).unwrap();
        assert!(script.contains("--restart-id=1"), "{script}");
        assert!(script.contains("--sim-dir="), "{script}");
        assert!(script.contains("#SBATCH --dependency=afterany:JOB-0"), "{script}");
        // A directive substitutes; a comment beside it stays verbatim (§6.1).
        assert!(script.contains("# chained (a comment: not substituted): @CHAINED_JOB_ID@"), "{script}");
        // Submit-script env goes after the whole '#' header block (§6.1), not
        // right after the shebang, so scheduler directives stay on top.
        assert!(
            script.starts_with("#!/bin/sh\n#SBATCH --dependency=afterany:JOB-0\n# chained (a comment: not substituted): @CHAINED_JOB_ID@\nexport CACTUP_TEST_ENV=1\nexec"),
            "env after the directive block: {script}"
        );

        // Frozen vars allow full reconstruction (D11) — the knob snapshot
        // included, so the compute node never needs the DB for @KNOB(…)@.
        let vars = restart::thaw_vars(&r2.meta.vars, &r2.meta.knobs).unwrap();
        assert_eq!(vars.get("RESTART_ID").unwrap().canonical(), "2");
        assert_eq!(vars.get("QUEUE").unwrap().canonical(), "batch");
        assert_eq!(vars.get("TASKS").unwrap().canonical(), "8", "fill-the-node default");
        assert_eq!(r2.meta.knobs["kadath-initial-data"], "/scratch/id/bhns.info");
        assert_eq!(r2.meta.knobs["mail-type"], "all", "derived defaults are part of the snapshot");
        assert!(!r2.meta.knobs.contains_key("mail"));

        // Fake a checkpoint in restart 0, then run restart 1 on the "compute
        // node": handoff + run-script execution. cactup must leave the
        // checkpoint exactly where it is (§8.8).
        let w0 = restart::workdir(&sim, 0);
        fs::create_dir_all(&w0).unwrap();
        fs::write(w0.join("bbh.chkpt.it_10.h5"), b"ckpt").unwrap();

        let run_args = SimRunArgs {
            start: start_args("bbh", "24:00:00"),
            debug: false,
            restart_id: Some(1),
            sim_dir: Some(sim.dir.clone()),
        };
        run_compute(&run_args, &sim.dir, 1).unwrap();

        // Handoff moved the active symlink 0 → 1 (§8.3.2).
        assert_eq!(restart::active_id(&sim.dir).unwrap(), Some(1));
        // Recovery is the parfile's business (§8.8): restart 0's checkpoint
        // stays put and nothing is linked into restart 1.
        assert!(w0.join("bbh.chkpt.it_10.h5").is_file());
        assert!(!restart::workdir(&sim, 1).join("bbh.chkpt.it_10.h5").exists());
        // The runscript ran in the restart dir via the -active symlink.
        let ran = fs::read_to_string(r1.dir.join("ran.txt")).unwrap();
        assert_eq!(ran.trim(), "ran bbh tasks=8");
        // Parfile resolved with substitution and the @@ escape, its comment
        // verbatim (§6.1/§6.2), knobs read from the frozen snapshot.
        let par = fs::read_to_string(r1.dir.join("bbh.par")).unwrap();
        assert!(par.contains("IO::out_dir = \"sim bbh t=8 lit=@\" # @SIMULATION_NAME@ me@host"), "{par}");
        assert!(par.contains("kadathimporter::filename = \"/scratch/id/bhns.info\""), "{par}");
        assert!(par.contains("IO::checkpoint_dir = \"none\""), "{par}");
        // Completion recorded; the heartbeat exists (§9.3). TERMINATE does
        // not: only a real Cactus run creates it (§8.6), and `sim stop` reads
        // its absence as "no graceful trigger, kill via the scheduler".
        let r1 = Restart::load(&sim.dir, 1).unwrap();
        assert!(r1.meta.terminated && r1.meta.finished.is_some());
        assert!(!r1.dir.join("TERMINATE").exists());
        assert!(r1.heartbeat_path().is_file());
        // log.txt in the preserved format (§12).
        let log = fs::read_to_string(sim.log_path()).unwrap();
        assert!(log.contains("] create::"), "{log}");
        assert!(log.contains("] submit::submitted output-0000 as job JOB-0"), "{log}");
    }

    /// §7.9's build-submit feature opens a real corruption race at `sim
    /// create`: it hard-links `exe/cactus_<config>` into the executable
    /// cache, and a live build's `make` can be replacing those very bytes
    /// right now. Unlike everywhere else `in_flight_build` is consulted,
    /// this is a hard error — only `-f` may proceed anyway.
    #[test]
    fn create_refuses_while_a_build_is_live_for_its_config() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "ActiveThorns = \"IOUtil\"\n").unwrap();

        // A rebuild of "sim" is actively running right now — running.lock is
        // held, exactly what `execute` holds for the whole life of a build.
        let cactus_root = inst.cactus_root();
        let config_dir = cactus_root.join("configs/sim");
        let attempt = crate::build::attempt::BuildAttempt::create(
            crate::build::attempt::BuildAttempt::attempt_dir(&config_dir, 0),
            crate::build::attempt::BuildMeta {
                attempt_id: 0,
                config: "sim".to_owned(),
                optionlist_source: OptionlistSource::Flavor("default".to_owned()),
                machine: "fake".to_owned(),
                alias: "et".to_owned(),
                config_dir: config_dir.clone(),
                cactus_root: cactus_root.clone(),
                install_root: cactus_root.clone(),
                submitted: false,
                job_id: "999".to_owned(),
                job_host: None,
                job_start: None,
                status: None,
                reservation: None,
                decision: "test decision".to_owned(),
                full_rebuild: false,
                make: None,
                build_env: String::new(),
                build_provisioning: String::new(),
                virtual_executable: None,
                universe: None,
                config_meta: toml::from_str(
                    r#"
                    schema = 1
                    name = "sim"
                    flavor = "default"
                    thornlist = "sim.th"
                    machine = "fake"
                    config-id = "cfg-sim"
                    build-id = "build-sim"
                    "#,
                )
                .unwrap(),
                vars: Default::default(),
                knobs: Default::default(),
                timestamps: crate::build::attempt::Timestamps::default(),
                outcome: None,
            },
        )
        .unwrap();
        let _held = crate::lock::LinkLock::acquire(&attempt.running_lock_path()).unwrap();

        let err = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("999"), "{msg}");
        assert!(
            !inst.simulations().unwrap().simulations.contains_key("bbh"),
            "refused before anything was registered"
        );

        // -f proceeds despite the live build.
        let mut req = create_req(&parfile);
        req.force = true;
        let sim = crate::sim::create(&ctx, &machine, &inst, &req).unwrap();
        assert!(sim.exe().is_file());
    }

    #[test]
    fn resubmit_allocates_a_fresh_restart_and_leaves_checkpoints_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));

        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "ActiveThorns = \"IOUtil\"\n").unwrap();
        let sim = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();
        let db = ctx.db.read().unwrap();

        // Two checkpoint-bearing restarts. Under the old model these drove a
        // backward scan; now they are simply none of cactup's business (§8.8).
        for id in [0u32, 1] {
            let w = restart::workdir(&sim, id);
            fs::create_dir_all(&w).unwrap();
            fs::write(w.join(format!("bbh.chkpt.it_{id}0.h5")), b"ckpt").unwrap();
        }

        let args = start_args("bbh", "1:00:00");
        submit_impl(&inst, &machine, &db, &sim, &args, false, Some("testhost")).unwrap();

        // Resubmit allocates the next id and activates it.
        assert_eq!(restart::list_ids(&sim.dir).unwrap(), vec![0, 1, 2]);
        assert_eq!(restart::active_id(&sim.dir).unwrap(), Some(2));
        let r = Restart::load(&sim.dir, 2).unwrap();
        let thawed = restart::thaw_vars(&r.meta.vars, &r.meta.knobs).unwrap();
        assert_eq!(thawed.get("RESTART_ID").unwrap().canonical(), "2");
        // Nothing was copied or linked forward, and the originals are untouched.
        assert!(!restart::workdir(&sim, 2).join("bbh.chkpt.it_10.h5").exists());
        assert!(restart::workdir(&sim, 0).join("bbh.chkpt.it_00.h5").is_file());
        assert!(restart::workdir(&sim, 1).join("bbh.chkpt.it_10.h5").is_file());
    }

    /// §7.4: a config or simulation recorded for another machine is refused
    /// at create and at submit; `--ignore-machine` (or `-f`) lets it through.
    #[test]
    fn foreign_machine_config_or_sim_is_refused_unless_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "x\n").unwrap();
        let sim = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();

        // The config now claims another machine.
        let cfg_path = inst.cactus_root().join("configs/sim/cactup-config.toml");
        let text = fs::read_to_string(&cfg_path)
            .unwrap()
            .replace("machine = \"fake\"", "machine = \"elsewhere\"");
        fs::write(&cfg_path, text).unwrap();

        let db = ctx.db.read().unwrap();
        let args = start_args("bbh", "1:00:00");
        let err = format!(
            "{:#}",
            submit_impl(&inst, &machine, &db, &sim, &args, false, Some("testhost")).unwrap_err()
        );
        assert!(err.contains("config \"sim\" was built for machine \"elsewhere\""), "{err}");
        let mut req = create_req(&parfile);
        req.force = true;
        let err = format!("{:#}", crate::sim::create(&ctx, &machine, &inst, &req).unwrap_err());
        assert!(err.contains("built for machine \"elsewhere\""), "{err}");
        req.ignore_machine = true;
        crate::sim::create(&ctx, &machine, &inst, &req).unwrap();
        let mut args = start_args("bbh", "1:00:00");
        args.start_flags.ignore_machine = true;
        submit_impl(&inst, &machine, &db, &sim, &args, false, Some("testhost")).unwrap();

        // A simulation created elsewhere is refused on its own account, and
        // -f covers it like every other nag.
        fs::write(
            &cfg_path,
            fs::read_to_string(&cfg_path).unwrap().replace("\"elsewhere\"", "\"fake\""),
        )
        .unwrap();
        let mut foreign = crate::sim::Simulation::open("bbh", &sim.dir).unwrap();
        foreign.meta.machine = "elsewhere".to_owned();
        let args = start_args("bbh", "1:00:00");
        let err = format!(
            "{:#}",
            submit_impl(&inst, &machine, &db, &foreign, &args, false, Some("testhost")).unwrap_err()
        );
        assert!(err.contains("simulation \"bbh\" was built for machine \"elsewhere\""), "{err}");
        let mut args = start_args("bbh", "1:00:00");
        args.force = true;
        submit_impl(&inst, &machine, &db, &foreign, &args, false, Some("testhost")).unwrap();
    }

    #[test]
    fn submit_conflicting_parfile_needs_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "x\n").unwrap();
        crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();

        // Existing sim + parfile → error without --overwrite (§8.3)…
        let mut args = start_args("bbh", "1:00:00");
        args.parfile = Some(parfile.clone());
        let err = obtain_sim(&ctx, &machine, &inst, &args).unwrap_err().to_string();
        assert!(err.contains("--overwrite"), "{err}");
        // …and recreates with it.
        args.overwrite = true;
        obtain_sim(&ctx, &machine, &inst, &args).unwrap();
        // No sim and no parfile → guidance.
        let args2 = start_args("ghost", "1:00:00");
        let err = obtain_sim(&ctx, &machine, &inst, &args2).unwrap_err().to_string();
        assert!(err.contains("parfile"), "{err}");
    }

    #[test]
    fn run_universe_precedence() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("mdb-fake");
        let mut machine = fake_machine(&dir);
        // Give the machine a universe registry.
        let extra: crate::mdb::Meta = toml::from_str(
            r#"
            [machine]
            name = "fake"
            [universes.sif]
            wrapper-argv = ["apptainer", "exec", "img.sif"]
            [universes.other]
            wrapper-argv = ["env"]
            [submitscripts]
            "default" = { queues = ["batch"] }
            [runscripts]
            "default" = { queues = ["batch"] }
            "#,
        )
        .unwrap();
        machine.meta.universes = extra.universes;

        let cfg_plain: ConfigMeta = toml::from_str(
            r#"
            name = "sim"
            flavor = "default"
            thornlist = "t"
            machine = "fake"
            config-id = "c"
            build-id = "b"
            "#,
        )
        .unwrap();
        let mut cfg_coerced = cfg_plain.clone();
        cfg_coerced.universe = Some("sif".to_owned());

        let no_flags = UniverseFlags { universe: None, no_universe: false };

        // 1. CLI wins.
        let cli = UniverseFlags { universe: Some("other".to_owned()), no_universe: false };
        let got = resolve_run_universe(&machine, &cfg_coerced, &cli, None, false).unwrap();
        assert_eq!(got.unwrap().0, "other");
        // --no-universe forces the host context.
        let no_uni = UniverseFlags { universe: None, no_universe: true };
        assert!(resolve_run_universe(&machine, &cfg_coerced, &no_uni, None, false).unwrap().is_none());
        // 2. Build-universe coercion beats the script's own universe.
        let got = resolve_run_universe(&machine, &cfg_coerced, &no_flags, Some("other"), false).unwrap();
        assert_eq!(got.unwrap().0, "sif");
        // Opt-out (§7.8) falls through to the script.
        let mut cfg_optout = cfg_coerced.clone();
        cfg_optout.coerce_run_universe = false;
        let got = resolve_run_universe(&machine, &cfg_optout, &no_flags, Some("other"), false).unwrap();
        assert_eq!(got.unwrap().0, "other");
        // 3. The runscript's own universe → 5. none (no declared host here).
        let got = resolve_run_universe(&machine, &cfg_plain, &no_flags, Some("sif"), false).unwrap();
        assert_eq!(got.unwrap().0, "sif");
        assert!(resolve_run_universe(&machine, &cfg_plain, &no_flags, None, false).unwrap().is_none());
        // Unknown names are hard errors listing the known set (§4.8).
        let bad = UniverseFlags { universe: Some("ghost".to_owned()), no_universe: false };
        let err = resolve_run_universe(&machine, &cfg_plain, &bad, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("sif"), "{err:#}");

        // 5'. A DECLARED host becomes the final fallback of both chains
        // (§4.8); --no-universe still bypasses it.
        let host: Universe = toml::from_str("run-env-setup = \"module load x\"").unwrap();
        machine.meta.universes.insert("host".to_owned(), host);
        let got = resolve_run_universe(&machine, &cfg_plain, &no_flags, None, false).unwrap();
        assert_eq!(got.unwrap().0, "host");
        assert!(resolve_run_universe(&machine, &cfg_plain, &no_uni, None, false).unwrap().is_none());
        let got = resolve_submit_universe(&machine, None).unwrap();
        assert_eq!(got.unwrap().0, "host");
        // With a script universe, host stays out of the way.
        let got = resolve_submit_universe(&machine, Some("sif")).unwrap();
        assert_eq!(got.unwrap().0, "sif");
    }

    #[test]
    fn env_prepend_placement_per_script_kind() {
        // Runscripts: right after the shebang (§6.1).
        let script = "#!/bin/bash\necho hi\n";
        let out = prepend_env(script, "module load x", ScriptKind::Run);
        assert_eq!(out, "#!/bin/bash\nmodule load x\necho hi\n");
        assert_eq!(prepend_env("echo hi\n", "E", ScriptKind::Run), "E\necho hi\n");
        assert_eq!(prepend_env(script, "", ScriptKind::Run), script);
        // Submitscripts: after the whole leading '#'-or-blank block, so
        // scheduler directives are never preceded by executable lines (§6.1);
        // an interior blank line does not split the header.
        let sub = "#!/bin/bash\n#SBATCH -N 1\n\n#SBATCH -p gpu\necho go\n";
        assert_eq!(
            prepend_env(sub, "E", ScriptKind::Submit),
            "#!/bin/bash\n#SBATCH -N 1\n\n#SBATCH -p gpu\nE\necho go\n"
        );
        assert_eq!(prepend_env(sub, "", ScriptKind::Submit), sub);
        assert_eq!(prepend_env("echo go\n", "E", ScriptKind::Submit), "E\necho go\n");
        // All-header script: env appended at the end.
        assert_eq!(
            prepend_env("#!/bin/sh\n# nothing else\n", "E", ScriptKind::Submit),
            "#!/bin/sh\n# nothing else\nE\n"
        );
        assert_eq!(prepend_env("#c", "E", ScriptKind::Submit), "#c\nE\n");
    }

    #[test]
    fn shell_quoting() {
        assert_eq!(shell_quote("/a b/c"), "'/a b/c'");
        assert_eq!(shell_quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn a_script_runs_by_the_interpreter_its_first_line_names() {
        let q = "'/w/run-script'";
        assert_eq!(interpreter_command("#!/bin/bash\necho\n", q), "'/bin/bash' '/w/run-script'");
        assert_eq!(interpreter_command("#! /bin/sh -e \n", q), "'/bin/sh' '-e' '/w/run-script'");
        // The rest of the line is one argument, as the kernel gives it.
        assert_eq!(interpreter_command("#!/usr/bin/env bash -x\n", q), "'/usr/bin/env' 'bash -x' '/w/run-script'");
        assert_eq!(interpreter_command("echo\n", q), "/bin/sh '/w/run-script'");
        assert_eq!(interpreter_command("#!\n", q), "/bin/sh '/w/run-script'");
        // As the kernel has it: a carriage return is part of the name, and a
        // relative interpreter is a path from here.
        assert_eq!(interpreter_command("#!/bin/sh\r\n", q), "'/bin/sh\r' '/w/run-script'");
        assert_eq!(interpreter_command("#!bash\n", q), "'./bash' '/w/run-script'");
    }

    /// A script this process still holds open for writing — as a child that
    /// another thread forked a moment ago may — runs all the same: it is
    /// read by its interpreter, not exec'd ("Text file busy").
    #[test]
    fn a_script_open_for_writing_still_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let script = tmp.path().join("run-script");
        write_executable(&script, "#!/bin/sh\necho ran \"$0\"\n").unwrap();
        let _held = fs::OpenOptions::new().append(true).open(&script).unwrap();
        let out = script_command(&script, None, &VarSet::default(), tmp.path()).unwrap().output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("ran {}\n", script.display()));
    }

    /// §8.2 step 6: `sim create` freezes the config record — the commit set
    /// the binary was built from, its flags and flavor — so a later rebuild
    /// of the config, which overwrites that record, cannot take it away.
    #[test]
    fn create_freezes_the_config_record_against_rebuilds() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let cfg_path = inst.cactus_root().join("configs/sim/cactup-config.toml");
        let with_sources = |build_id: &str, head: &str| {
            format!(
                "schema = 2\nname = \"sim\"\nflavor = \"default\"\nthornlist = \"installation-default.th\"\n\
                 machine = \"fake\"\nconfig-id = \"config-sim-1\"\nbuild-id = \"{build_id}\"\n\
                 [flags]\ndebug = true\n[sources]\ncarpetx = \"{head}\"\n"
            )
        };
        fs::write(&cfg_path, with_sources("build-sim-1", "3f2c")).unwrap();
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "ActiveThorns = \"IOUtil\"\n").unwrap();
        let sim = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();

        // The config is rebuilt from moved sources.
        fs::write(&cfg_path, with_sources("build-sim-2", "9a9a")).unwrap();

        let frozen = sim.frozen_config().unwrap().expect("create froze the record");
        assert_eq!(frozen.build_id, sim.meta.build_id);
        assert_eq!(frozen.build_id, "build-sim-1");
        assert!(frozen.flags.debug);
        assert_eq!(frozen.sources.unwrap().get("carpetx").map(String::as_str), Some("3f2c"));

        // A simulation from before the record was frozen reads as "none".
        fs::remove_file(sim.dir.join(".cactup/cfg/cactup-config.toml")).unwrap();
        assert!(sim.frozen_config().unwrap().is_none());
    }

    fn export_args(sim: &str, file: &Path) -> crate::args::SimExportArgs {
        crate::args::SimExportArgs {
            sim: sim.to_owned(),
            file: Some(file.to_owned()),
            describe: crate::args::SimDescribeArgs {
                force: true,
                yes: true,
                authors: vec!["A. Author".to_owned()],
                license: Some("CC-BY-4.0".to_owned()),
                ..Default::default()
            },
            allow_dirty: false,
            allow_unpushed: false,
            trim: false,
        }
    }

    /// `sim export` (§8.9) end to end: the pins come from the build the
    /// simulation froze, the parfile's required custom knob becomes an
    /// input, its leading comment the summary and description; and sources
    /// that were dirty, or whose commit no remote has, are refused unless
    /// explicitly allowed.
    #[test]
    fn export_writes_a_sim_file_from_what_the_simulation_froze() {
        use cactup_ecosystem_common::sim::SimFile;
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));

        // A real repo behind the install, cloned from an upstream, as a fetch leaves it.
        let upstream = tmp.path().join("upstream/carpetx");
        crate::fetch::git::testrepo::init(&upstream);
        let built = crate::fetch::git::testrepo::commit_tree(&upstream, &[("Makefile", "all:\n", false)]);
        let (branch, _) = crate::fetch::git::head_of(&upstream).unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let repo_dir = inst.cactus_root().join("repos/carpetx");
        let mut progress = prodash::tree::Root::new().add_child("test clone");
        crate::fetch::git::clone(&url, Some(&branch), &repo_dir, &mut progress).unwrap();

        let cfg_dir = inst.cactus_root().join("configs/sim");
        let list = format!(
            "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n!TARGET = $ROOT/arrangements\n!TYPE = git\n\
             !URL = {url}\n!REPO_BRANCH = {branch}\n!REPO_PATH = $2\n!CHECKOUT = CarpetX/Loop\n"
        );
        fs::write(cfg_dir.join("cactup-thornlist.src.th"), &list).unwrap();
        fs::write(cfg_dir.join("cactup-thornlist.th"), format!("{list}#DISABLED Machine/Off\n")).unwrap();
        let record = |state: &str| {
            format!(
                "schema = 2\nname = \"sim\"\nflavor = \"default\"\nthornlist = \"/x/bhns.th\"\n\
                 machine = \"fake\"\nconfig-id = \"config-sim-1\"\nbuild-id = \"build-sim-1\"\n\
                 [sources]\ncarpetx = \"{state}\"\n"
            )
        };
        let cfg_path = cfg_dir.join("cactup-config.toml");
        let parfile = tmp.path().join("bhns.par");
        fs::write(
            &parfile,
            "\n# BHNS merger, GW230529-like\n# dense output\nActiveThorns = \"IOUtil\"\n\
             K::f = \"@KNOB(kadath-initial-data)@\"\nA::b = \"@KNOB-OPTIONAL(mail, none)@ @WALLTIME_HOURS@\"\n",
        )
        .unwrap();
        let out = tmp.path().join("out.sim");
        let export = |sim: &str, args: crate::args::SimExportArgs| {
            crate::sim::export::export_from(&ctx, &inst, crate::args::SimExportArgs { sim: sim.to_owned(), ..args })
        };
        let create = |name: &'static str| {
            let req = crate::sim::CreateRequest { name, ..create_req(&parfile) };
            crate::sim::create(&ctx, &machine, &inst, &req).unwrap();
        };

        // Clean, published: exported, fully specified.
        fs::write(&cfg_path, record(&built.to_string())).unwrap();
        create("bhns");
        export("bhns", export_args("bhns", &out)).unwrap();
        let file = SimFile::from_toml(&fs::read_to_string(&out).unwrap()).unwrap();
        assert!(file.is_fully_specified(), "{:?}", file.unspecified());
        assert_eq!(file.sim.root, "Cactus");
        assert_eq!(file.sim.summary.as_deref(), Some("BHNS merger, GW230529-like"));
        assert_eq!(file.sim.description.as_deref(), Some("BHNS merger, GW230529-like\ndense output"));
        assert_eq!(file.thornlist.filename, "bhns.th");
        assert_eq!(file.thornlist.text, list, "the thornlist goes in verbatim");
        assert_eq!(file.repos.len(), 1);
        assert_eq!(file.repos[0].commit.as_deref(), Some(built.to_string().as_str()));
        assert_eq!(file.repos[0].git_ref.as_deref(), Some(branch.as_str()));
        let inputs: Vec<&str> = file.inputs.iter().map(|i| i.knob.as_str()).collect();
        assert_eq!(inputs, ["kadath-initial-data"], "optional and standard knobs are not inputs");
        assert_eq!(file.parfile.vars, ["WALLTIME_HOURS"]);
        let provenance = file.provenance.unwrap();
        assert_eq!(provenance.machine.as_deref(), Some("fake"));
        assert_eq!(provenance.flavor.as_deref(), Some("default"));
        assert_eq!(provenance.machine_disabled, ["Machine/Off"]);

        // Built from edited sources: refused, or marked dirty when allowed.
        fs::write(&cfg_path, record(&format!("{built}+1mod@1700"))).unwrap();
        create("dirty");
        let err = format!("{:#}", export("dirty", export_args("dirty", &out)).unwrap_err());
        assert!(err.contains("--allow-dirty") && err.contains("carpetx"), "{err}");
        export("dirty", crate::args::SimExportArgs { allow_dirty: true, ..export_args("dirty", &out) }).unwrap();
        let file = SimFile::from_toml(&fs::read_to_string(&out).unwrap()).unwrap();
        assert!(file.repos[0].dirty && !file.is_fully_specified());

        // Built from a commit no remote has: refused unless allowed.
        let local = crate::fetch::git::testrepo::commit(&repo_dir, "local only");
        fs::write(&cfg_path, record(&local.to_string())).unwrap();
        create("unpushed");
        let err = format!("{:#}", export("unpushed", export_args("unpushed", &out)).unwrap_err());
        assert!(err.contains("--allow-unpushed"), "{err}");
        export("unpushed", crate::args::SimExportArgs { allow_unpushed: true, ..export_args("unpushed", &out) })
            .unwrap();

        // Unpack: the thornlist comes back with its pin, the parfile verbatim.
        export("bhns", export_args("bhns", &out)).unwrap();
        let unpacked = tmp.path().join("unpacked");
        crate::sim::export::unpack(&out, &unpacked, false).unwrap();
        let list = crate::thornlist::parse(&fs::read_to_string(unpacked.join("bhns.th")).unwrap()).unwrap();
        assert_eq!(list.components()[0].commit.as_deref(), Some(built.to_string().as_str()));
        assert_eq!(fs::read_to_string(unpacked.join("bhns.par")).unwrap(), fs::read_to_string(&parfile).unwrap());
        assert!(crate::sim::export::unpack(&out, &unpacked, false).is_err(), "no silent overwrite");
    }


    /// `sim pack` (§8.9) without an installation: each repo pinned to the
    /// commit its branch names on the remote now, a repo the list already
    /// pins kept at its pin, and a remote that does not have the branch
    /// failing the whole pack.
    #[test]
    fn pack_pins_a_loose_thornlist_to_todays_tips() {
        use cactup_ecosystem_common::sim::SimFile;
        let tmp = tempfile::tempdir().unwrap();
        let mut ctx = fake_ctx(&tmp.path().join("db"));
        ctx.globals.installation = None;
        let upstream = tmp.path().join("upstream/carpetx");
        crate::fetch::git::testrepo::init(&upstream);
        let old = crate::fetch::git::testrepo::commit_tree(&upstream, &[("v", "1\n", false)]);
        let tip = crate::fetch::git::testrepo::commit_tree(&upstream, &[("v", "2\n", false)]);
        let (branch, _) = crate::fetch::git::head_of(&upstream).unwrap();
        let url = upstream.to_string_lossy().into_owned();
        let other = tmp.path().join("upstream/other");
        crate::fetch::git::testrepo::init(&other);
        crate::fetch::git::testrepo::commit_tree(&other, &[("w", "1\n", false)]);
        let other_url = other.to_string_lossy().into_owned();

        let th = tmp.path().join("bhns.th");
        let par = tmp.path().join("bhns.par");
        let section = |url: &str, branch: &str, pin: &str, checkout: &str| {
            format!("!TARGET = $ROOT/arrangements\n!TYPE = git\n!URL = {url}\n!REPO_BRANCH = {branch}\n{pin}!CHECKOUT = {checkout}\n")
        };
        fs::write(
            &th,
            format!(
                "!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}{}",
                section(&url, &branch, "", "CarpetX/Loop"),
                section(&other_url, &branch, &format!("!REPO_COMMIT = {old}\n"), "Other/Thorn"),
            ),
        )
        .unwrap();
        fs::write(&par, "# A test run\nActiveThorns = \"Loop\"\n").unwrap();
        let out = tmp.path().join("out.sim");
        let args = |th: &Path| crate::args::SimPackArgs {
            thornlist: th.to_owned(),
            parfile: par.clone(),
            file: Some(out.clone()),
            describe: crate::args::SimDescribeArgs { force: true, yes: true, ..Default::default() },
            allow_dirty: false,
            allow_unpushed: false,
            trim: false,
        };
        crate::sim::export::pack(&ctx, args(&th)).unwrap();
        let file = SimFile::from_toml(&fs::read_to_string(&out).unwrap()).unwrap();
        assert!(file.is_fully_specified(), "{:?}", file.unspecified());
        assert_eq!(file.sim.name, "bhns");
        assert_eq!(file.sim.summary.as_deref(), Some("A test run"));
        let commit = |repo: &str| file.repos.iter().find(|r| r.name == repo).unwrap().commit.clone().unwrap();
        assert_eq!(commit("carpetx"), tip.to_string(), "pinned to the branch's tip today");
        assert_eq!(commit("other"), old.to_string(), "a pin the list sets is kept");
        assert!(file.run.is_none());

        // An !INCLUDE is spliced in: the .sim carries the whole list.
        let part = tmp.path().join("other.part");
        fs::write(&part, section(&other_url, &branch, &format!("!REPO_COMMIT = {old}\n"), "Other/Thorn")).unwrap();
        let including = tmp.path().join("including.th");
        fs::write(
            &including,
            format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}!INCLUDE = other.part\n", section(&url, &branch, "", "CarpetX/Loop")),
        )
        .unwrap();
        crate::sim::export::pack(&ctx, args(&including)).unwrap();
        let file = SimFile::from_toml(&fs::read_to_string(&out).unwrap()).unwrap();
        assert!(!file.thornlist.text.contains("!INCLUDE"), "{}", file.thornlist.text);
        assert!(file.thornlist.text.contains("Other/Thorn"));
        assert!(file.is_fully_specified(), "{:?}", file.unspecified());

        // A branch the remote does not have fails the pack.
        let bad = tmp.path().join("bad.th");
        fs::write(&bad, format!("!CRL_VERSION = 1.0\n!DEFINE ROOT = Cactus\n{}", section(&url, "no-such-branch", "", "CarpetX/Loop"))).unwrap();
        let err = format!("{:#}", crate::sim::export::pack(&ctx, args(&bad)).unwrap_err());
        assert!(err.contains("carpetx") && err.contains("no-such-branch"), "{err}");
    }


    /// §5.1: a simulation's own knob values outrank the global ones in what
    /// its restarts freeze — the parfile's input is the simulation's, set by
    /// `cactup import` — while the global knob itself is left alone.
    #[test]
    fn a_simulations_own_knobs_outrank_the_global_ones() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "K::f = \"@KNOB(kadath-initial-data)@\"\n").unwrap();
        let mut sim = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();
        ctx.db
            .update(|db| {
                db.set_knob("kadath-initial-data", "/global/id.info".to_owned());
                Ok(())
            })
            .unwrap();
        sim.set_knobs([("kadath-initial-data".to_owned(), "/sim/id.info".to_owned())].into_iter().collect()).unwrap();
        let sim = crate::sim::Simulation::open("bbh", &sim.dir).unwrap();
        assert_eq!(sim.meta.knobs["kadath-initial-data"], "/sim/id.info", "stored with the simulation");

        let db = ctx.db.read().unwrap();
        submit_impl(&inst, &machine, &db, &sim, &start_args("bbh", "1:00:00"), false, Some("testhost")).unwrap();
        let id = *restart::list_ids(&sim.dir).unwrap().last().unwrap();
        let r = Restart::load(&sim.dir, id).unwrap();
        assert_eq!(r.meta.knobs["kadath-initial-data"], "/sim/id.info");
        assert_eq!(ctx.db.read().unwrap().knob("kadath-initial-data"), Some("/global/id.info"), "the global knob is untouched");
    }

    /// The allocation a restart freezes, as `@KNOB(allocation)@` and as
    /// `@ALLOCATION@` alike (§5.1): the command line (`-a`, shorthand for
    /// `-K allocation=`) over the simulation's own knob over the global one.
    #[test]
    fn the_allocation_is_the_command_line_then_the_sim_then_the_global_knob() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "A::b = 1\n").unwrap();
        let mut sim = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();
        ctx.db
            .update(|db| {
                db.set_knob("allocation", "global_acct".to_owned());
                Ok(())
            })
            .unwrap();
        let frozen = |argv: &[&str], sim: &crate::sim::Simulation| {
            let mut db = ctx.db.read().unwrap();
            db.apply_overlay(&<crate::args::Args as clap::Parser>::try_parse_from(argv).unwrap().knob_overlay().unwrap());
            submit_impl(&inst, &machine, &db, sim, &start_args("bbh", "1:00:00"), false, Some("testhost")).unwrap();
            let id = *restart::list_ids(&sim.dir).unwrap().last().unwrap();
            let meta = Restart::load(&sim.dir, id).unwrap().meta;
            let var = meta.vars.get("ALLOCATION").and_then(|v| v.as_str()).unwrap_or_default().to_owned();
            (meta.knobs.get("allocation").cloned().unwrap_or_default(), var, meta.allocation.unwrap_or_default())
        };
        let plain = ["cactup", "sim", "submit", "bbh"];
        let with_a = ["cactup", "sim", "submit", "bbh", "-a", "cli_acct"];
        let with_k = ["cactup", "-K", "allocation=cli_acct", "sim", "submit", "bbh"];
        let all = |a: &str| (a.to_owned(), a.to_owned(), a.to_owned());

        assert_eq!(frozen(&plain, &sim), all("global_acct"), "the global knob alone");
        assert_eq!(frozen(&with_a, &sim), all("cli_acct"), "-a over the global knob");
        sim.set_knobs([("allocation".to_owned(), "sim_acct".to_owned())].into_iter().collect()).unwrap();
        let sim = crate::sim::Simulation::open("bbh", &sim.dir).unwrap();
        assert_eq!(frozen(&plain, &sim), all("sim_acct"), "the simulation's own over the global knob");
        assert_eq!(frozen(&with_a, &sim), all("cli_acct"), "-a over the simulation's own");
        assert_eq!(frozen(&with_k, &sim), all("cli_acct"), "-K allocation= the same");
    }

    /// `cactup import x.sim --submit --allocation acct` on a machine that
    /// requires an account: import's --allocation joins the knob overlay as
    /// main installs it (`Args::knob_overlay`, through `Commands::topology`),
    /// and the `sim submit` import starts renders it into the account line.
    /// Without it the same submit refuses.
    #[test]
    fn imports_allocation_reaches_the_account_line() {
        use crate::args::{Args, Commands, SimCommand};
        use cactup_ecosystem_common::sim::{Parfile, ParfileKind, SimFile, SimMeta, ThornlistText};
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        fs::write(
            machine.dir.join("submitscripts/default.sh"),
            "#!/bin/sh\n#PBS -A @KNOB(allocation)@\nexec @CACTUP@ sim run @SIMULATION_NAME@ --installation=@ALIAS@ \
             --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ --restart-id=@RESTART_ID@\n",
        )
        .unwrap();
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "A::b = 1\n").unwrap();
        let sim = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();
        let sim_file = SimFile {
            format: cactup_ecosystem_common::SIM_FORMAT,
            sim: SimMeta {
                name: "bbh".into(),
                summary: None,
                description: None,
                authors: vec![],
                license: None,
                contact: None,
                cactarium: None,
                root: "Cactus".into(),
            },
            thornlist: ThornlistText { filename: "bbh.th".into(), text: String::new(), untracked: vec![] },
            parfile: Parfile { filename: "bbh.par".into(), kind: ParfileKind::Par, vars: vec![], text: String::new() },
            inputs: vec![],
            repos: vec![],
            run: None,
            provenance: None,
        };
        // What main does with the command line, then what import does.
        let import = |argv: &[&str]| {
            let args = <Args as clap::Parser>::try_parse_from(argv).unwrap();
            let mut db = ctx.db.read().unwrap();
            db.apply_overlay(&args.knob_overlay().unwrap());
            let Commands::Import(import) = args.command else { panic!("not an import") };
            let Commands::Sim(SimCommand::Submit(start)) =
                crate::commands::import::started_command(&sim_file, &import, "bbh").unwrap()
            else {
                panic!("not a sim submit")
            };
            submit_impl(&inst, &machine, &db, &sim, &start, false, Some("testhost"))
        };

        let err = import(&["cactup", "import", "bbh.sim", "--submit"]).unwrap_err();
        assert!(format!("{err:#}").contains("pass -a VALUE or -K allocation=VALUE"), "{err:#}");
        assert!(restart::list_ids(&sim.dir).unwrap().is_empty(), "the refusal left nothing");

        import(&["cactup", "import", "bbh.sim", "--submit", "--allocation", "acct"]).unwrap();
        let id = *restart::list_ids(&sim.dir).unwrap().last().unwrap();
        let script = fs::read_to_string(restart::restart_dir(&sim.dir, id).join(".cactup/submit-script")).unwrap();
        assert!(script.lines().any(|l| l == "#PBS -A acct"), "{script}");
    }

    /// The overlay is the one way `-a` reaches a command: a flag it never
    /// saw is refused rather than silently dropped.
    #[test]
    fn an_allocation_flag_the_overlay_never_saw_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "A::b = 1\n").unwrap();
        let sim = crate::sim::create(&ctx, &machine, &inst, &create_req(&parfile)).unwrap();
        let mut args = start_args("bbh", "1:00:00");
        args.topology.allocation = Some("acct".to_owned());
        let db = ctx.db.read().unwrap();
        let err = submit_impl(&inst, &machine, &db, &sim, &args, false, Some("testhost")).unwrap_err();
        assert!(format!("{err:#}").contains("--allocation acct did not reach the allocation knob"), "{err:#}");
        assert!(restart::list_ids(&sim.dir).unwrap().is_empty());
    }

    /// The account directive of a scheduler header line (`#SBATCH -A x`,
    /// `#SBATCH --account=x`, `#PBS -A x`), as its value — `Some("")` when
    /// the directive is there with nothing after it.
    fn account_directive(line: &str) -> Option<String> {
        let line = line.trim();
        if !line.starts_with("#SBATCH") && !line.starts_with("#PBS") {
            return None;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        tokens.iter().enumerate().find_map(|(i, t)| match *t {
            "-A" | "--account" => Some(tokens.get(i + 1).copied().unwrap_or("").to_owned()),
            _ => t.strip_prefix("--account=").map(str::to_owned),
        })
    }

    /// A variable set broad enough for every in-repo submit and build-submit
    /// script, and the runscripts that pass an allocation to `srun`, on
    /// `queue`, with `db`'s knobs: `@ALLOCATION@` and the snapshot, as the
    /// submit paths bind them.
    fn sweep_vars(machine: &Machine, queue: &str, db: &Database) -> VarSet {
        let mut v = VarSet::new();
        vars::set_machine_vars(&mut v, machine, queue, None).unwrap();
        for (var, n) in [
            ("NODES", 1u64), ("TASKS", 1), ("TASKS_PER_NODE", 1), ("CPUS_PER_TASK", 1),
            ("GPUS_PER_TASK", 1), ("RESTART_ID", 0), ("RESULTS_ID", 0), ("ATTEMPT_ID", 0), ("MAKEJOBS", 1),
        ] {
            v.set(var, n);
        }
        v.set("GPU", true);
        v.set("QUEUE", machine.meta.scheduler_queue_name(queue).unwrap());
        v.set("ALLOCATION", db.knob("allocation").unwrap_or_default());
        for var in ["EMAIL", "MAIL", "CHAINED_JOB_ID", "JOB_ID"] {
            v.set(var, "");
        }
        for var in [
            "STDOUT_FILE", "STDERR_FILE", "CACTUP", "SIMULATION_NAME", "SHORT_SIMULATION_NAME", "SIMULATION_DIR",
            "SOURCEDIR", "ALIAS", "TEST_NAME", "TEST_DIR", "RUNDIR", "CONFIGURATION", "CONFIG_DIR",
            "JOB_NAME", "MAIL_TYPE", "USER", "HOSTNAME", "SCRIPTFILE", "SCRATCH_HOME", "EXECUTABLE", "PARFILE",
            "SIMULATION_ID", "TESTSUITE_RESULTS_DIR", "TESTSUITE_SELECT",
        ] {
            v.set(var, format!("<{}>", var.to_lowercase()));
        }
        v.set("MACHINE", machine.name.as_str());
        vars::set_walltime_vars(&mut v, Walltime::parse("1:00:00").unwrap(), Walltime::parse("0:05:00").unwrap());
        v.set_knobs(db.knob_snapshot());
        v
    }

    /// The knobs of a command: none set; `-a acct_x` on the command line,
    /// through the real overlay; or the stored knob.
    fn sweep_dbs() -> [(&'static str, Database); 3] {
        let mut by_flag = Database::new();
        let argv = ["cactup", "sim", "submit", "s", "-a", "acct_x"];
        by_flag.apply_overlay(&<crate::args::Args as clap::Parser>::try_parse_from(argv).unwrap().knob_overlay().unwrap());
        let mut by_knob = Database::new();
        by_knob.set_knob("allocation", "acct_x".to_owned());
        [("no allocation", Database::new()), ("-a alone", by_flag), ("the knob alone", by_knob)]
    }

    /// The queue a script's route serves first, else the machine's default.
    fn sweep_queue(machine: &Machine, route: &crate::mdb::meta::Route) -> String {
        route
            .queues
            .first()
            .cloned()
            .or_else(|| machine.meta.default_queue(None).map(str::to_owned))
            .or_else(|| machine.meta.queues.keys().next().cloned())
            .unwrap()
    }

    fn sweep_mdb() -> crate::mdb::Mdb {
        crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        )
    }

    /// The bug a real QB4 test hit: with no allocation, a script wrote
    /// `#SBATCH -A ` and sbatch read the next directive as the account. Every
    /// in-repo submit and build-submit script, rendered with no allocation,
    /// must leave the directive out (a machine where it is optional) or
    /// refuse naming `-a` (one where it is required). Given one, by `-a`
    /// alone or by the knob alone — the same knob now, so the two agree
    /// trivially — every account line it writes carries it.
    #[test]
    fn no_mdb_script_writes_an_empty_account_directive() {
        let mdb = sweep_mdb();
        let mut directives = 0;
        for name in mdb.machines().unwrap().into_keys() {
            let machine = mdb.load(&name).unwrap();
            for kind in [ScriptKind::Submit, ScriptKind::BuildSubmit] {
                for (script, route) in &machine.meta.scripts(kind).routes {
                    let queue = sweep_queue(&machine, route);
                    let [(_, none), (_, by_flag), (_, by_knob)] = sweep_dbs();
                    let render = |db: &Database| {
                        generate_script(&machine, kind, script, &sweep_vars(&machine, &queue, db), Phase::Submit, None)
                    };
                    let what = format!("{name} {} script {script}", kind.dir_name());
                    match render(&none) {
                        Ok(text) => {
                            if let Some(line) = text.lines().find(|l| account_directive(l).is_some()) {
                                panic!("{what}: no allocation, yet it writes `{line}`");
                            }
                        }
                        Err(e) => {
                            // A .py script's own refusal, or @KNOB(allocation)@'s.
                            let e = format!("{e:#}");
                            assert!(
                                e.contains(&format!("{name} needs an allocation: pass -a ACCOUNT, or set it once with"))
                                    || e.contains("knob allocation is unset or empty; set it with `cactup knob \
                                                   allocation VALUE` or pass -a VALUE"),
                                "{what}: {e}"
                            );
                        }
                    }
                    let lines = |db: &Database| -> Vec<String> {
                        let text = render(db).unwrap_or_else(|e| panic!("{what}: {e:#}"));
                        text.lines().filter(|l| account_directive(l).is_some()).map(str::to_owned).collect()
                    };
                    let given = lines(&by_flag);
                    for line in &given {
                        let value = account_directive(line).unwrap();
                        assert!(value.contains("acct_x"), "{what}: account directive `{line}`");
                    }
                    // A script that reads the allocation at all (a knob
                    // token, `knob(…)`, or the legacy alias) must write it.
                    let template = fs::read_to_string(machine.script_path(kind, script).unwrap().path).unwrap();
                    if ["(allocation)", "knob(\"allocation\"", "ALLOCATION"].iter().any(|r| template.contains(r)) {
                        assert!(!given.is_empty(), "{what}: reads the allocation but writes no account line");
                    }
                    assert_eq!(lines(&by_knob), given, "{what}: -a alone and the knob alone");
                    directives += given.len();
                }
            }
        }
        // The sweep saw the scripts it is about.
        assert!(directives > 25, "only {directives} account directives rendered");
    }

    /// A runscript that hands the allocation to `srun -A` (qbc, db1) runs
    /// inside the job, which already has an account: with none set the `-A`
    /// must vanish, not leave `srun -A -p …` taking `-p` as the account. The
    /// `-A` sits mid-line, so a drop-line token cannot do it: each such
    /// runscript binds `alloc='@KNOB-OPTIONAL(allocation)@'` and passes
    /// `${alloc:+-A …}`; the shell's own expansion is checked here.
    #[test]
    fn runscripts_pass_srun_a_only_with_an_allocation() {
        let mdb = sweep_mdb();
        let mut checked = Vec::new();
        for name in mdb.machines().unwrap().into_keys() {
            let machine = mdb.load(&name).unwrap();
            for (script, route) in &machine.meta.scripts(ScriptKind::Run).routes {
                let file = machine.script_path(ScriptKind::Run, script).unwrap();
                let template = fs::read_to_string(&file.path).unwrap();
                if !template.contains("ALLOCATION") && !template.contains("(allocation") {
                    continue;
                }
                let what = format!("{name} runscript {script}");
                let queue = sweep_queue(&machine, route);
                for (label, db) in sweep_dbs() {
                    let flag = if label == "no allocation" { "" } else { "acct_x" };
                    let v = sweep_vars(&machine, &queue, &db);
                    let text = generate_script(&machine, ScriptKind::Run, script, &v, Phase::Run, None)
                        .unwrap_or_else(|e| panic!("{what}: {e:#}"));
                    // No `-A` token followed by another flag or by nothing.
                    for line in text.lines().filter(|l| l.contains("srun")) {
                        let tokens: Vec<&str> = line.split_whitespace().collect();
                        for (i, t) in tokens.iter().enumerate() {
                            if *t == "-A" {
                                let next = tokens.get(i + 1).copied().unwrap_or("");
                                assert!(!next.is_empty() && !next.starts_with('-') && next != "\\", "{what}: `{line}`");
                            }
                        }
                    }
                    let alloc = text.lines().find(|l| l.starts_with("alloc=")).unwrap_or_else(|| panic!("{what}: no alloc= line"));
                    let out = Command::new("bash")
                        .arg("-c")
                        .arg(format!("{alloc}\nset -- ${{alloc:+-A \"$alloc\"}}; echo \"$#:$*\""))
                        .output()
                        .unwrap();
                    let expect = if flag.is_empty() { "0:\n".to_owned() } else { format!("2:-A {flag}\n") };
                    assert_eq!(String::from_utf8_lossy(&out.stdout), expect, "{what} with {label}");
                    // Each `-A` use goes through the guard, never the bare value.
                    assert!(!text.contains("-A acct_x") && !text.contains("-A  "), "{what}: an unguarded -A");
                    let code = text.lines().filter(|l| !l.trim_start().starts_with('#'));
                    for line in code.filter(|l| l.contains("srun") && l.contains("-A")) {
                        assert!(line.contains("${alloc:+-A "), "{what}: `{line}`");
                    }
                }
                checked.push(what);
            }
        }
        for want in [
            "qbc.loni.org runscript default", "qbc.loni.org runscript test",
            "db1.hpc.lsu.edu runscript sing", "db1.hpc.lsu.edu runscript sing-test",
        ] {
            assert!(checked.iter().any(|c| c == want), "{want} not swept: {checked:?}");
        }
    }
}
