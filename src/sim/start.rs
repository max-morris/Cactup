//! `sim submit` (§8.3) and `sim run` (§8.4): restart preparation, script
//! generation, universes (§4.8), auto-recovery + walltime chaining (§8.8),
//! and the compute-node path (§8.3.1).

use crate::args::{SimRunArgs, SimStartArgs, UniverseFlags};
use crate::build::ConfigMeta;
use crate::commands::Ctx;
use crate::database::{Database, SCHEMA};
use crate::installation::Installation;
use crate::lock::{LinkLock, HEARTBEAT_SECS};
use crate::mdb::{discover, Machine, Phase, ScriptKind, Universe, WrappedCommand, HOST_UNIVERSE};
use crate::scheduler::Scheduler;
use crate::sim::restart::{self, Restart, RestartMeta, UniverseSpec, NO_JOB_ID};
use crate::sim::{vars, Simulation};
use crate::template::VarSet;
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
        (None, false) => bail!(
            "no simulation named \"{}\" — pass a parfile to create it (§8.3)",
            args.sim
        ),
        (Some(par), false) => crate::sim::create(
            ctx,
            machine,
            inst,
            false,
            &args.sim,
            par,
            args.config.as_deref(),
            None,
        ),
        (Some(par), true) => {
            if !(args.overwrite || args.force) {
                bail!(
                    "simulation \"{}\" already exists but a parfile was given; \
                     use --overwrite (or -f) to replace it",
                    args.sim
                );
            }
            crate::sim::create(ctx, machine, inst, true, &args.sim, par, args.config.as_deref(), None)
        }
    }
}

/// Resolve the RUN universe per the §4.8 precedence:
/// CLI → coerced build universe → runscript variant / default-universe →
/// declared host → none (a machine without `[universes.host]` keeps
/// resolving to none: implicit host ≡ identity ≡ bare execution).
pub fn resolve_run_universe<'m>(
    machine: &'m Machine,
    cfg: &ConfigMeta,
    cli: &UniverseFlags,
    variant_universe: Option<&str>,
    default_universe: Option<&str>,
    verbose: bool,
) -> Res<Option<(String, &'m Universe)>> {
    if cli.no_universe {
        return Ok(None);
    }
    if let Some(name) = &cli.universe {
        return Ok(Some((name.clone(), machine.meta.universe(name)?)));
    }
    if let Some(build_uni) = &cfg.universe {
        if cfg.coerce_run_universe {
            let u = machine.meta.universe(build_uni).with_context(|| {
                format!(
                    "config \"{}\" was built in universe \"{build_uni}\", which this machine no \
                     longer defines; pass --no-universe or rebuild (§4.8)",
                    cfg.name
                )
            })?;
            if verbose {
                if let Some(v) = variant_universe {
                    if v != build_uni {
                        eprintln!(
                            "{} runscript variant names universe \"{v}\" but the config's build \
                             universe \"{build_uni}\" takes precedence (§4.8)",
                            "note:".yellow()
                        );
                    }
                }
            }
            return Ok(Some((build_uni.clone(), u)));
        }
    }
    let name = variant_universe.or(default_universe);
    match name {
        Some(n) => Ok(Some((n.to_owned(), machine.meta.universe(n)?))),
        None => Ok(machine.meta.declared_host().map(|u| (HOST_UNIVERSE.to_owned(), u))),
    }
}

/// Resolve the SUBMIT universe: submitscript variant → `default-universe` →
/// declared host → none (the CLI flag targets the run universe — §8.3).
pub(crate) fn resolve_submit_universe<'m>(
    machine: &'m Machine,
    variant_universe: Option<&str>,
    default_universe: Option<&str>,
) -> Res<Option<(String, &'m Universe)>> {
    match variant_universe.or(default_universe) {
        Some(n) => Ok(Some((n.to_owned(), machine.meta.universe(n)?))),
        None => Ok(machine.meta.declared_host().map(|u| (HOST_UNIVERSE.to_owned(), u))),
    }
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
        ScriptKind::Submit => {
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
/// variants are `@NAME@`-substituted with the phase env — under the phase's
/// resolved `universe`, whose env keys override the machine's (§6.1) —
/// auto-prepended; `.py` variants run per the calling convention and own
/// their env placement.
pub(crate) fn generate_script(
    machine: &Machine,
    kind: ScriptKind,
    variant: &str,
    vars: &VarSet,
    phase: Phase,
    universe: Option<&str>,
) -> Res<String> {
    let script = machine.script_path(kind, variant)?;
    if script.python {
        vars.run_py_script(&script.path)
    } else {
        let template = fs::read_to_string(&script.path)
            .with_context(|| format!("Failed to read {}", script.path.display()))?;
        let substituted = vars
            .substitute(&template)
            .with_context(|| format!("substituting {}", script.path.display()))?;
        Ok(prepend_env(&substituted, &machine.meta.effective_env(universe, phase), kind))
    }
}

pub(crate) fn write_executable(path: &Path, content: &str) -> Res<()> {
    fs::write(path, content).with_context(|| format!("Failed to write {}", path.display()))?;
    use std::os::unix::fs::PermissionsExt;
    let mut perms = fs::metadata(path)?.permissions();
    perms.set_mode(perms.mode() | 0o755);
    fs::set_permissions(path, perms)?;
    Ok(())
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
            .with_context(|| format!("Failed to read parfile master copy {}", master.display()))?;
        vars.substitute(&template)
            .with_context(|| format!("substituting parfile {}", master.display()))?
    };
    fs::write(&out, content).with_context(|| format!("Failed to write {}", out.display()))?;
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
    let cfg = ConfigMeta::load(&cactus_root, &sim.meta.configuration)?.ok_or_else(|| {
        anyhow!(
            "config \"{}\" (which created this simulation) no longer exists",
            sim.meta.configuration
        )
    })?;
    crate::commands::delta::warn_if_sources_diverged(inst, &cfg, args.silent);
    let force_queue = args.force_queue || args.force;
    let mut topo = vars::resolve_topology(&args.topology, machine, db, &cfg, force_queue)?;
    let sim_home = inst.meta()?.sim_home()?.to_owned();
    let sched = Scheduler::new(&machine.meta);

    // Per-simulation mutual exclusion for everything below (§2.3 item 3).
    let _lock = sim.lock()?;

    // Reap a stale active restart (§8.3); a live one triggers chaining.
    restart::reap_stale(sim, &sched)?;
    let mut prev_job: Option<String> = None;
    let mut activate_first = true;
    if let Some(active) = restart::active_id(&sim.dir)? {
        let r = Restart::load(&sim.dir, active)?;
        if r.meta.job_id == NO_JOB_ID {
            bail!(
                "restart {} is active but has no job and is not reapable yet; \
                 try again shortly or `cactup sim stop {}`",
                restart::dir_name(active),
                sim.name
            );
        }
        println!(
            "Restart {} is still live (job {}); chaining the new submission behind it",
            restart::dir_name(active),
            r.meta.job_id
        );
        prev_job = Some(r.meta.job_id.clone());
        activate_first = false;
    }

    // Script variants for the chosen queue (§4.4) and universes (§4.8);
    // selection is driven by the config's BUILD universe (§4.4).
    let submit_scripts = machine.meta.script_variants(ScriptKind::Submit);
    let run_scripts = machine.meta.script_variants(ScriptKind::Run);
    let cfg_universe = cfg.universe.as_deref().unwrap_or(HOST_UNIVERSE);
    let (sub_variant, sub_entry) = submit_scripts.select(&topo.queue, cfg_universe, false, None)?;
    let (run_variant, run_entry) = run_scripts.select(&topo.queue, cfg_universe, false, None)?;
    // Script-variant default tasks (§4.2): submitscript first for a submit.
    vars::apply_tasks_default(&mut topo, &args.topology, sub_entry.tasks.or(run_entry.tasks), None);
    let submit_uni = resolve_submit_universe(
        machine,
        sub_entry.universe.as_deref(),
        submit_scripts.default_universe.as_deref(),
    )?;
    let run_uni = resolve_run_universe(
        machine,
        &cfg,
        &args.universe,
        run_entry.universe.as_deref(),
        run_scripts.default_universe.as_deref(),
        verbose,
    )?;
    let run_uni_spec = run_uni.as_ref().map(|(name, u)| UniverseSpec::from_universe(name, u));
    let run_uni_name = run_uni.as_ref().map(|(name, _)| name.as_str());
    let submit_uni_name = submit_uni.as_ref().map(|(name, _)| name.as_str());

    // Automatic walltime chaining (§8.8).
    let ceiling = machine.meta.effective_max_walltime(&topo.queue)?;
    let (segments, job_wall) = vars::chain_segments(topo.total_wall, ceiling);
    let buffer = args.checkpt_buffer.unwrap_or_else(|| vars::default_checkpt_buffer(job_wall));
    if segments > 1 {
        println!(
            "Total walltime {} exceeds the {} ceiling of queue \"{}\": pre-submitting {} chained jobs (§8.8)",
            topo.total_wall.canonical(),
            ceiling.canonical(),
            topo.queue,
            segments
        );
    }

    let identity = Identity::resolve(db, hostname_override);
    let (sub_variant, run_variant) = (sub_variant.to_owned(), run_variant.to_owned());

    for seg in 0..segments {
        // Every restart gets a fresh id: the reaper (above) cleared the stale
        // active restart, so `next_id` advances the chain.
        let id = restart::next_id(&sim.dir)?;
        let rdir = restart::restart_dir(&sim.dir, id);
        fs::create_dir_all(rdir.join(".cactup"))
            .with_context(|| format!("Failed to create {}", rdir.display()))?;

        let chained = prev_job.clone();
        let vset = vars::assemble(&vars::RestartVarsInput {
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
        })?;

        // Submit-phase artifact: same vars, submit-phase ENV_SETUP under the
        // submit universe (§6.1).
        let mut submit_vars = vset.clone();
        submit_vars.set("ENV_SETUP", machine.meta.effective_env(submit_uni_name, Phase::Submit));
        let submit_script = generate_script(machine, ScriptKind::Submit, &sub_variant, &submit_vars, Phase::Submit, submit_uni_name)?;
        write_executable(&rdir.join(".cactup").join("submit-script"), &submit_script)?;

        // Run-phase artifact, frozen now so the compute node never touches
        // the MDB (§8.3.1).
        let run_script = generate_script(machine, ScriptKind::Run, &run_variant, &vset, Phase::Run, run_uni_name)?;
        write_executable(&rdir.join(".cactup").join("run-script"), &run_script)?;

        let mut r = Restart {
            id,
            dir: rdir.clone(),
            meta: RestartMeta {
                schema: SCHEMA,
                created: Utc::now(),
                started: None,
                finished: None,
                nodes: topo.nodes,
                tasks: topo.tasks,
                tpn: topo.tpn,
                cpus: topo.cpus,
                queue: topo.queue.clone(),
                allocation: topo.allocation.clone(),
                walltime: job_wall,
                checkpt_buffer: buffer,
                job_id: NO_JOB_ID.to_owned(),
                chained_job_id: chained,
                status: None,
                terminated: false,
                universe: run_uni_spec.clone(),
                vars: restart::freeze_vars(&vset),
            },
        };
        r.store()?;

        // makeActive only for the restart that runs first (§8.3/§8.3.2);
        // chained pre-submissions stay un-activated until their handoff.
        if seg == 0 && activate_first {
            restart::make_active(&sim.dir, id)?;
        }

        let job_id = sched
            .submit(&submit_vars, submit_uni.as_ref().map(|(n, u)| (n.as_str(), *u)))
            .with_context(|| format!("submitting restart {}", restart::dir_name(id)))?;
        r.meta.job_id = job_id.clone();
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
    let cfg = ConfigMeta::load(&cactus_root, &sim.meta.configuration)?.ok_or_else(|| {
        anyhow!(
            "config \"{}\" (which created this simulation) no longer exists",
            sim.meta.configuration
        )
    })?;
    crate::commands::delta::warn_if_sources_diverged(inst, &cfg, args.start.silent);
    let force_queue = args.start.force_queue || args.start.force;
    let mut topo = vars::resolve_topology(&args.start.topology, machine, db, &cfg, force_queue)?;
    let sim_home = inst.meta()?.sim_home()?.to_owned();
    let sched = Scheduler::new(&machine.meta);

    let lock = sim.lock()?;
    restart::reap_stale(sim, &sched)?;
    if let Some(active) = restart::active_id(&sim.dir)? {
        bail!(
            "simulation \"{}\" already has an active restart ({}); wait for it, or \
             `cactup sim stop {}` first",
            sim.name,
            restart::dir_name(active),
            sim.name
        );
    }

    let run_scripts = machine.meta.script_variants(ScriptKind::Run);
    // Selection is driven by the config's BUILD universe (§4.4).
    let cfg_universe = cfg.universe.as_deref().unwrap_or(HOST_UNIVERSE);
    let (run_variant, run_entry) = run_scripts.select(&topo.queue, cfg_universe, false, None)?;
    // Script-variant default tasks (§4.2).
    vars::apply_tasks_default(&mut topo, &args.start.topology, run_entry.tasks, None);
    let run_uni = resolve_run_universe(
        machine,
        &cfg,
        &args.start.universe,
        run_entry.universe.as_deref(),
        run_scripts.default_universe.as_deref(),
        verbose,
    )?;
    let run_uni_spec = run_uni.as_ref().map(|(name, u)| UniverseSpec::from_universe(name, u));
    let run_uni_name = run_uni.as_ref().map(|(name, _)| name.as_str());

    let job_wall = topo.total_wall.min(machine.meta.effective_max_walltime(&topo.queue)?);
    let buffer = args
        .start
        .checkpt_buffer
        .unwrap_or_else(|| vars::default_checkpt_buffer(job_wall));

    let identity = Identity::resolve(db, hostname_override);
    // A fresh interactive run always gets a new id; reaping (above) cleared any
    // stale active restart.
    let id = restart::next_id(&sim.dir)?;
    let rdir = restart::restart_dir(&sim.dir, id);
    fs::create_dir_all(rdir.join(".cactup"))
        .with_context(|| format!("Failed to create {}", rdir.display()))?;

    let vset = vars::assemble(&vars::RestartVarsInput {
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
    })?;

    let run_script = generate_script(machine, ScriptKind::Run, run_variant, &vset, Phase::Run, run_uni_name)?;
    write_executable(&rdir.join(".cactup").join("run-script"), &run_script)?;

    let mut r = Restart {
        id,
        dir: rdir.clone(),
        meta: RestartMeta {
            schema: SCHEMA,
            created: Utc::now(),
            started: None,
            finished: None,
            nodes: topo.nodes,
            tasks: topo.tasks,
            tpn: topo.tpn,
            cpus: topo.cpus,
            queue: topo.queue.clone(),
            allocation: topo.allocation.clone(),
            walltime: job_wall,
            checkpt_buffer: buffer,
            // The interactive run IS the job; our pid is what `ps`-style
            // get-status commands can see (matches the generic machine).
            job_id: std::process::id().to_string(),
            chained_job_id: None,
            status: None,
            terminated: false,
            universe: run_uni_spec,
            vars: restart::freeze_vars(&vset),
        },
    };
    r.store()?;
    restart::make_active(&sim.dir, id)?;
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
    let mut r = Restart::load(&sim.dir, id)?;

    // Chain handoff (§8.3.2), under the per-simulation lock.
    {
        let _lock = sim.lock()?;
        restart::handoff_active(&sim.dir, id)?;
    }

    // No recovery step here: the parfile points Cactus at its recovery dir and
    // Cactus loads the newest checkpoint itself (§8.8).
    sim.log("run", &format!("compute-node run of {}", restart::dir_name(id)));
    execute_restart(&sim, &mut r, false)
}

/// Single-quote a string for `/bin/sh`.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Build the command that executes a stored script (with shebang → exec the
/// file; without → via `/bin/sh`), optionally wrapped in a universe (§4.8),
/// with `cwd` as the working directory.
pub(crate) fn script_command(
    script: &Path,
    universe: Option<&Universe>,
    vars: &VarSet,
    cwd: &Path,
) -> Res<Command> {
    let script_text = fs::read_to_string(script).unwrap_or_default();
    let inner = if script_text.starts_with("#!") {
        shell_quote(&script.display().to_string())
    } else {
        format!("/bin/sh {}", shell_quote(&script.display().to_string()))
    };
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

/// Execute a prepared restart: resolve the parfile, create the working dir
/// and `TERMINATE`, hold the liveness marker + heartbeat (§9.3), run the
/// stored run-script wrapped in the frozen run universe (§4.8), and record
/// completion. `tee` echoes to the terminal while writing
/// `<SimName>.{out,err}` (the §8.4 foreground behavior).
fn execute_restart(sim: &Simulation, r: &mut Restart, tee: bool) -> Res<()> {
    // The frozen variable set drives parfile resolution and universe wrapping.
    let vset = restart::thaw_vars(&r.meta.vars)?;

    let parfile = resolve_parfile(sim, &r.dir, &vset)?;
    let workdir = restart::workdir(sim, r.id);
    fs::create_dir_all(&workdir).with_context(|| format!("Failed to create {}", workdir.display()))?;
    let terminate = r.dir.join("TERMINATE");
    if !terminate.exists() {
        fs::write(&terminate, b"0\n").with_context(|| "Failed to create TERMINATE")?;
    }

    // Build the command: run-script, wrapped in the run universe if any.
    let script = r.cactup_dir().join("run-script");
    let universe = r.meta.universe.as_ref().map(|u| u.to_universe());
    let cmd = script_command(&script, universe.as_ref(), &vset, &r.dir)?;

    // Liveness (§2.3/§9.3): running.lock with heartbeat + the heartbeat file.
    let running = LinkLock::acquire(&r.running_lock_path())?.with_heartbeat();
    r.touch_heartbeat();
    r.meta.started = Some(Utc::now());
    r.meta.status = Some("R".to_owned());
    r.store()?;

    let tee_files = tee.then(|| {
        (r.dir.join(format!("{}.out", sim.name)), r.dir.join(format!("{}.err", sim.name)))
    });
    let status = spawn_and_wait(cmd, &r.heartbeat_path(), tee_files);
    drop(running);

    // Record completion whatever happened; the parfile decides its own
    // checkpoint/termination behavior — cactup only bookkeeps (§8.8).
    r.meta.finished = Some(Utc::now());
    r.meta.terminated = true;
    r.meta.status = Some("U".to_owned());
    r.store()?;
    let _ = parfile; // the ready-to-run parfile stays in the restart (§9.1)

    match status {
        Ok(st) if st.success() => {
            sim.log("run", &format!("{} finished", restart::dir_name(r.id)));
            println!("Simulation {} restart {} finished", sim.name.bold(), restart::dir_name(r.id));
            Ok(())
        }
        Ok(st) => {
            sim.log("run", &format!("{} failed ({st})", restart::dir_name(r.id)));
            bail!("the run-script exited unsuccessfully ({st})");
        }
        Err(e) => {
            sim.log("run", &format!("{} failed to start: {e:#}", restart::dir_name(r.id)));
            Err(e)
        }
    }
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
    let status = loop {
        if let Some(st) = child.try_wait()? {
            break st;
        }
        let elapsed = started.elapsed().as_secs();
        if elapsed / HEARTBEAT_SECS > last_beat {
            last_beat = elapsed / HEARTBEAT_SECS;
            let _ = fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&heartbeat)
                .and_then(|f| f.set_modified(std::time::SystemTime::now()));
        }
        std::thread::sleep(std::time::Duration::from_millis(200));
    };
    for t in copiers {
        let _ = t.join();
    }
    Ok(status)
}

#[cfg(test)]
mod tests {
    use super::*;
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
            "#!/bin/sh\n# chained: @CHAINED_JOB_ID@\n\
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

            [variants.submitscript]
            "default" = { queues = ["batch"], default = true }

            [variants.runscript]
            "default" = { queues = ["batch"], default = true }

            [variants.optionlist]
            variants = []
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
            variant = "default"
            thornlist = "einsteintoolkit.th"
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
            force_queue: false,
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
                job_name: None,
                wall_time: Some(Walltime::parse(wall).unwrap()),
                out: None,
                err: None,
            },
            checkpt_buffer: None,
        }
    }

    #[test]
    fn submit_chains_and_compute_run_executes() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));

        // Create via the real path (registry + cache + metadata).
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "ActiveThorns = \"IOUtil\"\n# sim @SIMULATION_NAME@ t=@TASKS@ lit=@@\n").unwrap();
        let sim = crate::sim::create(&ctx, &machine, &inst, false, "bbh", &parfile, None, None).unwrap();
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

        // 50 h on a 24 h ceiling → 3 chained segments (§8.8).
        let db = ctx.db.read().unwrap();
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
        assert!(script.contains("# chained: JOB-0"), "{script}");
        // Submit-script env goes after the whole '#' header block (§6.1), not
        // right after the shebang, so scheduler directives stay on top.
        assert!(
            script.starts_with("#!/bin/sh\n# chained: JOB-0\nexport CACTUP_TEST_ENV=1\nexec"),
            "env after the directive block: {script}"
        );

        // Frozen vars allow full reconstruction (D11).
        let vars = restart::thaw_vars(&r2.meta.vars).unwrap();
        assert_eq!(vars.get("RESTART_ID").unwrap().canonical(), "2");
        assert_eq!(vars.get("QUEUE").unwrap().canonical(), "batch");
        assert_eq!(vars.get("TASKS").unwrap().canonical(), "8", "fill-the-node default");

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
        // Parfile resolved with substitution and the @@ escape (§6.1/§6.2).
        let par = fs::read_to_string(r1.dir.join("bbh.par")).unwrap();
        assert!(par.contains("# sim bbh t=8 lit=@"), "{par}");
        // Completion recorded; TERMINATE + heartbeat exist (§9.3).
        let r1 = Restart::load(&sim.dir, 1).unwrap();
        assert!(r1.meta.terminated && r1.meta.finished.is_some());
        assert!(r1.dir.join("TERMINATE").is_file());
        assert!(r1.heartbeat_path().is_file());
        // log.txt in the preserved format (§12).
        let log = fs::read_to_string(sim.log_path()).unwrap();
        assert!(log.contains("] create::"), "{log}");
        assert!(log.contains("] submit::submitted output-0000 as job JOB-0"), "{log}");
    }

    #[test]
    fn resubmit_allocates_a_fresh_restart_and_leaves_checkpoints_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));

        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "ActiveThorns = \"IOUtil\"\n").unwrap();
        let sim = crate::sim::create(&ctx, &machine, &inst, false, "bbh", &parfile, None, None).unwrap();
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
        assert_eq!(restart::thaw_vars(&r.meta.vars).unwrap().get("RESTART_ID").unwrap().canonical(), "2");
        // Nothing was copied or linked forward, and the originals are untouched.
        assert!(!restart::workdir(&sim, 2).join("bbh.chkpt.it_10.h5").exists());
        assert!(restart::workdir(&sim, 0).join("bbh.chkpt.it_00.h5").is_file());
        assert!(restart::workdir(&sim, 1).join("bbh.chkpt.it_10.h5").is_file());
    }

    #[test]
    fn submit_conflicting_parfile_needs_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = fake_machine(&tmp.path().join("mdb-fake"));
        let inst = fake_installation(&tmp.path().join("inst"));
        let ctx = fake_ctx(&tmp.path().join("db"));
        let parfile = tmp.path().join("bbh.par");
        fs::write(&parfile, "x\n").unwrap();
        crate::sim::create(&ctx, &machine, &inst, false, "bbh", &parfile, None, None).unwrap();

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
            [variants.submitscript]
            "default" = { queues = ["batch"], default = true }
            [variants.runscript]
            "default" = { queues = ["batch"], default = true }
            [variants.optionlist]
            variants = []
            "#,
        )
        .unwrap();
        machine.meta.universes = extra.universes;

        let cfg_plain: ConfigMeta = toml::from_str(
            r#"
            name = "sim"
            variant = "default"
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
        let got = resolve_run_universe(&machine, &cfg_coerced, &cli, None, None, false).unwrap();
        assert_eq!(got.unwrap().0, "other");
        // --no-universe forces the host context.
        let no_uni = UniverseFlags { universe: None, no_universe: true };
        assert!(resolve_run_universe(&machine, &cfg_coerced, &no_uni, None, None, false).unwrap().is_none());
        // 2. Build-universe coercion beats the variant's own universe.
        let got = resolve_run_universe(&machine, &cfg_coerced, &no_flags, Some("other"), None, false).unwrap();
        assert_eq!(got.unwrap().0, "sif");
        // Opt-out (§7.8) falls through to the variant.
        let mut cfg_optout = cfg_coerced.clone();
        cfg_optout.coerce_run_universe = false;
        let got = resolve_run_universe(&machine, &cfg_optout, &no_flags, Some("other"), None, false).unwrap();
        assert_eq!(got.unwrap().0, "other");
        // 3/4. Variant → default-universe → 5. none (no declared host here).
        let got = resolve_run_universe(&machine, &cfg_plain, &no_flags, None, Some("sif"), false).unwrap();
        assert_eq!(got.unwrap().0, "sif");
        assert!(resolve_run_universe(&machine, &cfg_plain, &no_flags, None, None, false).unwrap().is_none());
        // Unknown names are hard errors listing the known set (§4.8).
        let bad = UniverseFlags { universe: Some("ghost".to_owned()), no_universe: false };
        let err = resolve_run_universe(&machine, &cfg_plain, &bad, None, None, false).unwrap_err();
        assert!(format!("{err:#}").contains("sif"), "{err:#}");

        // 5'. A DECLARED host becomes the final fallback of both chains
        // (§4.8); --no-universe still bypasses it.
        let host: Universe = toml::from_str("env-run-setup = \"module load x\"").unwrap();
        machine.meta.universes.insert("host".to_owned(), host);
        let got = resolve_run_universe(&machine, &cfg_plain, &no_flags, None, None, false).unwrap();
        assert_eq!(got.unwrap().0, "host");
        assert!(resolve_run_universe(&machine, &cfg_plain, &no_uni, None, None, false).unwrap().is_none());
        let got = resolve_submit_universe(&machine, None, None).unwrap();
        assert_eq!(got.unwrap().0, "host");
        // With a variant/default-universe, host stays out of the way.
        let got = resolve_submit_universe(&machine, Some("sif"), None).unwrap();
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
}
