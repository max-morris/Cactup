//! `cactup build` — the build command family (spec §7, §7.9, §8.3.1): a bare
//! foreground build, `build submit` (queueing one), the auto-selection
//! between the two, and the compute-node entry point generated submit
//! scripts invoke. `list`/`show`/`log`/`stop`/`prune` land in a later chunk.

use super::{machine, Ctx};
use crate::args::{BuildCommand, BuildOpts, BuildStartArgs, BuildSubmitArgs};
use crate::build::attempt::{BuildAttempt, BuildMeta, BuildOutcomeRecord, Reservation};
use crate::build::{self, OptionlistSource, Prepared, SubmitReservation};
use crate::database::Database;
use crate::installation::Installation;
use crate::lock::LinkLock;
use crate::mdb::{BuildAction, Machine, Phase, ScriptKind, HOST_UNIVERSE};
use crate::scheduler::{is_live, JobRef, JobStatus, Scheduler, Statuses, Stopped};
use crate::sim::cache;
use crate::sim::restart::{thaw_vars, NO_JOB_ID};
use crate::sim::start::{generate_script, resolve_submit_universe, write_executable, Identity};
use crate::sim::vars::{resolve_topology, set_machine_vars, set_topology_vars, set_wall_only_vars};
use crate::Res;
use anyhow::{bail, Context};
use chrono::Utc;
use colored::Colorize;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn dispatch(ctx: &Ctx, start: BuildStartArgs, command: Option<BuildCommand>) -> Res<()> {
    match command {
        None => auto(ctx, start),
        Some(BuildCommand::Run(args)) => run(ctx, args),
        Some(BuildCommand::Submit(args)) => submit(ctx, args),
        Some(BuildCommand::List { long, all }) => list(ctx, long, all),
        Some(BuildCommand::Show { name, long }) => show(ctx, name, long),
        Some(BuildCommand::Log { name, follow, follow_out, follow_err }) => {
            log(ctx, name, crate::tail::FollowMode::from_flags(follow, follow_out, follow_err))
        }
        Some(BuildCommand::Stop { name, force }) => stop(ctx, name, force),
        Some(BuildCommand::Prune { name, keep }) => prune(ctx, name, keep),
    }
}

/// Bare `cactup build` (§7.9): choose between the foreground and queued
/// paths per `[build].default-action` and whether the machine can even
/// submit builds. The compute-node form (`--config-dir`/`--attempt-id`)
/// short-circuits into `run` before anything below touches the MDB, the
/// global DB, or the installation registry (D11).
fn auto(ctx: &Ctx, start: BuildStartArgs) -> Res<()> {
    if start.config_dir.is_some() {
        return run(ctx, start);
    }
    let machine = machine::resolve(ctx)?;
    let installation = Installation::resolve(ctx)?;
    if choose_submit(&machine, &start.opts)? {
        let db = ctx.db.read()?;
        let submitted =
            submit_impl(&installation, &machine, &db, &start, ctx.globals.hostname.as_deref())?;
        // `submitted` is None when the config was already up to date: there
        // is no job, so there is nothing to block on either.
        let Some((name, attempt_id)) = submitted.filter(|_| start.block) else { return Ok(()) };
        let config_dir = installation.cactus_root().join("configs").join(&name);
        watch_build(&machine, &config_dir, attempt_id, Watch::Block)
    } else {
        if start.block {
            return Err(no_queue_to_block_on(&machine, &start.opts));
        }
        run_with(&machine, &installation, start)
    }
}

/// Why `--block` has nothing to wait for on a build that is about to run in
/// the foreground (§7.9). These are exactly the three ways `choose_submit`
/// says "run", said back to the user: "this build is not going to the queue"
/// is useless without knowing *which* of them decided it, since the fix
/// differs — drop the flag, drop `--virtual-executable`, say `build submit`
/// explicitly, or fix the machine.
fn no_queue_to_block_on(machine: &Machine, opts: &BuildOpts) -> anyhow::Error {
    let why = if opts.virtual_executable.is_some() {
        "a --virtual-executable build copies a prebuilt binary and is never queued".to_owned()
    } else if machine.meta.build.default_action == Some(BuildAction::Run) {
        format!("machine \"{}\" sets [build].default-action = \"run\"", machine.name)
    } else {
        format!(
            "machine \"{}\" cannot submit builds to the queue — it is missing {}",
            machine.name,
            missing_to_submit_builds(machine).unwrap_or("nothing")
        )
    };
    anyhow::anyhow!(
        "--block waits for a queued build to finish, but this build runs in the foreground: {why}"
    )
}

/// §7.9's auto-selection table:
///
/// | `[build].default-action` | can submit | result           |
/// |---------------------------|------------|------------------|
/// | unset                     | yes        | submit           |
/// | unset                     | no         | run              |
/// | "run"                     | either     | run              |
/// | "submit"                  | yes        | submit           |
/// | "submit"                  | no         | hard error       |
///
/// A `--virtual-executable` build is never queued (see `submit_impl`'s own
/// refusal) — it always runs in the foreground regardless of what the
/// machine would otherwise pick, so the auto path routes around the table
/// entirely rather than picking "submit" and then failing.
fn choose_submit(machine: &Machine, opts: &BuildOpts) -> Res<bool> {
    if opts.virtual_executable.is_some() {
        return Ok(false);
    }
    match machine.meta.build.default_action {
        Some(BuildAction::Run) => Ok(false),
        Some(BuildAction::Submit) => {
            require_can_submit_builds(machine)?;
            Ok(true)
        }
        None => Ok(machine.meta.can_submit_builds()),
    }
}

/// Hard-error naming exactly what a machine is missing to submit builds —
/// shared by the auto-selection table's "submit" row and an explicit
/// `cactup build submit` on a machine where it is impossible.
fn require_can_submit_builds(machine: &Machine) -> Res<()> {
    match missing_to_submit_builds(machine) {
        None => Ok(()),
        Some(missing) => bail!(
            "machine \"{}\" cannot submit builds to the queue — it is missing {missing}",
            machine.name
        ),
    }
}

/// What `machine` lacks in order to submit builds, or `None` when it lacks
/// nothing — the naming half of [`require_can_submit_builds`], split out so
/// [`no_queue_to_block_on`] can name the same gap without raising an error.
fn missing_to_submit_builds(machine: &Machine) -> Option<&'static str> {
    let no_script = machine.meta.scripts(ScriptKind::BuildSubmit).routes.is_empty();
    let no_submit_cmd = machine.meta.scheduler.submit.is_none();
    match (no_script, no_submit_cmd) {
        (false, false) => None,
        (true, true) => Some("a [buildsubmitscripts] script and a [scheduler].submit command"),
        (true, false) => Some("a [buildsubmitscripts] script"),
        (false, true) => Some("a [scheduler].submit command"),
    }
}

/// Refuse when `name`'s latest build attempt is still live — running here
/// (`running.lock` held), or queued/running via the scheduler — unless
/// `force`, which stops a live SCHEDULED job first (never one actually
/// executing right now; nothing here can safely kill this process's own
/// `make` from outside it). Builds never chain (§7.9): two live attempts of
/// one config is never wanted. Shared by the foreground and submit paths,
/// as `testsuite::run::start_impl` guards a test run.
///
/// Only [`requeue_query`]'s job goes to the scheduler: a foreground
/// attempt's job id is the pid of the cactup that ran it, and once that
/// cactup is gone the pid can be any other process's — asking the
/// scheduler about it (on the generic machine, `ps`) would find that
/// process live, and `-f` would stop its process group. After the
/// attempt's outcome a job the scheduler still lists, or could not be asked
/// about (it may have been queued again), refuses a new attempt: the
/// refusal prints the failed query and names `build stop`; `-f` stops the
/// job first (`Scheduler::stop_judged`: a stop that fails is an error,
/// unless the scheduler says it does not know the job). A job settled by
/// an earlier stop (`job-stopped`) is not asked about again.
fn guard_no_live_attempt(config_dir: &Path, name: &str, sched: &Scheduler, force: bool) -> Res<()> {
    let Some(id) = BuildAttempt::latest_id(config_dir)? else { return Ok(()) };
    let attempt = BuildAttempt::open(config_dir, id)?;
    if LinkLock::is_held_live(&attempt.running_lock_path())? {
        bail!("config \"{name}\" is currently building (running.lock is held)");
    }
    let Some(job) = requeue_query(&attempt.meta, sched) else { return Ok(()) };
    let ended = attempt.meta.outcome.is_some();
    // After the outcome: still listed (leaving the queue, or starting
    // again), or not answered (it may have been queued again): a new
    // attempt beside it is refused, the safe reading for new work (§10).
    // `-f` stops it; after a failed query, a stop the scheduler refuses
    // saying it does not know the job is taken as the job ended, and any
    // other failure of the stop command is an error.
    let (status, live, failed) = match ended {
        true => match sched.ask_after_end(job)? {
            crate::scheduler::AfterEnd::Listed(status) => (Some(status), true, None),
            crate::scheduler::AfterEnd::Unanswered(failed) => (None, true, Some(failed)),
            crate::scheduler::AfterEnd::Over(_) => (None, false, None),
        },
        false => (None, is_live(sched.get_status(job)?, false), None),
    };
    if live {
        // An older record's pid (§10): never stopped, `-f` neither.
        if sched.unverified(job) {
            bail!("{}; then build again", crate::scheduler::UnverifiedPid::of(job.id));
        }
        if sched.shares_group(job) {
            bail!("{}; then build again", crate::scheduler::SharedGroup(job.id.to_owned()));
        }
        if let Some(failed) = failed.as_deref()
            && !force
        {
            bail!(
                "config \"{name}\"'s last build attempt ended, but the scheduler could not be asked whether \
                 its job {} was queued again: {failed}\n`cactup build stop {name}` asks the scheduler to \
                 stop the job (a job it says it does not know is then taken as ended); or pass -f to stop it and build",
                job.id
            );
        }
        // After the attempt's outcome, a job the scheduler still lists in a
        // state its patterns read: leaving the queue, or starting again.
        if let Some(status) = status
            && !force
        {
            bail!(
                "config \"{name}\"'s last build attempt ended, but the scheduler still lists its job {} {} \
                 (or pass -f to stop it)",
                job.id,
                crate::scheduler::listed_after_end(status, &format!("cactup build stop {name}"))
            );
        }
        if !force {
            bail!(
                "config \"{name}\" already has a live build attempt ({}); \
                 `cactup build stop {name}` first, or pass -f to stop it",
                job.id
            );
        }
        match ended {
            true => {
                if let crate::scheduler::EndedStop::NotKnown(said) = sched.stop_judged(job, match failed.as_deref() {
                    Some(failed) => crate::scheduler::StopCase::Unanswered(failed),
                    None => crate::scheduler::StopCase::Listed,
                })? {
                    println!(
                        "The scheduler does not know build job {} (its stop command said: {said}): taken as \
                         ended",
                        job.id
                    );
                }
            }
            false => {
                sched.stop(job)?;
            }
        }
    }
    Ok(())
}

/// Foreground build (the bare `cactup build` and `cactup build run` forms),
/// or the compute-node path when `--config-dir`/`--attempt-id` are given.
///
/// The compute-node branch MUST be checked first, before anything resolves
/// an installation or a machine (D11: `cactup build run --config-dir …
/// --attempt-id …`, what a generated submit script invokes, may never touch
/// the global DB, the installation registry, the MDB, or knobs).
fn run(ctx: &Ctx, args: BuildStartArgs) -> Res<()> {
    // Ahead of the compute-node branch, and reading nothing (D11): a flag
    // that cannot apply should be rejected before anything is done, not
    // after.
    if args.block {
        bail!(
            "--block waits for a queued build to finish; `cactup build run` builds here, in the \
             foreground, and is already finished when it returns — drop the flag, or use \
             `cactup build submit --block`"
        );
    }
    if let (Some(config_dir), Some(attempt_id)) = (args.config_dir.clone(), args.attempt_id) {
        let mut attempt = BuildAttempt::open(&config_dir, attempt_id)?;
        // tee = false: the scheduler owns the output files here, unlike a
        // foreground build where cactup itself is the user's terminal.
        // No probe to inherit: this attempt came off disk, possibly hours
        // after `prepare` staged it, so `execute` takes its own (§7.4).
        build::execute(&mut attempt, false, None)?;
        return Ok(());
    }

    let machine = machine::resolve(ctx)?;
    let installation = Installation::resolve(ctx)?;
    run_with(&machine, &installation, args)
}

/// The config a nameless `cactup build` / `build run` / `build submit` acts
/// on: the installation's active config.
///
/// The generic null-config error (`InstallationMeta::active_config`) reads as
/// a dead end to someone building their *first* config, so the build paths
/// spell out the naming rule it leaves implicit: with no name, `build`
/// rebuilds whatever is already active, so a config nobody has built yet —
/// and any config other than the active one — has to be named. §7.1
fn start_name(inst: &Installation, name: Option<&str>) -> Res<String> {
    if let Some(name) = name {
        return Ok(name.to_owned());
    }
    if let Some(active) = inst.meta()?.active_config {
        return Ok(active);
    }
    // Best-effort: the configs that do exist are exactly what the user needs
    // to pick a name from, so listing them saves a `cactup config list` hop.
    let existing: Vec<String> = super::config::list_configs(&inst.cactus_root())
        .unwrap_or_default()
        .into_iter()
        .map(|(name, _)| name)
        .collect();
    if existing.is_empty() {
        bail!(
            "nothing to build: `cactup build` with no name rebuilds the active config, and \
             this installation has no active config yet. A config's first build has to name \
             it — `cactup build <name>` creates the config and makes it active, and a bare \
             `cactup build` rebuilds it from then on."
        );
    }
    bail!(
        "no active config to rebuild: `cactup build` with no name rebuilds the active config, \
         and this installation has none selected. Name the config to build — `cactup build \
         <name>`, which also creates it if it is new — or make one active with `cactup config \
         use <name>`. Configs here: {} (see `cactup config list`).",
        config_name_list(&existing)
    )
}

/// The config names for `start_name`'s error, capped so a big installation
/// doesn't bury the hint under the list.
fn config_name_list(names: &[String]) -> String {
    const MAX: usize = 6;
    if names.len() <= MAX {
        return names.join(", ");
    }
    format!("{}, and {} more", names[..MAX].join(", "), names.len() - MAX)
}

fn run_with(machine: &Machine, installation: &Installation, args: BuildStartArgs) -> Res<()> {
    let name = start_name(installation, args.name.as_deref())?;

    // Builds never chain (§7.9): a config with a live attempt already in
    // flight refuses a second one, foreground included.
    let config_dir = installation.cactus_root().join("configs").join(&name);
    let sched = Scheduler::for_machine(machine);
    guard_no_live_attempt(&config_dir, &name, &sched, args.opts.force)?;

    let outcome = build::build(installation, machine, &name, &args.opts)?;
    // First build becomes active; later builds keep the pointer (§7.1).
    let locked = installation.locked()?;
    let mut meta = locked.meta()?;
    if meta.active_config.is_none() {
        meta.active_config = Some(name.clone());
        locked.set_meta(&meta)?;
        println!("Config {} is now the active config.", name.bold());
    }
    if outcome.rebuilt {
        println!(
            "{}",
            format!("Built config {} (build-id {}).", name.bold(), outcome.meta.build_id).bright_green()
        );
        // The rebuild minted a new build-id and replaced the exe, so the
        // previous build's CACHE/exe entry may now be unreferenced (§8.1).
        // Best-effort.
        if let Ok(inst_meta) = installation.meta()
            && let Ok(sim_home) = inst_meta.sim_home()
        {
            let _ = cache::gc(sim_home, None);
        }
    }
    Ok(())
}

/// `cactup build submit` (§7.9, §8.3.1).
fn submit(ctx: &Ctx, args: BuildSubmitArgs) -> Res<()> {
    let machine = machine::resolve(ctx)?;
    let installation = Installation::resolve(ctx)?;
    let db = ctx.db.read()?;
    let submitted =
        submit_impl(&installation, &machine, &db, &args.start, ctx.globals.hostname.as_deref())?;
    let Some((name, attempt_id)) = submitted else { return Ok(()) };
    let mode = match (args.start.block, args.follow) {
        (true, _) => Watch::Block,
        (false, true) => Watch::Follow,
        (false, false) => return Ok(()),
    };
    let config_dir = installation.cactus_root().join("configs").join(&name);
    watch_build(&machine, &config_dir, attempt_id, mode)
}

/// Resolve the reservation/topology BEFORE `prepare` runs (prepare stages
/// the reservation into the attempt), then stage the attempt, generate the
/// buildsubmitscript, and hand it to the scheduler. Mirrors
/// `testsuite::run::start_impl`'s submit half.
fn submit_impl(
    inst: &Installation,
    machine: &Machine,
    db: &Database,
    args: &BuildStartArgs,
    hostname_override: Option<&str>,
) -> Res<Option<(String, u32)>> {
    require_can_submit_builds(machine)?;
    // §7.7: a virtual-executable build copies a prebuilt binary and runs no
    // `make` — queueing it would just burn a scheduler slot to run `cp`.
    if args.opts.virtual_executable.is_some() {
        bail!(
            "`cactup build submit` cannot be combined with --virtual-executable — it copies a \
             prebuilt binary and runs no `make`, so queueing it is meaningless; use `cactup \
             build run --virtual-executable` instead"
        );
    }

    let name = start_name(inst, args.name.as_deref())?;
    let cactus_root = inst.cactus_root();
    let config_dir = cactus_root.join("configs").join(&name);
    let sched = Scheduler::for_machine(machine);
    guard_no_live_attempt(&config_dir, &name, &sched, args.opts.force)?;

    // §2: [build] defaults + the MAKEJOBS/CPUS_PER_TASK coupling, resolved
    // BEFORE the queue is picked — a build must never reserve fewer cores
    // than `make -j` asks for — then the topology itself. No binary fit: the
    // build job's queue is where `make` runs, and the optionlist's
    // compatible-queues/gpu/build universe describe where the *binary* may
    // run, which is a sim's business, not this job's (§7.9).
    //
    // The queue: -q → the `build-queue` knob → `[build].queue` → the machine
    // default. The knob is the user's standing choice for build jobs, as
    // `queue` is for sims and tests (which builds never read), so it outranks
    // the MDB's `[build].queue` — unless this machine cannot queue a build
    // there (no such queue, or no build-submit script serves it), in which
    // case it is passed over with a note.
    let mut flags = args.topology.clone();
    let mut skipped_knob = None;
    if flags.queue.is_none()
        && let Some(knob) = db.knob("build-queue")
    {
        let routed = machine.meta.scripts(ScriptKind::BuildSubmit).routes.values().any(|r| r.queues.iter().any(|q| q == knob));
        if machine.meta.queues.contains_key(knob) && routed {
            flags.queue = Some(knob.to_owned());
        } else {
            skipped_knob = Some(knob);
        }
    }
    build::apply_build_defaults(&mut flags, machine);
    let make_jobs = build::reconcile_make_jobs(&mut flags, args.opts.make_jobs, machine);
    let topo = resolve_topology(&flags, machine, db, None, false)?;
    if let Some(knob) = skipped_knob {
        eprintln!(
            "{} not using your `build-queue` knob (\"{knob}\"): machine \"{}\" cannot queue a build \
             there; using \"{}\" instead",
            "note:".yellow(),
            machine.name,
            topo.queue
        );
    }

    let reservation = Reservation {
        queue: topo.queue.clone(),
        nodes: topo.nodes,
        tasks: topo.tasks,
        tpn: topo.tpn,
        cpus: topo.cpus,
        gpus_per_task: topo.gpus_per_task,
        walltime: topo.total_wall,
        allocation: topo.allocation.clone(),
    };
    let plan = SubmitReservation { reservation, make_jobs };

    let mut attempt = match build::prepare(inst, machine, &name, &args.opts, Some(&plan))? {
        Prepared::UpToDate(_) => {
            println!("Config {} is up to date; nothing to submit.", name.bold());
            return Ok(None);
        }
        // `prepare`'s probe is dropped on the submit path on purpose: this
        // attempt runs later, on a compute node, and must probe there.
        Prepared::Ready(a, _) => a,
    };

    // §3: the build submit variable set — topology/walltime on top of what
    // `prepare` already froze (MAKEJOBS, USER, SOURCEDIR, CONFIGURATION,
    // SCRATCH_HOME, ALLOCATION, CACTUP, STDOUT_FILE, STDERR_FILE), plus the
    // submit-only identity and script-plumbing names. Deliberately NOT
    // `sim::vars::assemble` — that sets simulation-only names a build script
    // must never see (§9.3's no-leak rule, mirrored here).
    let identity = Identity::resolve(db, hostname_override);
    let build_universe = attempt.meta.universe.as_ref().map(|u| u.name.as_str());
    let mut vset = thaw_vars(&attempt.meta.vars, &attempt.meta.knobs)?;
    set_topology_vars(&mut vset, &topo, &format!("build-{name}"));
    set_wall_only_vars(&mut vset, topo.total_wall);
    vset.set("SCRIPTFILE", attempt.submit_script_path().display().to_string());
    vset.set("CONFIG_DIR", config_dir.display().to_string());
    vset.set("ATTEMPT_ID", attempt.id as u64);
    vset.set("ALIAS", inst.alias.as_str());
    vset.set("MACHINE", machine.name.as_str());
    vset.set("HOSTNAME", identity.hostname.as_str());
    vset.set("USER", identity.user.as_str());
    vset.set("EMAIL", identity.email.as_str());
    vset.set("JOB_ID", "");
    set_machine_vars(&mut vset, machine, &topo.queue, build_universe)?;

    // §4: buildsubmitscript + universe, generate + write, submit.
    // Routed by the queue (filtered by the build universe the job's `make`
    // runs in) and nothing else: the optionlist's `--flavor` names what is
    // built, never which script queues it (§7.9).
    let scripts = machine.meta.scripts(ScriptKind::BuildSubmit);
    let cfg_universe = attempt.meta.config_meta.universe.as_deref().unwrap_or(HOST_UNIVERSE);
    let (script_name, entry) = scripts.route(&topo.queue, cfg_universe, false)?;
    let submit_uni =
        resolve_submit_universe(machine, entry.universe.as_deref())?;
    let submit_uni_name = submit_uni.as_ref().map(|(n, _)| n.as_str());
    let script_name = script_name.to_owned();
    // `set_machine_vars` hardcodes the RUN-phase env (§6.1 — it's shared by
    // three subsystems that mostly want that); a submit script needs the
    // SUBMIT-phase one, in the submit universe, the very block a `.sh`
    // script gets prepended (`generate_script`), so a `.py` script prints
    // the same — the same fix `sim submit` applies rather than changing the
    // shared helper. Never the build universe's: that is where `make` runs,
    // not the batch script.
    vset.set("ENV_SETUP", machine.meta.phase_env(submit_uni_name, Phase::Submit));

    let script =
        generate_script(machine, ScriptKind::BuildSubmit, &script_name, &vset, Phase::Submit, submit_uni_name)?;
    write_executable(&attempt.submit_script_path(), &script)?;
    attempt.store_meta()?;

    let uni = submit_uni.as_ref().map(|(n, u)| (n.as_str(), *u));
    let attempt_id = attempt.id;

    // §7.9's `--block` on a machine that declares [scheduler].blocking-submit:
    // the submit command itself does not return until the build is over, so
    // the job id is recorded from *inside* the wait (the callback) rather than
    // after it. Nothing may be stored once that call returns: by then the
    // compute node's own cactup has written this attempt's outcome into the
    // very `build.toml` a `store_meta` here would overwrite with our stale
    // copy.
    //
    // The callback itself can come that late, too: `sbatch --wait` writing
    // into a pipe holds back even its "Submitted batch job" line until it
    // exits (seen on Deep Bayou, also with `stdbuf -oL`), so the job id may
    // only arrive once the build is over. So the callback records it into
    // what is on disk now, not into our copy, and never drops an outcome the
    // compute node has already written.
    if args.block && machine.meta.scheduler.blocking_submit.is_some() {
        let mut record = |job: &crate::scheduler::JobRecord| -> Res<()> {
            let _lock = BuildAttempt::record_lock(&BuildAttempt::attempt_dir(&config_dir, attempt_id))?;
            let mut fresh = BuildAttempt::open(&config_dir, attempt_id)?;
            fresh.meta.set_job(job);
            fresh.meta.submitted = true;
            fresh.meta.timestamps.submitted.get_or_insert_with(Utc::now);
            fresh.store_meta()?;
            println!("Submitted build of {} as job {}", name.bold(), job.id.bold());
            Ok(())
        };
        sched.submit_blocking(&vset, uni, &mut record)?;
        return Ok(Some((name, attempt_id)));
    }

    // Store-submit-store: the job id is only recorded once the submit
    // actually went through, so a crash between them can never masquerade a
    // failed submission as a successful one, nor lose a real job id.
    // Held until the job is recorded: a job that starts at once waits for
    // it before it writes the record (`BuildAttempt::store_meta_keeping_job`).
    let record_lock = BuildAttempt::record_lock(&attempt.dir)?;
    let submitted = sched.submit(&vset, uni)?;
    attempt.meta.set_job(&submitted);
    let job_id = submitted.id;
    attempt.meta.submitted = true;
    attempt.meta.timestamps.submitted = Some(Utc::now());
    attempt.store_meta()?;
    drop(record_lock);

    println!("Submitted build of {} as job {}", name.bold(), job_id.bold());
    Ok(Some((name, attempt.id)))
}

/// How often `--follow` re-checks the scheduler for a job that vanished from
/// the queue without cactup ever recording an outcome (a crashed compute
/// node, an OOM-killed job) — far coarser than the output poll itself: unlike
/// a `stat` on the attempt's own files, a status query is a real scheduler
/// round-trip, and nothing about watching output needs it more than a few
/// times a minute.
const FOLLOW_JOB_CHECK_INTERVAL: std::time::Duration = std::time::Duration::from_secs(5);

fn detached_message(job_id: &str) {
    println!("Detached — job {job_id} keeps running in the queue.");
}

/// What [`watch_build`] does while it waits for a queued build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Watch {
    /// `--follow`: stream the attempt's output as it grows.
    Follow,
    /// `--block`: wait in silence for the outcome. Also how a *native*
    /// blocking submit finishes — by the time it returns the outcome is
    /// already on disk, so the first round through the loop reads it and
    /// returns, and the polling below never happens.
    Block,
}

/// Wait for a freshly-queued attempt and report the outcome once it lands —
/// `--follow` streaming its output as it grows, `--block` silent (§7.9).
/// Deliberately NOT `tail::tail_log` — that streams until Ctrl-C only, and
/// its side-by-side mode needs a tty, neither of which fits "watch my build
/// finish" — but built from the same two primitives it is itself built from:
/// `LogTail` polls the two output files, `PollBackoff` paces how often
/// (NFS-aware). Ctrl-C detaches without touching the queued job — it keeps
/// running; only this local loop stops.
///
/// The two modes differ only in what is printed: both derive the verdict from
/// the attempt's own recorded outcome, which is the single authority on what
/// a build did (§7.9) — never a relayed exit code.
fn watch_build(machine: &Machine, config_dir: &Path, attempt_id: u32, mode: Watch) -> Res<()> {
    let attempt = BuildAttempt::open(config_dir, attempt_id)?;
    let sched = Scheduler::for_machine(machine);
    let job_id = attempt.meta.job_id.clone();
    let config_name = attempt.meta.config.clone();

    let (mut out_path, mut err_path) = (attempt.out_path(), attempt.err_path());
    if let Some(toml::Value::String(s)) = attempt.meta.vars.get("STDOUT_FILE") {
        out_path = PathBuf::from(s);
    }
    if let Some(toml::Value::String(s)) = attempt.meta.vars.get("STDERR_FILE") {
        err_path = PathBuf::from(s);
    }
    // A native blocking submit has already waited; announcing a wait that is
    // over would be a lie, so the banner waits for the first round to prove
    // there is something left to wait for.
    let mut announced = false;
    let mut announce = || {
        if std::mem::replace(&mut announced, true) {
            return;
        }
        let what = match mode {
            Watch::Follow => "Following",
            Watch::Block => "Waiting for",
        };
        println!(
            "{what} build of {} (job {job_id}) — Ctrl-C detaches without stopping the queued job.",
            config_name.bold()
        );
    };

    let mut tails = [crate::tail::LogTail::new(out_path, 0), crate::tail::LogTail::new(err_path, 0)];
    let mut backoff = crate::tail::PollBackoff::new();
    let mut last_job_check = std::time::Instant::now() - FOLLOW_JOB_CHECK_INTERVAL;

    loop {
        if gix::interrupt::is_triggered() {
            detached_message(&job_id);
            return Ok(());
        }

        let mut had_data = false;
        if mode == Watch::Follow {
            for tail in &mut tails {
                if let Some(buf) = tail.poll() {
                    had_data = true;
                    let mut stdout = std::io::stdout();
                    let _ = stdout.write_all(&buf);
                    let _ = stdout.flush();
                }
            }
        }

        let fresh = BuildAttempt::open(config_dir, attempt_id)?;
        if let Some(outcome) = &fresh.meta.outcome {
            // Drain whatever landed between the poll above and the outcome
            // being written, so the tail never truncates the last lines.
            if mode == Watch::Follow {
                for tail in &mut tails {
                    if let Some(buf) = tail.poll() {
                        let mut stdout = std::io::stdout();
                        let _ = stdout.write_all(&buf);
                        let _ = stdout.flush();
                    }
                }
            }
            return if outcome.complete {
                println!("{}", format!("Build of {} complete.", config_name.bold()).bright_green());
                Ok(())
            } else {
                bail!(
                    "build of {config_name} failed; see {} and {}",
                    fresh.out_path().display(),
                    fresh.err_path().display()
                );
            };
        }

        if last_job_check.elapsed() >= FOLLOW_JOB_CHECK_INTERVAL {
            last_job_check = std::time::Instant::now();
            // A query that failed is no sign the job died: keep waiting.
            let live = match sched.get_status(fresh.meta.job()) {
                Ok(status) => matches!(status, JobStatus::Running | JobStatus::Queued | JobStatus::Holding),
                Err(_) => true,
            } || LinkLock::is_held_live(&fresh.running_lock_path()).unwrap_or(false);
            if !live {
                bail!(
                    "build of {config_name} ended without ever recording an outcome — it may \
                     have died unexpectedly; see {} and {}",
                    fresh.out_path().display(),
                    fresh.err_path().display()
                );
            }
        }

        announce();
        backoff.note(had_data);
        let mut remaining = backoff.interval();
        while !remaining.is_zero() {
            if gix::interrupt::is_triggered() {
                detached_message(&job_id);
                return Ok(());
            }
            let chunk = remaining.min(std::time::Duration::from_millis(100));
            std::thread::sleep(chunk);
            remaining -= chunk;
        }
    }
}

// ---- shared state derivation (§7.9's monitoring model) --------------------

/// A build attempt's derived display state, mirroring `sim::manage`'s
/// `SimState`/`display_state` split: the live scheduler query is held back so
/// callers can batch it across a whole `build list`/`build show` (see
/// [`pending_job_query`]) instead of one round-trip per attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BuildDisplayState {
    Running,
    Queued,
    Holding,
    Failed,
    Built,
    Stale,
    NeverBuilt,
    /// Its job's status query failed, and its lock does not say it runs.
    Unknown,
}

/// What `list` and `show` print for an attempt, given the query of
/// [`pending_job_query`]'s job: `None` when there was none, `Some(None)`
/// when the query failed. A failed query is UNKNOWN unless the attempt's
/// lock says it runs: never a state the query did not give.
fn shown_state(meta: &BuildMeta, query: Option<Option<JobStatus>>, lock_live: bool) -> BuildDisplayState {
    match query {
        Some(None) if !lock_live => BuildDisplayState::Unknown,
        _ => build_state(meta.outcome.as_ref(), query.flatten(), lock_live),
    }
}

/// The state matrix itself, as a pure function so it is testable without a
/// scheduler or a filesystem: `status` is the already-resolved live status
/// (`None` when there was nothing to query, or the query failed); `lock_live`
/// is whether `running.lock` is currently held by a live process — the only
/// signal left behind by a foreground build (`job_id` is its pid, not a real
/// scheduler id) or a submitted job the scheduler has already forgotten (U).
/// Never returns `NeverBuilt` — that is a "no attempts exist at all" case
/// callers decide before there is an attempt to derive a state from.
fn build_state(
    outcome: Option<&BuildOutcomeRecord>,
    status: Option<JobStatus>,
    lock_live: bool,
) -> BuildDisplayState {
    if let Some(outcome) = outcome {
        return if outcome.complete { BuildDisplayState::Built } else { BuildDisplayState::Failed };
    }
    match status {
        Some(JobStatus::Running) => BuildDisplayState::Running,
        Some(JobStatus::Queued) => BuildDisplayState::Queued,
        Some(JobStatus::Holding) => BuildDisplayState::Holding,
        // Gone from the queue (or never a real scheduler id to begin with —
        // a foreground build's job_id is its pid), or nothing was queried:
        // the attempt's own liveness marker breaks the tie.
        _ => {
            if lock_live {
                BuildDisplayState::Running
            } else {
                BuildDisplayState::Stale
            }
        }
    }
}

fn state_str(state: BuildDisplayState) -> colored::ColoredString {
    match state {
        BuildDisplayState::Running => "RUNNING".green().bold(),
        BuildDisplayState::Queued => "QUEUED".cyan(),
        BuildDisplayState::Holding => "HOLDING".yellow(),
        BuildDisplayState::Failed => "FAILED".red().bold(),
        BuildDisplayState::Built => "BUILT".blue(),
        BuildDisplayState::Stale => "STALE".yellow(),
        BuildDisplayState::NeverBuilt => "NEVER BUILT".normal(),
        BuildDisplayState::Unknown => "UNKNOWN".yellow(),
    }
}

/// The job worth a live scheduler query, if any: an attempt with a
/// recorded outcome is done (BUILT/FAILED) whatever the queue says now,
/// `NO_JOB_ID` was never submitted anywhere, and a foreground attempt's job
/// id is a pid, not a job (`build::build`): `running.lock` alone says
/// whether it runs, and the pid may be another process's by now. Shared by
/// `list`'s batching, `show`'s per-attempt history and `prune`; what acts
/// on the latest attempt asks [`requeue_query`].
fn pending_job_query(meta: &BuildMeta) -> Option<JobRef<'_>> {
    (meta.outcome.is_none() && meta.submitted && meta.job_id != NO_JOB_ID).then(|| meta.job())
}

/// What the guard before a new attempt and `build stop` ask the scheduler
/// about: [`pending_job_query`]'s job, and, where a batch scheduler may
/// have requeued it, within the requeue window after the outcome
/// (`Scheduler::asks_after_end`), a submitted attempt's job
/// after its outcome was recorded too: a requeued build waiting in the
/// queue is live, though it shows its last outcome until it runs again
/// (`build::execute` then clears it). Where job ids are pids, a recorded
/// outcome is the end (§10).
fn requeue_query<'m>(meta: &'m BuildMeta, sched: &Scheduler) -> Option<JobRef<'m>> {
    (meta.submitted
        && meta.job_id != NO_JOB_ID
        && (meta.outcome.is_none() || sched.asks_after_end(&meta.machine, crate::scheduler::end_to_ask(meta.timestamps.finished, meta.timestamps.job_stopped))))
        .then(|| meta.job())
}

/// Best-effort, ready-to-print description of `name`'s in-flight build
/// attempt, if any — e.g. `a build of config "mp" is RUNNING on the queue
/// (job 3774765, submitted 4m ago)`. Every place elsewhere in cactup that
/// would otherwise tell the user their config "was never built" or "is
/// incomplete" checks this first, so that advice never fires while a build
/// is actually in flight (the build-submit feature this module belongs to
/// makes that a real, not just theoretical, race).
///
/// `machine`, when the caller already has one, buys a single live scheduler
/// query to disambiguate RUNNING/QUEUED/HOLDING (mirrors [`build_state`]).
/// Without one — `config list` renders every config and must never
/// round-trip the scheduler per row — the attempt's own recorded metadata
/// still answers: `submitted` + a real job id + no recorded outcome means
/// the build went out and has not reported back yet. Either way,
/// `running.lock` (checked with no scheduler involved at all) still catches
/// an attempt actually executing right now, foreground or compute-node.
///
/// Best-effort throughout: no attempts, unreadable metadata, or a failed
/// scheduler query all yield `None`, never an error — this must never turn
/// someone else's command into a failure.
pub fn in_flight_build(config_dir: &Path, name: &str, machine: Option<&Machine>) -> Option<String> {
    let id = BuildAttempt::latest_id(config_dir).ok().flatten()?;
    let attempt = BuildAttempt::open(config_dir, id).ok()?;
    if attempt.meta.outcome.is_some() {
        return None;
    }
    let lock_live = LinkLock::is_held_live(&attempt.running_lock_path()).unwrap_or(false);

    if !attempt.meta.submitted {
        // A foreground build's job_id is this process's own pid, not a
        // scheduler id — running.lock is the only liveness signal there is,
        // and it needs no scheduler to check.
        return lock_live.then(|| {
            format!(
                "a build of config \"{name}\" is RUNNING in the foreground (pid {})",
                attempt.meta.job_id
            )
        });
    }
    if attempt.meta.job_id == NO_JOB_ID {
        return None; // the submission itself never went through
    }
    let job_id = &attempt.meta.job_id;

    let ago = attempt.meta.timestamps.submitted.map(|t| humanize_ago(Utc::now() - t));
    let submitted_suffix = ago.map(|a| format!(", submitted {a}")).unwrap_or_default();

    if let Some(machine) = machine {
        let sched = Scheduler::for_machine(machine);
        let status = sched.get_status(attempt.meta.job()).ok();
        let word = match build_state(None, status, lock_live) {
            BuildDisplayState::Running => "RUNNING",
            BuildDisplayState::Queued => "QUEUED",
            BuildDisplayState::Holding => "HOLDING",
            _ => return None,
        };
        return Some(format!(
            "a build of config \"{name}\" is {word} on the queue (job {job_id}{submitted_suffix})"
        ));
    }

    // No machine to query: fall back to what the attempt's own record can
    // say without a scheduler round-trip. A held lock means it is actually
    // executing right now, wherever it landed; otherwise "submitted, not
    // yet reported back" is as specific as this gets.
    if lock_live {
        return Some(format!(
            "a build of config \"{name}\" is RUNNING on the queue (job {job_id}{submitted_suffix})"
        ));
    }
    Some(format!(
        "a build of config \"{name}\" was submitted to the queue and has not reported back yet \
         (job {job_id}{submitted_suffix})"
    ))
}

/// A short "Ns/m/h/d ago" rendering for [`in_flight_build`]'s phrase.
fn humanize_ago(d: chrono::Duration) -> String {
    let secs = d.num_seconds().max(0);
    if secs < 60 {
        format!("{secs}s ago")
    } else if secs < 3600 {
        format!("{}m ago", secs / 60)
    } else if secs < 86400 {
        format!("{}h ago", secs / 3600)
    } else {
        format!("{}d ago", secs / 86400)
    }
}

// ---- login-node reconcile (D11) --------------------------------------------

/// A submitted build finishes on a compute node with no access to the global
/// DB, the installation registry, or the MDB (D11) — so the two steps
/// `run_with` performs right after a *foreground* build finishes (point
/// `active_config` at it if nothing is active yet; best-effort GC the
/// executable cache) can never happen there for a queued one. Run this at the
/// start of `build list`/`build show` instead. Keyed on each config's own
/// recorded success (`cactup-config.toml`'s `built`), never a stored
/// "already reconciled" flag: re-running costs nothing, since `cache::gc` is
/// just an idempotent readdir plus an `nlink() == 1` check, and pointing
/// `active_config` at a built config is itself a no-op once it is set.
fn reconcile_finished_builds(inst: &Installation) -> Res<()> {
    let configs = super::config::list_configs(&inst.cactus_root())?;
    let Some((first_built, _)) =
        configs.iter().find(|(_, m)| m.as_ref().and_then(|m| m.built).is_some())
    else {
        return Ok(());
    };

    let inst_meta = inst.meta()?;
    if inst_meta.active_config.is_none() {
        let locked = inst.locked()?;
        let mut meta = locked.meta()?;
        // Re-check under the lock: another process may have set it since the
        // unlocked read above.
        if meta.active_config.is_none() {
            meta.active_config = Some(first_built.clone());
            locked.set_meta(&meta)?;
            println!("Config {} is now the active config.", first_built.bold());
        }
    }

    if let Ok(sim_home) = inst_meta.sim_home() {
        let _ = cache::gc(sim_home, None);
    }
    Ok(())
}

// ---- build list -------------------------------------------------------

/// One `build list` row, gathered from disk before any scheduler query —
/// mirrors `sim::manage::Row`. `Listed` is the overwhelmingly common variant
/// and there is one row per config (tens, not thousands), so boxing
/// `BuildMeta` would add an allocation per row to save padding on the rare
/// NeverBuilt/Broken one — mirrors `testsuite::manage::Row`'s same call.
#[allow(clippy::large_enum_variant)]
enum Row {
    /// No build attempts recorded for this config at all.
    NeverBuilt,
    /// The latest attempt's directory exists but its metadata is unreadable.
    Broken(String),
    Listed { dir: PathBuf, meta: BuildMeta, lock_live: bool },
}

impl Row {
    fn job_to_query(&self) -> Option<JobRef<'_>> {
        match self {
            Row::Listed { meta, .. } => pending_job_query(meta),
            _ => None,
        }
    }
}

/// Everything one row needs from disk: the latest attempt's metadata, plus a
/// liveness probe of its own lock file.
fn gather_row(config_dir: &Path) -> Row {
    let id = match BuildAttempt::latest_id(config_dir) {
        Ok(Some(id)) => id,
        Ok(None) => return Row::NeverBuilt,
        Err(e) => return Row::Broken(format!("{e:#}")),
    };
    match BuildAttempt::open(config_dir, id) {
        Ok(attempt) => {
            let lock_live = LinkLock::is_held_live(&attempt.running_lock_path()).unwrap_or(false);
            Row::Listed { dir: attempt.dir, meta: attempt.meta, lock_live }
        }
        Err(e) => Row::Broken(format!("{e:#}")),
    }
}

fn print_row(name: &str, row: &Row, statuses: &Statuses, long: bool) {
    match row {
        Row::NeverBuilt => {
            println!("  {:24} {:12}", name.bold(), state_str(BuildDisplayState::NeverBuilt));
        }
        Row::Broken(e) => println!("  {:24} {:12} {e}", name.bold(), "BROKEN".red()),
        Row::Listed { dir, meta, lock_live } => {
            let query = pending_job_query(meta).map(|job| statuses.get(job).copied());
            let state = shown_state(meta, query, *lock_live);
            let mut extra = format!("attempt {}", BuildAttempt::dir_name(meta.attempt_id));
            if meta.job_id != NO_JOB_ID {
                extra.push_str(&format!(", job {}", meta.job_id));
            }
            if long {
                if let Some(r) = &meta.reservation {
                    extra.push_str(&format!(", queue {}, wall {}", r.queue, r.walltime.canonical()));
                }
                extra.push_str(&format!(", {}", dir.display()));
            }
            println!("  {:24} {:12} {}", name.bold(), state_str(state), extra);
        }
    }
}

/// `cactup build list` (§8.3.1's monitoring half): one row per config in the
/// active installation; `--all` spans every installation.
fn list(ctx: &Ctx, long: bool, all: bool) -> Res<()> {
    let machine = machine::resolve(ctx)?;
    let installations: Vec<Installation> = if all {
        let db = ctx.db.read()?;
        db.installations
            .values()
            .map(|i| Installation::new(i.alias.clone(), i.path.clone()))
            .collect()
    } else {
        vec![Installation::resolve(ctx)?]
    };
    list_impl(&machine, &installations, all, long)
}

fn list_impl(machine: &Machine, installations: &[Installation], all: bool, long: bool) -> Res<()> {
    for inst in installations {
        reconcile_finished_builds(inst)?;
    }

    let configs_per_inst: Vec<Vec<(String, Option<crate::build::ConfigMeta>)>> = installations
        .iter()
        .map(|inst| super::config::list_configs(&inst.cactus_root()))
        .collect::<Res<_>>()?;
    if configs_per_inst.iter().all(Vec::is_empty) {
        println!("No configs (see `cactup config list`)");
        return Ok(());
    }

    // Gather every row on a fixed-width pool (§2.4): each is a handful of
    // small local reads (a `.cactup-builds` scan, one `build.toml`, a lock
    // probe), latency-bound the same way a sim's row is.
    let dirs: Vec<PathBuf> = installations
        .iter()
        .zip(&configs_per_inst)
        .flat_map(|(inst, configs)| {
            configs.iter().map(move |(name, _)| inst.cactus_root().join("configs").join(name))
        })
        .collect();
    let rows = crate::par::parallel_map(&dirs, |dir| gather_row(dir))?;

    // One batched query for every row whose state the queue can still
    // change; a finished history costs no scheduler round-trip at all.
    let sched = Scheduler::for_machine(machine);
    let pending: Vec<JobRef> = rows.iter().filter_map(Row::job_to_query).collect();
    let statuses = sched.get_statuses_shown(&pending)?;

    let mut rows = rows.iter();
    for (inst, configs) in installations.iter().zip(&configs_per_inst) {
        if configs.is_empty() {
            continue;
        }
        if all {
            println!("{}", format!("[{}]", inst.alias).bold());
        }
        for (name, _) in configs {
            let row = rows.next().expect("one row per config");
            print_row(name, row, &statuses, long);
        }
    }
    Ok(())
}

// ---- build show ---------------------------------------------------------

/// `cactup build show [<name>]`: the config's build-attempt history.
fn show(ctx: &Ctx, name: Option<String>, long: bool) -> Res<()> {
    let machine = machine::resolve(ctx)?;
    let installation = Installation::resolve(ctx)?;
    show_impl(&machine, &installation, name.as_deref(), long)
}

fn show_impl(machine: &Machine, inst: &Installation, name: Option<&str>, long: bool) -> Res<()> {
    reconcile_finished_builds(inst)?;

    let name = match name {
        Some(n) => n.to_owned(),
        None => inst.meta()?.active_config()?.to_owned(),
    };
    let config_dir = inst.cactus_root().join("configs").join(&name);
    if !config_dir.is_dir() {
        bail!("no config named \"{name}\" in this installation (see `cactup config list`)");
    }

    println!("{}", name.bold());
    let ids = BuildAttempt::scan(&config_dir)?;
    if ids.is_empty() {
        println!("  state: {}", state_str(BuildDisplayState::NeverBuilt));
        println!(
            "  no build attempts yet (see `cactup build {name}` or `cactup build submit {name}`)"
        );
        return Ok(());
    }

    // Load the whole history first, so every attempt's live status comes out
    // of one batched query instead of a scheduler round-trip each (§10).
    let loaded: Vec<Res<BuildAttempt>> =
        ids.iter().map(|&id| BuildAttempt::open(&config_dir, id)).collect();
    let sched = Scheduler::for_machine(machine);
    let queries: Vec<JobRef> = loaded
        .iter()
        .filter_map(|a| a.as_ref().ok())
        .filter_map(|a| pending_job_query(&a.meta))
        .collect();
    let statuses = sched.get_statuses_shown(&queries)?;

    let latest_id = *ids.last().expect("checked non-empty above");
    match loaded.last().expect("checked non-empty above") {
        Ok(attempt) => {
            let lock_live = LinkLock::is_held_live(&attempt.running_lock_path())?;
            let query = pending_job_query(&attempt.meta).map(|j| statuses.get(j).copied());
            let state = shown_state(&attempt.meta, query, lock_live);
            println!("  state:       {}", state_str(state));
            println!("  machine:     {}", attempt.meta.machine);
            println!("  attempt-dir: {}", attempt.dir.display());
            println!("  job-id:      {}", attempt.meta.job_id);
            if let Some(r) = &attempt.meta.reservation {
                let alloc =
                    r.allocation.as_deref().map(|a| format!(", allocation {a}")).unwrap_or_default();
                println!(
                    "  reservation: queue {}, {} node(s), {} task(s), {} cpu(s)/task, wall {}{alloc}",
                    r.queue, r.nodes, r.tasks, r.cpus, r.walltime.canonical()
                );
            }
            let fmt = |ts: Option<chrono::DateTime<chrono::Utc>>| {
                ts.map(|d| d.to_rfc3339()).unwrap_or_else(|| "-".to_owned())
            };
            let t = &attempt.meta.timestamps;
            println!(
                "  timestamps:  created {}, submitted {}, started {}, finished {}",
                fmt(t.created),
                fmt(t.submitted),
                fmt(t.started),
                fmt(t.finished)
            );
            if long {
                match &attempt.meta.optionlist_source {
                    OptionlistSource::Optionlist(p) => println!("  optionlist:  {p}"),
                    OptionlistSource::Flavor(v) => println!("  flavor:      {v}"),
                }
                if let Some(u) = &attempt.meta.universe {
                    println!("  universe:    {}", u.name);
                }
            }
        }
        Err(e) => println!("  state:       {} ({e:#})", "BROKEN".red()),
    }

    println!("  attempts:");
    for (&id, attempt) in ids.iter().zip(&loaded) {
        let marker = if id == latest_id { " (latest)" } else { "" };
        match attempt {
            Ok(a) => {
                let lock_live = LinkLock::is_held_live(&a.running_lock_path()).unwrap_or(false);
                let query = pending_job_query(&a.meta).map(|j| statuses.get(j).copied());
                let state = shown_state(&a.meta, query, lock_live);
                println!(
                    "    {}{marker}: {} — {}",
                    BuildAttempt::dir_name(id),
                    state_str(state),
                    a.meta.decision
                );
            }
            Err(_) => println!("    {}{marker}: (no metadata)", BuildAttempt::dir_name(id)),
        }
    }
    Ok(())
}

// ---- build log ------------------------------------------------------------

/// Where `build log` reads stdout/stderr from: the frozen `STDOUT_FILE`/
/// `STDERR_FILE` vars when present (mirrors `sim log`/`test log`), else the
/// attempt's own conventional `build.{out,err}`.
fn build_log_sources(attempt: &BuildAttempt) -> [(&'static str, PathBuf); 2] {
    let mut out = attempt.out_path();
    let mut err = attempt.err_path();
    if let Some(toml::Value::String(s)) = attempt.meta.vars.get("STDOUT_FILE") {
        out = PathBuf::from(s);
    }
    if let Some(toml::Value::String(s)) = attempt.meta.vars.get("STDERR_FILE") {
        err = PathBuf::from(s);
    }
    [("stdout", out), ("stderr", err)]
}

/// `cactup build log [<name>]`: tail the latest attempt's stdout/stderr.
fn log(ctx: &Ctx, name: Option<String>, mode: crate::tail::FollowMode) -> Res<()> {
    let inst = Installation::resolve(ctx)?;
    let name = match name {
        Some(n) => n,
        None => inst.meta()?.active_config()?.to_owned(),
    };
    let config_dir = inst.cactus_root().join("configs").join(&name);
    let Some(id) = BuildAttempt::latest_id(&config_dir)? else {
        bail!("config \"{name}\" has no build attempts yet (nothing to show)");
    };
    let attempt = BuildAttempt::open(&config_dir, id)?;
    let sources = build_log_sources(&attempt);
    let subject = format!("{} attempt {}", name.bold(), BuildAttempt::dir_name(id));
    crate::tail::tail_log(&sources, mode, &subject)
}

// ---- build stop -------------------------------------------------------

/// `cactup build stop [<name>]`.
fn stop(ctx: &Ctx, name: Option<String>, force: bool) -> Res<()> {
    let machine = machine::resolve(ctx)?;
    let installation = Installation::resolve(ctx)?;
    stop_impl(&machine, &installation, name.as_deref(), force)
}

fn stop_impl(machine: &Machine, inst: &Installation, name: Option<&str>, force: bool) -> Res<()> {
    let name = match name {
        Some(n) => n.to_owned(),
        None => inst.meta()?.active_config()?.to_owned(),
    };
    let config_dir = inst.cactus_root().join("configs").join(&name);
    let Some(id) = BuildAttempt::latest_id(&config_dir)? else {
        println!("Config {} has no build attempts; nothing to stop", name.bold());
        return Ok(());
    };
    let mut attempt = BuildAttempt::open(&config_dir, id)?;
    let sched = Scheduler::for_machine(machine);
    if attempt.meta.outcome.is_some() {
        // Over, unless the scheduler still lists its job after the outcome
        // was recorded (asked only where it may have requeued it,
        // `requeue_query`: queued again, or leaving the queue) or could
        // not be asked: the stop is sent either way, harmless if the job is
        // leaving or gone; the outcome stands. Where it answered that the
        // job is gone or over, or nothing was asked, nothing is stopped.
        use crate::scheduler::{AfterEnd, EndedStop, StopCase};
        let answer = match requeue_query(&attempt.meta, &sched) {
            Some(job) => Some((job, sched.ask_after_end(job)?)),
            None => None,
        };
        let refused = |job: crate::scheduler::JobRef<'_>, said: &str| {
            println!(
                "The scheduler does not know build job {} (its stop command said: {said}): taken as ended",
                job.id
            )
        };
        // A stop the scheduler took for a job it could not be asked about,
        // or refused saying it does not know the job, settles the job:
        // recorded (`job-stopped`), so the next command takes it as ended
        // rather than ask a query that may keep failing. Any other failure
        // of the stop command is an error, nothing recorded.
        let settled = match answer {
            Some((job, AfterEnd::Listed(_))) => match sched.stop_judged(job, StopCase::Listed)? {
                EndedStop::Sent => {
                    println!(
                        "Asked the scheduler to stop build job {}, still listed after the attempt recorded \
                         its outcome (queued again, or leaving the queue)",
                        job.id.bold()
                    );
                    false
                }
                EndedStop::NotKnown(said) => {
                    refused(job, &said);
                    true
                }
            },
            Some((job, AfterEnd::Unanswered(failed))) => {
                match sched.stop_judged(job, StopCase::Unanswered(&failed))? {
                    EndedStop::Sent => println!(
                        "Could not ask the scheduler about build job {} (the attempt has recorded its \
                         outcome); asked it to stop the job in case it was queued again",
                        job.id.bold()
                    ),
                    EndedStop::NotKnown(said) => refused(job, &said),
                }
                true
            }
            Some((job, AfterEnd::Over(JobStatus::Unknown))) => {
                println!(
                    "Config {}'s latest build attempt already finished, and the scheduler does not know its \
                     job {}; nothing to stop",
                    name.bold(),
                    job.id
                );
                false
            }
            Some((job, AfterEnd::Over(_))) => {
                println!(
                    "Config {}'s latest build attempt already finished, and the scheduler lists its job {} as \
                     over; nothing to stop",
                    name.bold(),
                    job.id
                );
                false
            }
            None => {
                println!("Config {}'s latest build attempt already finished; nothing to stop", name.bold());
                false
            }
        };
        if settled {
            let _lock = BuildAttempt::record_lock(&attempt.dir)?;
            let mut fresh = BuildAttempt::open(&config_dir, id)?;
            fresh.meta.timestamps.job_stopped = Some(Utc::now());
            fresh.store_meta()?;
        }
        return Ok(());
    }

    // §7.9: `job_id` is a real scheduler id only when `submitted` — a
    // foreground build's job_id is this process's own pid (`build::build`),
    // and handing a pid to the scheduler's stop command is nonsense.
    if attempt.meta.submitted
        && let Some(host) = sched.recorded_elsewhere(attempt.meta.job()).map(str::to_owned)
    {
        // Another host's pid (§10): only the attempt's lock says whether it
        // runs there, and this host cannot stop it.
        if LinkLock::is_held_live(&attempt.running_lock_path())? {
            println!(
                "Build job {} runs on host {host}, where it was submitted, and this host cannot stop \
                 it: stop it there (`cactup build stop {name}` on {host}). Nothing recorded.",
                attempt.meta.job_id
            );
            return Ok(());
        }
        println!(
            "Build job {} was submitted on host {host}, and nothing of the attempt holds its lock: \
             taken as ended",
            attempt.meta.job_id
        );
    } else if attempt.meta.submitted {
        let job = attempt.meta.job().to_record();
        match sched.get_status(job.job())? {
            JobStatus::Running | JobStatus::Queued | JobStatus::Holding => match sched.stop(job.job())? {
                Stopped::Sent => println!("Stopped build job {}", job.id.bold()),
                // Its pid's process went between the query and the stop.
                Stopped::Gone => println!("Build job {} has ended; nothing to stop", job.id),
                // `recorded_elsewhere` above: not reached.
                Stopped::Elsewhere(host) => bail!("build job {} runs on host {host}: stop it there", job.id),
            },
            _ => println!(
                "Build job {} is not in the queue; nothing to stop via the scheduler",
                attempt.meta.job_id
            ),
        }
    } else if force {
        println!(
            "Config {}'s build is running locally (pid {}); cactup cannot stop a foreground \
             build for you — interrupt it (Ctrl-C) in the terminal running it.",
            name.bold(),
            attempt.meta.job_id
        );
    } else {
        println!(
            "Config {}'s build is running locally (pid {}) in its own terminal; interrupt it \
             there (Ctrl-C) to stop it.",
            name.bold(),
            attempt.meta.job_id
        );
    }

    // Either way: this attempt is done as far as cactup is concerned, so
    // `build list`/`build show` stop showing it as live forever.
    attempt.meta.outcome = Some(BuildOutcomeRecord { exit_status: None, complete: false });
    attempt.meta.timestamps.finished = Some(Utc::now());
    attempt.meta.status = Some("U".to_owned());
    attempt.store_meta()?;
    Ok(())
}

// ---- build prune ------------------------------------------------------

/// Default number of an attempt's most-recent siblings `build prune` keeps.
const DEFAULT_KEEP_ATTEMPTS: u32 = 10;

/// Whether `attempt` is still live: expressed through [`build_state`] so
/// `list`/`show`/`prune` all share one notion of "live".
fn attempt_is_live(attempt: &BuildAttempt, sched: &Scheduler) -> Res<bool> {
    if attempt.meta.outcome.is_some() {
        return Ok(false);
    }
    let status = match pending_job_query(&attempt.meta).map(|job| sched.get_status(job)) {
        None => None,
        Some(Ok(status)) => Some(status),
        // Kept: a query that failed says nothing of whether it runs.
        Some(Err(e)) => {
            if gix::interrupt::is_triggered() {
                bail!("interrupted");
            }
            eprintln!(
                "{} attempt {} kept: its job's status query failed ({})",
                "note:".yellow(),
                BuildAttempt::dir_name(attempt.id),
                format!("{e:#}").lines().next().unwrap_or_default()
            );
            return Ok(true);
        }
    };
    let lock_live = LinkLock::is_held_live(&attempt.running_lock_path())?;
    Ok(matches!(
        build_state(None, status, lock_live),
        BuildDisplayState::Running | BuildDisplayState::Queued | BuildDisplayState::Holding
    ))
}

fn prune(ctx: &Ctx, name: Option<String>, keep: Option<u32>) -> Res<()> {
    let machine = machine::resolve(ctx)?;
    let installation = Installation::resolve(ctx)?;
    prune_impl(&machine, &installation, name.as_deref(), keep)
}

/// `cactup build prune [<name>] [--keep N]` (default `N` =
/// [`DEFAULT_KEEP_ATTEMPTS`]): never automatic, and never touches a still-live
/// attempt — surprising deletion is worse than the disk it would reclaim.
fn prune_impl(machine: &Machine, inst: &Installation, name: Option<&str>, keep: Option<u32>) -> Res<()> {
    let name = match name {
        Some(n) => n.to_owned(),
        None => inst.meta()?.active_config()?.to_owned(),
    };
    let keep = keep.unwrap_or(DEFAULT_KEEP_ATTEMPTS) as usize;
    let config_dir = inst.cactus_root().join("configs").join(&name);
    let ids = BuildAttempt::scan(&config_dir)?;
    if ids.len() <= keep {
        println!(
            "Config {} has {} build attempt(s); nothing to prune (keeping {keep})",
            name.bold(),
            ids.len()
        );
        return Ok(());
    }

    let sched = Scheduler::for_machine(machine);
    let mut removed = Vec::new();
    let mut skipped = Vec::new();
    for &id in &ids[..ids.len() - keep] {
        if gix::interrupt::is_triggered() {
            bail!("interrupted");
        }
        let attempt = BuildAttempt::open(&config_dir, id)?;
        if attempt_is_live(&attempt, &sched)? {
            skipped.push(id);
            continue;
        }
        fs::remove_dir_all(&attempt.dir)
            .with_context(|| format!("Failed to remove {}", attempt.dir.display()))?;
        removed.push(id);
    }

    let ids_str =
        |ids: &[u32]| ids.iter().map(|&id| BuildAttempt::dir_name(id)).collect::<Vec<_>>().join(", ");
    if !removed.is_empty() {
        println!("Removed {} build attempt(s) of {}: {}", removed.len(), name.bold(), ids_str(&removed));
    }
    if !skipped.is_empty() {
        println!(
            "{}",
            format!(
                "Skipped {} build attempt(s) of {} that are, or may be, still live (not removed): {}",
                skipped.len(),
                name.bold(),
                ids_str(&skipped)
            )
            .yellow()
        );
    }
    if removed.is_empty() && skipped.is_empty() {
        println!("Nothing to prune.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::{BuildOpts, MakeJobs, TopologyFlags};
    use crate::database::Database;
    use crate::mdb::{Layer, Meta};
    use crate::walltime::Walltime;
    use std::fs;
    use std::path::PathBuf;

    fn fake_ctx(dir: &std::path::Path) -> Ctx {
        Ctx {
            globals: crate::args::GlobalOpts {
                verbose: false,
                trace: false,
                manifest_url: String::new(),
                mdb_path: None,
                machine: None,
                installation: None,
                hostname: None,
                knob: Vec::new(),
            },
            db: crate::database::Db::in_dir(dir),
        }
    }

    fn start_args(config_dir: Option<std::path::PathBuf>, attempt_id: Option<u32>) -> BuildStartArgs {
        BuildStartArgs {
            name: None,
            opts: BuildOpts::default_for_tests(),
            topology: bare_topology(),
            config_dir,
            attempt_id,
            block: false,
        }
    }

    fn bare_topology() -> TopologyFlags {
        TopologyFlags {
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
            wall_time: None,
            out: None,
            err: None,
        }
    }

    /// The compute-node path must reach `BuildAttempt::open` (and, beyond
    /// it, `build::execute`) WITHOUT resolving an installation first (D11):
    /// this DB has no active installation configured, so if `dispatch` ever
    /// called `Installation::resolve`, it would fail with "no active
    /// installation" rather than the attempt-open failure asserted below.
    /// Covers both the bare form and the explicit `build run` subcommand —
    /// a generated submit script is expected to invoke the latter.
    #[test]
    fn compute_node_path_skips_installation_resolution() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = fake_ctx(&tmp.path().join("db"));
        // No attempt actually exists at this path — proves we got as far as
        // (and no further than) trying to open it.
        let missing_config_dir = tmp.path().join("configs/sim");
        let compute_args = || start_args(Some(missing_config_dir.clone()), Some(0));

        // Bare form: `command` is None, so `dispatch` takes `start` itself.
        let err = dispatch(&ctx, compute_args(), None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Failed to read"), "expected an attempt-open failure, got: {msg}");
        assert!(!msg.contains("no active installation"), "installation was resolved: {msg}");

        // Explicit `build run` form: `dispatch` uses the subcommand's own
        // args, not the (irrelevant, here empty) bare `start`.
        let err = dispatch(&ctx, start_args(None, None), Some(BuildCommand::Run(compute_args())))
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("Failed to read"), "expected an attempt-open failure, got: {msg}");
        assert!(!msg.contains("no active installation"), "installation was resolved: {msg}");
    }

    // ---- build submit -----------------------------------------------------
    //
    // These exercise `run_with`/`submit_impl` directly, never through `Ctx`
    // (i.e. never through `machine::resolve`/`Mdb::open`): that always reads
    // the real `~/.cactup/machines` user overlay, which `--mdb-path` does not
    // override — the established test-isolation pattern (see `mdb/mod.rs`,
    // `sim/vars.rs`, `scheduler.rs`). `Machine` is instead built by hand,
    // exactly as `testsuite::run::tests::fake_machine` does.

    /// A machine that can submit builds: a `[build]` reservation, a
    /// buildsubmitscript, and a `[scheduler]` whose `submit` is a
    /// local shell command that echoes a fake job id (mirrors
    /// `testsuite::run::tests::fake_machine`). The `make` command is a fake
    /// `case "$2" in …` script, exactly like `build::tests::fake_tree` — that
    /// helper is private to `build::mod`'s own test module, so it is
    /// reproduced here with the extra submission plumbing that module
    /// doesn't need. `always_queued` makes the fake scheduler report every
    /// job as queued (for the "already live" guard tests); otherwise it
    /// reports nothing, matching real "not in the queue" behavior.
    fn fake_submit_machine(dir: &Path, make_body: &str, build_extra: &str, always_queued: bool) -> Machine {
        fs::create_dir_all(dir.join("optionlists")).unwrap();
        fs::create_dir_all(dir.join("runscripts")).unwrap();
        fs::create_dir_all(dir.join("submitscripts")).unwrap();
        fs::create_dir_all(dir.join("buildsubmitscripts")).unwrap();

        let fake_make = dir.join("fakemake");
        fs::write(&fake_make, format!("#!/bin/sh\ncase \"$2\" in\n{make_body}\nesac\n")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_make, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let (get_status, status_pattern, queued_pattern) = if always_queued {
            ("echo QUEUED", "QUEUED", "QUEUED")
        } else {
            ("true", "$^", "$^")
        };

        fs::write(
            dir.join("meta.toml"),
            format!(
                r#"
                [machine]
                nickname = "fake"

                [build]
                make = "{make} -j@MAKEJOBS@"
                {build_extra}

                [scheduler]
                submit = "echo [JOB-B@ATTEMPT_ID@]"
                submit-pattern = '\[(JOB-[^\]]*)\]'
                get-status = "{get_status}"
                status-pattern = "{status_pattern}"
                queued-pattern = "{queued_pattern}"
                running-pattern = "$^"
                holding-pattern = "$^"
                stop = "true"

                [queues.local]
                default = true
                max-walltime = "8:00:00"

                [submitscripts]
                "default" = ["local"]
                [runscripts]
                "default" = ["local"]
                [buildsubmitscripts]
                "default" = {{ queues = ["local"] }}
                "#,
                make = fake_make.display(),
            ),
        )
        .unwrap();
        fs::write(
            dir.join("optionlists/default.toml"),
            "[cactup]\n[cactup.runs-on]\nqueues = [\"local\"]\ngpu = false\n[options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        for s in ["runscripts/default.sh", "submitscripts/default.sh"] {
            fs::write(dir.join(s), "#!/bin/sh\n").unwrap();
        }
        fs::write(
            dir.join("buildsubmitscripts/default.sh"),
            "#!/bin/sh\n\
             echo cfg=@CONFIGURATION@ attempt=@ATTEMPT_ID@ q=@QUEUE@ n=@NODES@ t=@TASKS@ \
             c=@CPUS_PER_TASK@ w=@WALLTIME@ j=@MAKEJOBS@ job_name=@JOB_NAME@\n\
             exec @CACTUP@ build run @CONFIGURATION@ --config-dir=@CONFIG_DIR@ --attempt-id=@ATTEMPT_ID@\n",
        )
        .unwrap();

        let meta: Meta = toml::from_str(&fs::read_to_string(dir.join("meta.toml")).unwrap()).unwrap();
        meta.validate("fake").unwrap();
        Machine { name: "fake".to_owned(), dir: dir.to_owned(), layer: Layer::System, meta }
    }

    /// A config-completing make body: `<name>-config` drops the marker,
    /// `<name>` drops the executable. Mirrors `build::tests::fake_tree`'s own
    /// callers.
    fn completing_make_body(cactus: &Path, name: &str) -> String {
        format!(
            "{name}-config) cd {c}/configs/{name}/config-data && touch cctk_Config.h ;;\n\
             {name}) mkdir -p {c}/exe && touch {c}/exe/cactus_{name} ;;",
            c = cactus.display(),
        )
    }

    fn submit_args(name: &str, topology: TopologyFlags) -> BuildStartArgs {
        BuildStartArgs {
            name: Some(name.to_owned()),
            opts: BuildOpts::default_for_tests(),
            topology,
            config_dir: None,
            attempt_id: None,
            block: false,
        }
    }

    /// End-to-end: submit stages the reservation into the generated script,
    /// records the parsed job id, and the compute-node re-entry (`build run
    /// --config-dir --attempt-id`, exactly what the script itself invokes)
    /// completes the build and records the outcome.
    #[test]
    fn submit_then_compute_run_records_results() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "make-jobs = 4\ncpus-per-task = 4\nnodes = 1\ntasks = 1\nwalltime = \"2:00:00\"\n\
             queue = \"local\"",
            false,
        );
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        submit_impl(&inst, &machine, &db, &submit_args("sim", bare_topology()), Some("subhost")).unwrap();

        let config_dir = cactus.join("configs/sim");
        let attempt = BuildAttempt::open(&config_dir, 0).unwrap();
        // The [build] reservation landed in the frozen attempt metadata...
        let reservation = attempt.meta.reservation.as_ref().expect("reservation recorded");
        assert_eq!(reservation.queue, "local");
        assert_eq!((reservation.nodes, reservation.tasks, reservation.cpus), (1, 1, 4));
        assert_eq!(reservation.walltime, Walltime::parse("2:00:00").unwrap());
        // ...the MAKEJOBS/CPUS_PER_TASK coupling kept the two equal...
        let script = fs::read_to_string(attempt.submit_script_path()).unwrap();
        assert!(script.contains("j=4"), "{script}");
        assert!(script.contains("c=4"), "{script}");
        assert!(script.contains("q=local n=1 t=1"), "{script}");
        assert!(script.contains("job_name=build-sim"), "{script}");
        // ...and the job id the fake `submit` echoed was parsed and stored.
        assert_eq!(attempt.meta.job_id, "JOB-B0");
        assert!(attempt.meta.submitted);
        assert!(attempt.meta.timestamps.submitted.is_some());

        // The compute-node re-entry: exactly what the generated script's own
        // `exec` line invokes.
        let ctx = fake_ctx(&tmp.path().join("db"));
        run(&ctx, start_args(Some(config_dir.clone()), Some(0))).unwrap();

        let built = crate::build::ConfigMeta::load(&cactus, "sim").unwrap().expect("config recorded");
        assert!(built.built.is_some());
        assert!(crate::build::is_complete(&cactus, "sim"));
    }

    /// §6.1: a `.py` build-submit script's `ENV_SETUP` is the block a `.sh`
    /// one gets prepended: the submit phase's, in the submit universe, never
    /// the build universe's setup or provisioning.
    #[test]
    fn sh_and_py_build_submit_scripts_get_the_same_environment() {
        let generated = |python: bool| {
            let tmp = tempfile::tempdir().unwrap();
            let cactus = tmp.path().join("inst/Cactus");
            let dir = tmp.path().join("mdb/fake");
            let mut machine = fake_submit_machine(
                &dir,
                &completing_make_body(&cactus, "sim"),
                "make-jobs = 4\nuniverse = \"bu\"\n\
                 [universes.bu]\nenv-setup = \"export FROM=bu\"\nprovision = \"echo PROV-BU\"\n\
                 [environment]\nenv-setup = \"export FROM=machine\"\nsubmit-provision = \"echo PROV-SUBMIT\"",
                false,
            );
            if python {
                fs::remove_file(dir.join("buildsubmitscripts/default.sh")).unwrap();
                fs::write(
                    dir.join("buildsubmitscripts/default.py"),
                    "print('#!/bin/sh')\nprint(ENV_SETUP)\nprint('exec @CACTUP@')\n",
                )
                .unwrap();
                machine = Machine { meta: toml::from_str(&fs::read_to_string(dir.join("meta.toml")).unwrap()).unwrap(), ..machine };
            }
            fs::create_dir_all(cactus.join("thornlists")).unwrap();
            fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
            let inst = Installation::new("et", tmp.path().join("inst"));
            submit_impl(&inst, &machine, &Database::new(), &submit_args("sim", bare_topology()), Some("subhost")).unwrap();
            let attempt = BuildAttempt::open(&cactus.join("configs/sim"), 0).unwrap();
            assert_eq!(attempt.meta.universe.as_ref().map(|u| u.name.as_str()), Some("bu"));
            fs::read_to_string(attempt.submit_script_path()).unwrap()
        };
        for python in [false, true] {
            let script = generated(python);
            let (setup, provisioning) = (script.find("export FROM=machine\n"), script.find("echo PROV-SUBMIT\n"));
            assert!(setup.is_some() && setup < provisioning, "python {python}:\n{script}");
            assert!(!script.contains("FROM=bu") && !script.contains("PROV-BU"), "python {python}:\n{script}");
        }
    }

    // ---- the build job's queue is not the binary's (§7.9) ------------------

    /// A machine with a GPU and a CPU partition, a GPU and a CPU optionlist
    /// (each `compatible-queues`-restricted to its own partition), and one
    /// buildsubmitscript per partition. The script names deliberately match
    /// no optionlist name, and each script stamps its own name into the
    /// generated file, so a test can see which one was routed.
    fn two_partition_machine(dir: &Path, cactus: &Path) -> Machine {
        fake_submit_machine(dir, &completing_make_body(cactus, "sim"), "", false);
        fs::write(
            dir.join("meta.toml"),
            format!(
                r#"
                [machine]
                nickname = "fake"

                [build]
                make = "{make} -j@MAKEJOBS@"
                make-jobs = 4

                [scheduler]
                submit = "echo [JOB-B@ATTEMPT_ID@]"
                submit-pattern = '\[(JOB-[^\]]*)\]'
                get-status = "true"
                status-pattern = "$^"
                queued-pattern = "$^"
                running-pattern = "$^"
                holding-pattern = "$^"
                stop = "true"

                [queues.gpu]
                default = true
                gpu = true
                max-walltime = "8:00:00"
                [queues.cpu]
                max-walltime = "8:00:00"

                [submitscripts]
                "default" = {{ queues = ["gpu", "cpu"] }}
                [runscripts]
                "default" = {{ queues = ["gpu", "cpu"] }}
                [buildsubmitscripts]
                "on-gpu" = {{ queues = ["gpu"] }}
                "on-cpu" = ["cpu"]
                "#,
                make = dir.join("fakemake").display(),
            ),
        )
        .unwrap();
        fs::remove_file(dir.join("optionlists/default.toml")).unwrap();
        fs::write(
            dir.join("optionlists/gpu.toml"),
            "[cactup]\ndefault = true\n[cactup.runs-on]\nqueues = [\"gpu\"]\ngpu = true\n[options]\nVERSION = \"1\"\n",
        )
        .unwrap();
        fs::write(
            dir.join("optionlists/cpu.toml"),
            "[cactup]\n[cactup.runs-on]\nqueues = [\"cpu\"]\ngpu = false\n[options]\nVERSION = \"1\"\n",
        )
        .unwrap();
        fs::remove_file(dir.join("buildsubmitscripts/default.sh")).unwrap();
        for v in ["on-gpu", "on-cpu"] {
            fs::write(
                dir.join(format!("buildsubmitscripts/{v}.sh")),
                format!("#!/bin/sh\necho script={v} q=@QUEUE@\n"),
            )
            .unwrap();
        }
        let meta: Meta = toml::from_str(&fs::read_to_string(dir.join("meta.toml")).unwrap()).unwrap();
        meta.validate("fake").unwrap();
        Machine { name: "fake".to_owned(), dir: dir.to_owned(), layer: Layer::System, meta }
    }

    /// Submit `--flavor flavor` to `queue` (or the machine default) on the
    /// two-partition machine; returns the generated buildsubmitscript.
    fn submit_flavor(flavor: &str, queue: Option<&str>) -> Res<String> {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine = two_partition_machine(&tmp.path().join("mdb/fake"), &cactus);
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let mut topology = bare_topology();
        topology.queue = queue.map(str::to_owned);
        let mut args = submit_args("sim", topology);
        args.opts.flavor = Some(flavor.to_owned());
        submit_impl(&inst, &machine, &Database::new(), &args, Some("subhost"))?;
        let attempt = BuildAttempt::open(&cactus.join("configs/sim"), 0).unwrap();
        Ok(fs::read_to_string(attempt.submit_script_path()).unwrap())
    }

    /// Build jobs take their queue from the `build-queue` knob ahead of
    /// `[build].queue` (here: none, so the default `gpu`), pass over a knob
    /// naming a queue no build-submit script serves, and never read the
    /// `queue` knob, which is for sims and tests.
    #[test]
    fn build_queue_knob_routes_build_jobs() {
        let submit_with = |knobs: &[(&str, &str)]| -> String {
            let tmp = tempfile::tempdir().unwrap();
            let cactus = tmp.path().join("inst/Cactus");
            let machine = two_partition_machine(&tmp.path().join("mdb/fake"), &cactus);
            fs::create_dir_all(cactus.join("thornlists")).unwrap();
            fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
            let inst = Installation::new("et", tmp.path().join("inst"));
            let mut db = Database::new();
            for (k, v) in knobs {
                db.set_knob(k, (*v).to_owned());
            }
            let mut args = submit_args("sim", bare_topology());
            args.opts.flavor = Some("cpu".to_owned());
            submit_impl(&inst, &machine, &db, &args, Some("subhost")).unwrap();
            let attempt = BuildAttempt::open(&cactus.join("configs/sim"), 0).unwrap();
            fs::read_to_string(attempt.submit_script_path()).unwrap()
        };
        assert!(submit_with(&[("build-queue", "cpu")]).contains("q=cpu"));
        assert!(submit_with(&[("build-queue", "nowhere")]).contains("q=gpu"), "an unusable knob is skipped");
        assert!(submit_with(&[("queue", "cpu")]).contains("q=gpu"), "the `queue` knob is not for builds");
    }

    /// A CPU-only optionlist compiles on a GPU queue: its compatible-queues
    /// name where the binary may run, not where `make` may.
    #[test]
    fn cpu_optionlist_builds_on_a_gpu_queue() {
        let script = submit_flavor("cpu", Some("gpu")).unwrap();
        assert!(script.contains("script=on-gpu q=gpu"), "{script}");
    }

    /// A GPU optionlist compiles on a non-GPU queue: compiling CUDA code needs
    /// no GPU, so the binary's GPU cross-check has no say over the build job.
    #[test]
    fn gpu_optionlist_builds_on_a_cpu_queue() {
        let script = submit_flavor("gpu", Some("cpu")).unwrap();
        assert!(script.contains("script=on-cpu q=cpu"), "{script}");
    }

    /// The optionlist `--flavor` never picks the buildsubmitscript: with no
    /// -q, the default queue routes it — even though no script shares the
    /// optionlist's name, and the optionlist itself only runs elsewhere.
    #[test]
    fn flavor_never_selects_the_buildsubmitscript() {
        let script = submit_flavor("cpu", None).unwrap();
        assert!(script.contains("script=on-gpu q=gpu"), "{script}");
    }

    // ---- build submit --block (§7.9) --------------------------------------

    /// With `[scheduler].blocking-submit` declared, `--block` runs THAT
    /// command, not `submit` — the two echo different job ids here precisely
    /// so the recorded one proves which was taken.
    #[test]
    fn block_takes_the_machines_blocking_submit_when_it_has_one() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let mut machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            false,
        );
        machine.meta.scheduler.blocking_submit = Some("echo [JOB-WAITED@ATTEMPT_ID@]".to_owned());
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        let mut args = submit_args("sim", bare_topology());
        args.block = true;
        submit_impl(&inst, &machine, &db, &args, Some("h")).unwrap();

        let attempt = BuildAttempt::open(&cactus.join("configs/sim"), 0).unwrap();
        assert_eq!(attempt.meta.job_id, "JOB-WAITED0");
        // Recorded from inside the wait, so a Ctrl-C mid-build still leaves
        // the queued job named on disk.
        assert!(attempt.meta.submitted);
        assert!(attempt.meta.timestamps.submitted.is_some());
    }

    /// `sbatch --wait` writing into a pipe can hold back its "Submitted batch
    /// job" line until the job is over, so the job id may only reach the
    /// callback after the compute node has recorded the build's outcome.
    /// Recording the job id must not erase that outcome. The fake
    /// `blocking-submit` plays both parts: it writes the outcome the way the
    /// compute node would, and only then reports the job id.
    #[test]
    fn a_late_job_id_keeps_the_outcome_the_compute_node_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let mut machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            false,
        );
        machine.meta.scheduler.blocking_submit = Some(
            "printf '\\n[outcome]\\ncomplete = true\\n' >> @CONFIG_DIR@/.cactup-builds/0000/build.toml && \
             echo [JOB-LATE@ATTEMPT_ID@]"
                .to_owned(),
        );
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        let mut args = submit_args("sim", bare_topology());
        args.block = true;
        submit_impl(&inst, &machine, &db, &args, Some("h")).unwrap();

        let attempt = BuildAttempt::open(&cactus.join("configs/sim"), 0).unwrap();
        assert_eq!(attempt.meta.job_id, "JOB-LATE0");
        assert!(attempt.meta.submitted);
        let outcome = attempt.meta.outcome.as_ref().expect("the recorded outcome survived the late job id");
        assert!(outcome.complete);
    }

    /// Without the key, `--block` is not a different submission — it is the
    /// ordinary one plus a wait the caller performs afterward, so what
    /// reaches the scheduler here is unchanged.
    #[test]
    fn block_without_a_blocking_submit_key_submits_normally() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            false,
        );
        assert!(machine.meta.scheduler.blocking_submit.is_none());
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        let mut args = submit_args("sim", bare_topology());
        args.block = true;
        submit_impl(&inst, &machine, &db, &args, Some("h")).unwrap();

        let attempt = BuildAttempt::open(&cactus.join("configs/sim"), 0).unwrap();
        assert_eq!(attempt.meta.job_id, "JOB-B0");
        assert!(attempt.meta.submitted);
    }

    /// The emulated wait — what every machine without a `blocking-submit`
    /// key gets — reads its verdict from the attempt's own recorded outcome,
    /// exactly as the native route does once its command returns. Here the
    /// build is already finished before the wait starts, which is precisely
    /// the state a native blocking submit hands over, so this covers the tail
    /// end of both routes.
    #[test]
    fn the_block_wait_reports_the_attempts_recorded_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            false,
        );
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        let mut args = submit_args("sim", bare_topology());
        args.block = true;
        submit_impl(&inst, &machine, &db, &args, Some("h")).unwrap();

        // The compute node runs and records a completed build...
        let config_dir = cactus.join("configs/sim");
        let ctx = fake_ctx(&tmp.path().join("db"));
        run(&ctx, start_args(Some(config_dir.clone()), Some(0))).unwrap();
        assert!(BuildAttempt::open(&config_dir, 0).unwrap().meta.outcome.is_some());

        // ...so the wait returns straight away, and says the build worked.
        watch_build(&machine, &config_dir, 0, Watch::Block).unwrap();

        // Flip that same outcome to a failure: the wait must now fail too,
        // and point at the two files that say why. A relayed exit code is
        // never consulted — there is none here at all.
        let mut attempt = BuildAttempt::open(&config_dir, 0).unwrap();
        attempt.meta.outcome = Some(BuildOutcomeRecord { exit_status: Some(2), complete: false });
        attempt.store_meta().unwrap();

        let err = format!("{:#}", watch_build(&machine, &config_dir, 0, Watch::Block).unwrap_err());
        assert!(err.contains("build of sim failed"), "{err}");
        assert!(err.contains("build.out"), "{err}");
        assert!(err.contains("build.err"), "{err}");
    }

    /// `build run` never queues anything, so `--block` has nothing to wait
    /// for. Refused before the compute-node branch and before any resolution
    /// (D11), and the message points at the command that does support it.
    #[test]
    fn build_run_refuses_block() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = fake_ctx(&tmp.path().join("db"));
        let mut args = start_args(None, None);
        args.block = true;

        let err = dispatch(&ctx, start_args(None, None), Some(BuildCommand::Run(args))).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("--block"), "{msg}");
        assert!(msg.contains("build submit --block"), "{msg}");
        // Nothing was resolved on the way to the refusal.
        assert!(!msg.contains("no active installation"), "{msg}");
    }

    /// The bare `cactup build` can only discover that it is NOT submitting
    /// after reading the MDB, so its refusal has to name which of the three
    /// reasons decided it — the fix differs for each.
    #[test]
    fn block_on_a_foreground_build_names_the_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let mut machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            false,
        );

        // 1. A virtual-executable build is never queued (§7.7).
        let mut opts = BuildOpts::default_for_tests();
        opts.virtual_executable = Some(PathBuf::from("/bin/true"));
        let msg = format!("{:#}", no_queue_to_block_on(&machine, &opts));
        assert!(msg.contains("--virtual-executable"), "{msg}");

        // 2. The machine forces the foreground path.
        let opts = BuildOpts::default_for_tests();
        machine.meta.build.default_action = Some(BuildAction::Run);
        let msg = format!("{:#}", no_queue_to_block_on(&machine, &opts));
        assert!(msg.contains("default-action"), "{msg}");

        // 3. The machine cannot submit builds at all — named piece by piece,
        // the same wording `build submit` itself refuses with.
        machine.meta.build.default_action = None;
        machine.meta.scheduler.submit = None;
        let msg = format!("{:#}", no_queue_to_block_on(&machine, &opts));
        assert!(msg.contains("[scheduler].submit"), "{msg}");

        // Every one of them explains what --block was for.
        assert!(msg.contains("runs in the foreground"), "{msg}");
    }

    /// §9.3's no-leak rule, mirrored for builds: a buildsubmitscript naming a
    /// simulation-only variable must fail loudly at substitution time, not
    /// silently see an empty string.
    #[test]
    fn submit_script_naming_a_simulation_var_fails_loudly() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine_dir = tmp.path().join("mdb/fake");
        let machine = fake_submit_machine(&machine_dir, &completing_make_body(&cactus, "sim"), "", false);
        fs::write(
            machine_dir.join("buildsubmitscripts/default.sh"),
            "#!/bin/sh\necho @SIMULATION_NAME@\n",
        )
        .unwrap();
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        let err =
            submit_impl(&inst, &machine, &db, &submit_args("sim", bare_topology()), Some("h")).unwrap_err();
        assert!(format!("{err:#}").contains("SIMULATION_NAME"), "{err:#}");
    }

    /// A machine that never declares `[buildsubmitscripts]` refuses
    /// with a message naming exactly what is missing.
    #[test]
    fn submit_refused_without_a_buildsubmitscript_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine_dir = tmp.path().join("mdb/fake");
        let mut machine =
            fake_submit_machine(&machine_dir, &completing_make_body(&cactus, "sim"), "", false);
        machine.meta.buildsubmitscripts.routes.clear();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        let err =
            submit_impl(&inst, &machine, &db, &submit_args("sim", bare_topology()), Some("h")).unwrap_err();
        assert!(format!("{err:#}").contains("buildsubmitscript"), "{err:#}");
    }

    /// A machine with a buildsubmitscript but no `[scheduler].submit`
    /// command is refused too, naming that piece instead.
    #[test]
    fn submit_refused_without_a_scheduler_submit_command_names_it() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine_dir = tmp.path().join("mdb/fake");
        let mut machine =
            fake_submit_machine(&machine_dir, &completing_make_body(&cactus, "sim"), "", false);
        machine.meta.scheduler.submit = None;
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        let err =
            submit_impl(&inst, &machine, &db, &submit_args("sim", bare_topology()), Some("h")).unwrap_err();
        assert!(format!("{err:#}").contains("[scheduler].submit"), "{err:#}");
    }

    /// `--virtual-executable` copies a prebuilt binary and runs no `make`;
    /// queueing it is refused outright.
    #[test]
    fn submit_refuses_virtual_executable() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            false,
        );
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();
        let mut args = submit_args("sim", bare_topology());
        args.opts.virtual_executable = Some(PathBuf::from("/bin/true"));

        let err = submit_impl(&inst, &machine, &db, &args, Some("h")).unwrap_err();
        assert!(format!("{err:#}").contains("--virtual-executable"), "{err:#}");
    }

    /// Builds never chain: a second submit — and a foreground `build run` —
    /// of the same config are both refused while the first attempt is still
    /// live (per the fake scheduler, everything reads as queued); `-f` stops
    /// the old job and proceeds.
    #[test]
    fn second_submit_and_foreground_run_refused_while_one_is_live() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            true,
        );
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        submit_impl(&inst, &machine, &db, &submit_args("sim", bare_topology()), Some("h")).unwrap();

        // A second submit is refused...
        let err =
            submit_impl(&inst, &machine, &db, &submit_args("sim", bare_topology()), Some("h")).unwrap_err();
        assert!(format!("{err:#}").contains("already has a live build attempt"), "{err:#}");

        // ...and so is a foreground run of the same config.
        let mut run_args = start_args(None, None);
        run_args.name = Some("sim".to_owned());
        let err = run_with(&machine, &inst, run_args).unwrap_err();
        assert!(format!("{err:#}").contains("already has a live build attempt"), "{err:#}");

        // -f stops the old job (our fake `stop` always succeeds) and a fresh
        // submit proceeds, minting attempt 0001.
        let mut forced = submit_args("sim", bare_topology());
        forced.opts.force = true;
        submit_impl(&inst, &machine, &db, &forced, Some("h")).unwrap();
        assert_eq!(BuildAttempt::latest_id(&cactus.join("configs/sim")).unwrap(), Some(1));
    }

    /// A foreground attempt's job id is the pid of the cactup that ran it;
    /// once that cactup is gone the pid can be anyone's, and the scheduler
    /// (here: every id reads as queued, as `ps` reads a reused pid) must
    /// never be asked about it — whether the attempt finished or was killed
    /// before recording an outcome. Neither `build` nor `-f` stops anything,
    /// and `build list`'s state matrix calls the killed one STALE.
    #[test]
    fn a_foreground_attempts_pid_is_never_taken_for_a_job() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let mut machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            true,
        );
        let stopped = tmp.path().join("stopped");
        machine.meta.scheduler.stop = Some(format!("touch {}", stopped.display()));
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();
        let config_dir = cactus.join("configs/sim");
        let foreground = |force: bool| {
            let mut args = start_args(None, None);
            args.name = Some("sim".to_owned());
            args.opts.force = force;
            run_with(&machine, &inst, args)
        };

        foreground(false).unwrap();
        let finished = BuildAttempt::open(&config_dir, 0).unwrap();
        assert!(!finished.meta.submitted && finished.meta.outcome.is_some());
        assert_eq!(finished.meta.job_id, std::process::id().to_string());

        // Finished: a second foreground build and a submit both go ahead.
        foreground(false).unwrap();
        submit_impl(&inst, &machine, &db, &submit_args("sim", bare_topology()), Some("h")).unwrap();
        assert!(!stopped.exists(), "the scheduler's stop ran on a foreground attempt's pid");

        // Killed before recording an outcome: the same, -f included.
        let mut killed = BuildAttempt::open(&config_dir, BuildAttempt::latest_id(&config_dir).unwrap().unwrap()).unwrap();
        killed.meta.submitted = false;
        killed.meta.outcome = None;
        killed.meta.job_id = std::process::id().to_string();
        killed.store_meta().unwrap();
        assert_eq!(pending_job_query(&killed.meta), None);
        let sched = Scheduler::for_machine(&machine);
        assert!(!attempt_is_live(&killed, &sched).unwrap());
        guard_no_live_attempt(&config_dir, "sim", &sched, false).unwrap();
        guard_no_live_attempt(&config_dir, "sim", &sched, true).unwrap();
        assert!(!stopped.exists(), "the scheduler's stop ran on a foreground attempt's pid");
    }

    /// A nameless build in the null-config state explains the naming rule
    /// rather than just reporting the absence — both the first-build case
    /// (no configs at all) and the "configs exist, none active" one, on the
    /// foreground and submit paths alike.
    #[test]
    fn nameless_build_in_null_config_state_explains_the_naming_rule() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "",
            false,
        );
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists").join(crate::installation::LIVE_THORNLIST), "A/B\n").unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let db = Database::new();

        let err = format!("{:#}", run_with(&machine, &inst, start_args(None, None)).unwrap_err());
        assert!(err.contains("first build has to name it"), "{err}");
        assert!(err.contains("cactup build <name>"), "{err}");
        let submitted = format!(
            "{:#}",
            submit_impl(&inst, &machine, &db, &start_args(None, None), Some("h")).unwrap_err()
        );
        assert_eq!(submitted, err, "both start paths must give the same hint");

        // A config on disk with nothing active: point at it, not at creation.
        fs::create_dir_all(cactus.join("configs/sim")).unwrap();
        let err = format!("{:#}", run_with(&machine, &inst, start_args(None, None)).unwrap_err());
        assert!(err.contains("Configs here: sim"), "{err}");
        assert!(err.contains("cactup config use <name>"), "{err}");
    }

    /// §7.9's auto-selection table, exercised directly against `choose_submit`
    /// (no MDB/Ctx involved — see the module-level note on test isolation).
    #[test]
    fn auto_selection_matrix() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        // `Machine` (its `Meta`) carries no `Clone`, so each variant of the
        // table gets its own fresh fixture rather than a mutated copy of one.
        let fresh = |sub: &str| {
            fake_submit_machine(&tmp.path().join("mdb").join(sub), &completing_make_body(&cactus, "sim"), "", false)
        };

        let can_submit = fresh("can");
        let mut cannot_submit = fresh("cannot");
        cannot_submit.meta.buildsubmitscripts.routes.clear();

        let opts = BuildOpts::default_for_tests();

        // unset + can submit -> submit.
        assert!(choose_submit(&can_submit, &opts).unwrap());
        // unset + cannot submit -> run.
        assert!(!choose_submit(&cannot_submit, &opts).unwrap());

        let mut run_forced = fresh("run_forced");
        run_forced.meta.build.default_action = Some(BuildAction::Run);
        // "run" always runs, even when submission would be possible.
        assert!(!choose_submit(&run_forced, &opts).unwrap());

        let mut submit_forced = fresh("submit_forced");
        submit_forced.meta.build.default_action = Some(BuildAction::Submit);
        // "submit" + possible -> submit.
        assert!(choose_submit(&submit_forced, &opts).unwrap());

        let mut submit_impossible = fresh("submit_impossible");
        submit_impossible.meta.buildsubmitscripts.routes.clear();
        submit_impossible.meta.build.default_action = Some(BuildAction::Submit);
        // "submit" + impossible -> hard error naming the missing piece.
        let err = choose_submit(&submit_impossible, &opts).unwrap_err();
        assert!(format!("{err:#}").contains("buildsubmitscript"), "{err:#}");

        // A virtual-executable build always runs, regardless of the table.
        let mut virt_opts = BuildOpts::default_for_tests();
        virt_opts.virtual_executable = Some(PathBuf::from("/bin/true"));
        assert!(!choose_submit(&submit_forced, &virt_opts).unwrap());
    }

    #[test]
    fn make_jobs_cpus_coupling_table() {
        use crate::build::reconcile_make_jobs;
        use crate::template::VarValue;

        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let mut machine = fake_submit_machine(
            &tmp.path().join("mdb/fake"),
            &completing_make_body(&cactus, "sim"),
            "make-jobs = 8\ncpus-per-task = 8",
            false,
        );

        // -j N, no -c: both become N, overriding the machine defaults.
        let mut f = bare_topology();
        assert_eq!(reconcile_make_jobs(&mut f, Some(MakeJobs::Count(2)), &machine), VarValue::Int(2));
        assert_eq!(f.cpus, Some(2));

        // -c N, no -j: MAKEJOBS follows -c, not the machine default.
        let mut f = bare_topology();
        f.cpus = Some(3);
        assert_eq!(reconcile_make_jobs(&mut f, None, &machine), VarValue::Int(3));
        assert_eq!(f.cpus, Some(3));

        // -j max, no -c: shell expr, cpus falls to [build].cpus-per-task.
        let mut f = bare_topology();
        assert_eq!(
            reconcile_make_jobs(&mut f, Some(MakeJobs::Max), &machine),
            VarValue::Str("$(nproc 2>/dev/null || echo 1)".to_owned())
        );
        assert_eq!(f.cpus, Some(8));

        // -j max, no -c, no [build].cpus-per-task either: left for the
        // ordinary queue-default chain (resolve_topology owns it from here).
        machine.meta.build.cpus_per_task = None;
        let mut f = bare_topology();
        reconcile_make_jobs(&mut f, Some(MakeJobs::Max), &machine);
        assert_eq!(f.cpus, None);
        machine.meta.build.cpus_per_task = Some(8);

        // both given: as given, cpus untouched even when it disagrees.
        let mut f = bare_topology();
        f.cpus = Some(2);
        assert_eq!(reconcile_make_jobs(&mut f, Some(MakeJobs::Count(4)), &machine), VarValue::Int(4));
        assert_eq!(f.cpus, Some(2));

        // neither: MAKEJOBS = [build].make-jobs; cpus = [build].cpus-per-task.
        let mut f = bare_topology();
        assert_eq!(reconcile_make_jobs(&mut f, None, &machine), VarValue::Int(8));
        assert_eq!(f.cpus, Some(8));

        // neither, and [build].cpus-per-task unset: cpus falls to make-jobs.
        machine.meta.build.cpus_per_task = None;
        let mut f = bare_topology();
        assert_eq!(reconcile_make_jobs(&mut f, None, &machine), VarValue::Int(8));
        assert_eq!(f.cpus, Some(8));
    }

    // ---- build list/show/log/stop/prune ------------------------------------
    //
    // Same test-isolation discipline as the submit tests above: `list_impl`/
    // `show_impl`/`stop_impl`/`prune_impl` take hand-built `Machine`/
    // `Installation` fixtures directly, never through `Ctx`/`Mdb::open`.

    fn sample_config_meta_named(name: &str) -> crate::build::ConfigMeta {
        toml::from_str(&format!(
            r#"
            schema = 1
            name = "{name}"
            flavor = "default"
            thornlist = "{name}.th"
            machine = "fake"
            config-id = "cfg-{name}"
            build-id = "build-{name}"
            "#
        ))
        .unwrap()
    }

    /// A minimal `BuildMeta` fixture for one attempt — deliberately not
    /// reusing `build::attempt::tests::sample_meta`, which is private to that
    /// module.
    fn attempt_meta(
        config_dir: &Path,
        cactus_root: &Path,
        config: &str,
        id: u32,
        job_id: &str,
        submitted: bool,
        outcome: Option<BuildOutcomeRecord>,
    ) -> BuildMeta {
        BuildMeta {
            schema: crate::database::SCHEMA,
            attempt_id: id,
            config: config.to_owned(),
            optionlist_source: OptionlistSource::Flavor("default".to_owned()),
            machine: "fake".to_owned(),
            alias: "et".to_owned(),
            config_dir: config_dir.to_owned(),
            cactus_root: cactus_root.to_owned(),
            install_root: cactus_root.to_owned(),
            submitted,
            job_id: job_id.to_owned(),
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
            config_meta: sample_config_meta_named(config),
            vars: Default::default(),
            knobs: Default::default(),
            // A recorded outcome is recorded with its time, as `execute` does.
            timestamps: crate::build::attempt::Timestamps {
                finished: outcome.is_some().then(Utc::now),
                ..Default::default()
            },
            outcome,
        }
    }

    fn make_attempt(
        config_dir: &Path,
        cactus_root: &Path,
        config: &str,
        id: u32,
        job_id: &str,
        submitted: bool,
        outcome: Option<BuildOutcomeRecord>,
    ) -> BuildAttempt {
        let meta = attempt_meta(config_dir, cactus_root, config, id, job_id, submitted, outcome);
        BuildAttempt::create(BuildAttempt::attempt_dir(config_dir, id), meta).unwrap()
    }

    /// The §7.9 state matrix, exercised directly against the pure
    /// `build_state` — no scheduler, no filesystem.
    #[test]
    fn build_state_matrix() {
        let complete = BuildOutcomeRecord { exit_status: Some(0), complete: true };
        let failed = BuildOutcomeRecord { exit_status: Some(1), complete: false };

        // A recorded outcome wins outright, whatever the live status/lock say.
        assert_eq!(build_state(Some(&complete), Some(JobStatus::Running), true), BuildDisplayState::Built);
        assert_eq!(build_state(Some(&failed), None, false), BuildDisplayState::Failed);

        // No outcome: the live queue status, when there is one.
        assert_eq!(build_state(None, Some(JobStatus::Running), false), BuildDisplayState::Running);
        assert_eq!(build_state(None, Some(JobStatus::Queued), false), BuildDisplayState::Queued);
        assert_eq!(build_state(None, Some(JobStatus::Holding), false), BuildDisplayState::Holding);

        // No outcome, job gone (or never a real scheduler id) — running.lock
        // breaks the tie between "still running" and "died silently".
        assert_eq!(build_state(None, Some(JobStatus::Unknown), true), BuildDisplayState::Running);
        assert_eq!(build_state(None, Some(JobStatus::Unknown), false), BuildDisplayState::Stale);
        assert_eq!(build_state(None, Some(JobStatus::Error), true), BuildDisplayState::Running);
        assert_eq!(build_state(None, Some(JobStatus::Error), false), BuildDisplayState::Stale);
        assert_eq!(build_state(None, None, true), BuildDisplayState::Running);
        assert_eq!(build_state(None, None, false), BuildDisplayState::Stale);
    }

    /// A machine whose `get-status` command logs every distinct job id it is
    /// asked about, mirroring `sim::manage::tests::batched_status_query_*`.
    fn logging_status_machine(dir: &Path, log: &Path) -> Machine {
        let meta: Meta = toml::from_str(&format!(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            get-status = "echo @JOB_ID@ >> {}; echo '@JOB_ID@ Q'"
            status-pattern = "^@JOB_ID@ "
            queued-pattern = " Q"
            running-pattern = "$^"
            holding-pattern = "$^"
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
        Machine { name: "fake".to_owned(), dir: dir.to_owned(), layer: Layer::System, meta }
    }

    /// `build list`'s whole point (mirrors `sim list`): a config whose latest
    /// attempt already recorded an outcome costs zero scheduler round-trips,
    /// and the rest are resolved in one batched call.
    #[test]
    fn list_queries_the_scheduler_only_for_unfinished_attempts() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let log = tmp.path().join("calls.log");
        let inst = Installation::new("et", tmp.path().join("inst"));

        // Finished: outcome recorded already, no query needed.
        make_attempt(
            &cactus.join("configs/built"),
            &cactus,
            "built",
            0,
            "JOB-OLD",
            true,
            Some(BuildOutcomeRecord { exit_status: Some(0), complete: true }),
        );
        // Still queued: no outcome yet, a real job id to ask about.
        make_attempt(&cactus.join("configs/queued"), &cactus, "queued", 0, "42", true, None);

        let machine = logging_status_machine(&tmp.path().join("mdb/fake"), &log);
        list_impl(&machine, &[inst], false, false).unwrap();

        let calls: Vec<String> =
            fs::read_to_string(&log).unwrap_or_default().lines().map(str::to_owned).collect();
        assert_eq!(calls, vec!["42"], "only the unfinished attempt's job id is queried");
    }

    /// A machine whose `get-status` logs each job asked about and lists it
    /// queued until its `stop` has run; `job-ids` as `job_ids` says.
    fn requeue_machine(dir: &Path, log: &Path, gone: &Path, job_ids: &str) -> Machine {
        let meta: Meta = toml::from_str(&format!(
            r#"
            [machine]
            nickname = "fake"
            [scheduler]
            job-ids = "{job_ids}"
            get-status = "echo @JOB_ID@ >> {log}; test -e {gone} || echo '@JOB_ID@ PD'"
            stop = "touch {gone}"
            status-pattern = "^@JOB_ID@ "
            queued-pattern = " PD"
            [queues.local]
            default = true
            [submitscripts]
            "default" = ["local"]
            [runscripts]
            "default" = ["local"]
            "#,
            log = log.display(),
            gone = gone.display()
        ))
        .unwrap();
        Machine { name: "fake".to_owned(), dir: dir.to_owned(), layer: Layer::System, meta }
    }

    /// A build job a batch scheduler requeued after its attempt recorded an
    /// outcome (preemption, `scontrol requeue`) is live to the guard before
    /// a new attempt and to `build stop`; `build list` keeps its finished
    /// history free of scheduler round-trips. Where job ids are pids, a
    /// recorded outcome is the end.
    #[test]
    fn a_requeued_build_is_live_to_the_guard_and_to_stop() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        let done = BuildOutcomeRecord { exit_status: Some(0), complete: true };
        make_attempt(&config_dir, &cactus, "sim", 0, "4242", true, Some(done));
        let inst = Installation::new("et", tmp.path().join("inst"));
        let (log, gone) = (tmp.path().join("calls.log"), tmp.path().join("gone"));

        let pids = requeue_machine(&tmp.path().join("mdb/fake"), &log, &gone, "pids");
        guard_no_live_attempt(&config_dir, "sim", &Scheduler::new(&pids.meta), false).unwrap();
        stop_impl(&pids, &inst, Some("sim"), false).unwrap();
        assert!(!log.exists() && !gone.exists(), "a recorded outcome is the end where job ids are pids");

        let batch = requeue_machine(&tmp.path().join("mdb/fake"), &log, &gone, "scheduler");
        list_impl(&batch, std::slice::from_ref(&inst), false, false).unwrap();
        assert!(!log.exists(), "build list asked about a finished attempt");
        let sched = Scheduler::new(&batch.meta);
        let refused = guard_no_live_attempt(&config_dir, "sim", &sched, false).unwrap_err().to_string();
        assert!(refused.contains("still lists its job 4242") && refused.contains("build stop sim"), "{refused}");
        stop_impl(&batch, &inst, Some("sim"), false).unwrap();
        assert!(gone.exists(), "build stop missed the requeued job");
        guard_no_live_attempt(&config_dir, "sim", &sched, false).unwrap();
        assert!(BuildAttempt::open(&config_dir, 0).unwrap().meta.outcome.is_some_and(|o| o.complete));
    }

    /// After an attempt's outcome, a job the scheduler still lists in any
    /// state is live (leaving the queue, or starting again, which cactup
    /// cannot tell apart); a status query that fails is taken as the job
    /// ended (a scheduler that cannot be asked never blocks `build -f` of a
    /// config whose last build finished). Before the outcome, a failed query
    /// blocks, and `list` shows UNKNOWN.
    #[test]
    fn after_its_outcome_a_build_job_still_listed_in_any_state_is_live() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        let done = BuildOutcomeRecord { exit_status: Some(0), complete: true };
        make_attempt(&config_dir, &cactus, "sim", 0, "4242", true, Some(done));
        let inst = Installation::new("et", tmp.path().join("inst"));
        let (state, stopped) = (tmp.path().join("state"), tmp.path().join("stopped"));
        // While it exists, the stop command fails, printing what it holds:
        // a stop refused as a job the scheduler does not know (as Torque's
        // `qdel` refuses one; Slurm's `scancel` takes it silently), or one
        // failing to reach the controller.
        let refuse = tmp.path().join("refuse");
        let forgotten = "scancel: error: Kill job error on job id 4242: Invalid job id specified";
        let transient = "scancel: error: Unable to contact slurm controller (connect failure)";
        let machine = |get_status: &str| {
            let meta: Meta = toml::from_str(&format!(
                r#"
                [machine]
                nickname = "fake"
                [scheduler]
                get-status = "{get_status}"
                unknown-job-pattern = 'Invalid job id specified'
                stop = "touch {stopped}; test ! -e {refuse} || {{ cat {refuse} >&2; exit 1; }}"
                status-pattern = "^@JOB_ID@ "
                queued-pattern = " PD"
                running-pattern = " (CF|CG|R|TO)"
                [queues.local]
                default = true
                [submitscripts]
                "default" = ["local"]
                [runscripts]
                "default" = ["local"]
                "#,
                stopped = stopped.display(),
                refuse = refuse.display()
            ))
            .unwrap();
            Machine { name: "fake".to_owned(), dir: tmp.path().join("mdb/fake"), layer: Layer::System, meta }
        };
        let listing = machine(&format!("echo \\\"@JOB_ID@ $(cat {})\\\"", state.display()));
        let sched = Scheduler::new(&listing.meta);
        // Still listed in any state after the outcome (leaving the queue,
        // or starting again: `CF`, `R` before the requeued job's run clears
        // the outcome): a new build waits (or `-f` stops it), and `build
        // stop` asks the scheduler to stop it, the outcome standing.
        for letter in ["CF", "R", "CG", "TO"] {
            fs::write(&state, letter).unwrap();
            let refused = guard_no_live_attempt(&config_dir, "sim", &sched, false).unwrap_err().to_string();
            assert!(refused.contains("still lists its job 4242"), "{letter}: {refused}");
            stop_impl(&listing, &inst, Some("sim"), false).unwrap();
            assert!(stopped.exists(), "{letter}: build stop left a still-listed job");
            fs::remove_file(&stopped).unwrap();
            assert!(BuildAttempt::open(&config_dir, 0).unwrap().meta.outcome.is_some_and(|o| o.complete));
            guard_no_live_attempt(&config_dir, "sim", &sched, true).unwrap();
            assert!(stopped.exists(), "{letter}: -f left a still-listed job");
            fs::remove_file(&stopped).unwrap();
        }
        // `CD`, a state no pattern reads after the outcome: the ended job;
        // no refusal, and nothing stopped.
        fs::write(&state, "CD").unwrap();
        guard_no_live_attempt(&config_dir, "sim", &sched, false).unwrap();
        stop_impl(&listing, &inst, Some("sim"), false).unwrap();
        assert!(!stopped.exists(), "CD: stopped the ended job");
        // A failed query after the outcome: no answer. A new attempt is
        // not started beside a job that may have been queued again (the
        // guard refuses, saying so; `-f` stops it); `build stop` sends the
        // stop, harmless if the job is gone.
        let failing = machine("echo 'slurm_load_jobs error: Socket timed out on send/recv operation' >&2; exit 1");
        let failing_sched = Scheduler::new(&failing.meta);
        let refused = guard_no_live_attempt(&config_dir, "sim", &failing_sched, false).unwrap_err().to_string();
        assert!(refused.contains("could not be asked whether its job 4242 was queued again"), "{refused}");
        // The failed query's own text, and the way out that always works.
        assert!(refused.contains("Socket timed out") && refused.contains("unknown-job-pattern"), "{refused}");
        assert!(refused.contains("`cactup build stop sim`"), "{refused}");
        guard_no_live_attempt(&config_dir, "sim", &failing_sched, true).unwrap();
        assert!(stopped.exists(), "-f sent no stop for an unanswered job");
        fs::remove_file(&stopped).unwrap();
        let unsettle = || {
            let mut a = BuildAttempt::open(&config_dir, 0).unwrap();
            a.meta.timestamps.job_stopped = None;
            a.store_meta().unwrap();
        };
        // `build stop` sends the stop and settles the job: recorded, so the
        // guard then lets a new build go, asking nothing.
        stop_impl(&failing, &inst, Some("sim"), false).unwrap();
        assert!(stopped.exists(), "build stop sent no stop for an unanswered job");
        fs::remove_file(&stopped).unwrap();
        assert!(BuildAttempt::open(&config_dir, 0).unwrap().meta.timestamps.job_stopped.is_some());
        guard_no_live_attempt(&config_dir, "sim", &failing_sched, false).unwrap();
        stop_impl(&failing, &inst, Some("sim"), false).unwrap();
        assert!(!stopped.exists(), "a settled job was stopped again");
        unsettle();
        let settled = || BuildAttempt::open(&config_dir, 0).unwrap().meta.timestamps.job_stopped.is_some();
        // After the failed query, the stop refused as a job the scheduler
        // does not know (its output matches `unknown-job-pattern`): taken as
        // ended, settled; `-f` and `build stop` succeed on it, and the guard
        // then lets a new build go.
        fs::write(&refuse, forgotten).unwrap();
        guard_no_live_attempt(&config_dir, "sim", &failing_sched, true).unwrap();
        stop_impl(&failing, &inst, Some("sim"), false).unwrap();
        assert!(settled());
        guard_no_live_attempt(&config_dir, "sim", &failing_sched, false).unwrap();
        unsettle();
        // A stop that fails otherwise (the controller not reached) settles
        // nothing: an error for `-f` and `build stop`, nothing recorded, and
        // the guard refuses still.
        fs::write(&refuse, transient).unwrap();
        let err = guard_no_live_attempt(&config_dir, "sim", &failing_sched, true).unwrap_err().to_string();
        assert!(err.contains("may not have been reached") && err.contains("Unable to contact"), "{err}");
        assert!(stop_impl(&failing, &inst, Some("sim"), false).is_err());
        assert!(!settled());
        assert!(guard_no_live_attempt(&config_dir, "sim", &failing_sched, false).is_err());
        // Just listed, and any stop failure (a refusal too): an error,
        // nothing settled.
        fs::write(&state, "R").unwrap();
        for said in [transient, forgotten] {
            fs::write(&refuse, said).unwrap();
            let err = guard_no_live_attempt(&config_dir, "sim", &sched, true).unwrap_err().to_string();
            assert!(err.contains("the stop command for job 4242 failed"), "{err}");
            assert!(stop_impl(&listing, &inst, Some("sim"), false).is_err());
            assert!(!settled());
        }
        fs::remove_file(&refuse).unwrap();
        fs::remove_file(&stopped).unwrap();

        // No outcome yet: the failed query is the guard's refusal, and the
        // display shows the state unknown.
        let mut open = BuildAttempt::open(&config_dir, 0).unwrap();
        open.meta.outcome = None;
        open.store_meta().unwrap();
        let err = format!("{:#}", guard_no_live_attempt(&config_dir, "sim", &failing_sched, true).unwrap_err());
        assert!(err.contains("the status query for job 4242"), "{err}");
        assert_eq!(shown_state(&open.meta, Some(None), false), BuildDisplayState::Unknown);
        assert_eq!(shown_state(&open.meta, Some(None), true), BuildDisplayState::Running);
        assert!(attempt_is_live(&open, &failing_sched).unwrap(), "prune removed an attempt whose query failed");
    }

    /// Where job ids are pids, a submitted attempt's job recorded on another
    /// host is never taken for gone while the attempt's lock is live: `build
    /// stop` says where it runs and records nothing. With the lock let go it
    /// is taken as ended.
    #[test]
    fn build_stop_leaves_a_job_on_another_host_while_its_lock_is_live() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        let mut attempt = make_attempt(&config_dir, &cactus, "sim", 0, "4242", true, None);
        attempt.meta.job_host = Some("some-other-host".to_owned());
        attempt.meta.job_start = Some(7);
        attempt.store_meta().unwrap();
        let inst = Installation::new("et", tmp.path().join("inst"));
        let log = tmp.path().join("calls.log");
        let mut machine = logging_status_machine(&tmp.path().join("mdb/fake"), &log);
        machine.meta.scheduler.job_ids = crate::mdb::meta::JobIds::Pids;
        let held = LinkLock::acquire(&attempt.running_lock_path()).unwrap();
        stop_impl(&machine, &inst, Some("sim"), false).unwrap();
        assert!(BuildAttempt::open(&config_dir, 0).unwrap().meta.outcome.is_none(), "recorded an end of a job running elsewhere");
        drop(held);
        stop_impl(&machine, &inst, Some("sim"), false).unwrap();
        assert!(BuildAttempt::open(&config_dir, 0).unwrap().meta.outcome.is_some());
        assert!(!log.exists(), "another host's pid was asked about");
    }

    /// An attempt's job is asked about after its outcome only within the
    /// requeue window, and only for an attempt made for this machine (a
    /// home two clusters share): fifteen days on, or another machine's
    /// attempt, the outcome is the end.
    #[test]
    fn an_attempts_job_is_asked_about_after_its_outcome_only_within_the_requeue_window() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let done = BuildOutcomeRecord { exit_status: Some(0), complete: true };
        let mut meta = attempt_meta(&cactus.join("configs/sim"), &cactus, "sim", 0, "4242", true, Some(done));
        let log = tmp.path().join("calls.log");
        let machine = logging_status_machine(&tmp.path().join("mdb/fake"), &log);
        let sched = Scheduler::new(&machine.meta);
        assert!(requeue_query(&meta, &sched).is_some());
        assert!(requeue_query(&meta, &Scheduler::named(&machine.meta, &meta.machine)).is_some());
        assert!(requeue_query(&meta, &Scheduler::named(&machine.meta, "another-machine")).is_none());
        meta.timestamps.finished = Some(Utc::now() - chrono::Duration::days(15));
        assert!(requeue_query(&meta, &sched).is_none());
    }

    /// The compute side writes its record keeping the job the submitter
    /// recorded, even from a copy read before that (a job that started at
    /// once).
    #[test]
    fn the_compute_side_keeps_the_job_the_submitter_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        let mut early = make_attempt(&config_dir, &cactus, "sim", 0, NO_JOB_ID, false, None);
        let mut submitter = BuildAttempt::open(&config_dir, 0).unwrap();
        submitter.meta.set_job(&crate::scheduler::JobRecord {
            id: "77".to_owned(),
            host: Some("h".to_owned()),
            start: Some(9),
            since: None,
        });
        submitter.meta.submitted = true;
        submitter.meta.timestamps.submitted = Some(Utc::now());
        submitter.store_meta().unwrap();
        early.meta.status = Some("R".to_owned());
        early.store_meta_keeping_job().unwrap();
        let after = BuildAttempt::open(&config_dir, 0).unwrap();
        assert_eq!((after.meta.job_id.as_str(), after.meta.job_host.as_deref(), after.meta.job_start), ("77", Some("h"), Some(9)));
        assert!(after.meta.submitted && after.meta.timestamps.submitted.is_some());
        assert_eq!(after.meta.status.as_deref(), Some("R"));
    }

    /// A foreground attempt's `job_id` is this process's own pid, never a
    /// real scheduler id — `stop` must never hand it to the scheduler
    /// (mirrors the bug `testsuite::manage::stop` has and this must not).
    #[test]
    fn stop_on_a_foreground_attempt_never_touches_the_scheduler() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        make_attempt(&config_dir, &cactus, "sim", 0, "12345", false, None);
        let inst = Installation::new("et", tmp.path().join("inst"));

        let log = tmp.path().join("calls.log");
        let machine = logging_status_machine(&tmp.path().join("mdb/fake"), &log);

        stop_impl(&machine, &inst, Some("sim"), false).unwrap();

        assert!(!log.exists(), "a foreground build's pid must never reach the scheduler");
        let attempt = BuildAttempt::open(&config_dir, 0).unwrap();
        let outcome = attempt.meta.outcome.expect("stop records an outcome");
        assert!(!outcome.complete);
    }

    /// A submitted (queued/running) attempt's job id IS a real scheduler id,
    /// so `stop` must go through the scheduler's `stop` command for it.
    #[test]
    fn stop_on_a_submitted_attempt_calls_the_scheduler() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        make_attempt(&config_dir, &cactus, "sim", 0, "JOB-1", true, None);
        let inst = Installation::new("et", tmp.path().join("inst"));
        let machine = fake_submit_machine(&tmp.path().join("mdb/fake"), "", "", true);

        stop_impl(&machine, &inst, Some("sim"), false).unwrap();

        let attempt = BuildAttempt::open(&config_dir, 0).unwrap();
        let outcome = attempt.meta.outcome.expect("stop records an outcome");
        assert!(!outcome.complete);
    }

    /// `build prune` keeps the newest N, removes the rest, and skips (rather
    /// than removes) a still-live attempt even when it falls outside the
    /// keep window.
    #[test]
    fn prune_keeps_n_removes_rest_and_skips_a_live_attempt() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        let inst = Installation::new("et", tmp.path().join("inst"));

        // Attempt 0: no outcome, running.lock held live — must survive even
        // though it is the oldest and outside the keep window.
        let live = make_attempt(&config_dir, &cactus, "sim", 0, NO_JOB_ID, false, None);
        let _held = crate::lock::LinkLock::acquire(&live.running_lock_path()).unwrap();
        // Attempts 1..=5: finished, removable.
        for id in 1..6 {
            make_attempt(
                &config_dir,
                &cactus,
                "sim",
                id,
                NO_JOB_ID,
                false,
                Some(BuildOutcomeRecord { exit_status: Some(0), complete: true }),
            );
        }

        let machine = fake_submit_machine(&tmp.path().join("mdb/fake"), "", "", false);
        prune_impl(&machine, &inst, Some("sim"), Some(2)).unwrap();

        // 0 survives (live, skipped); 1..=3 pruned (oldest of the removable
        // ones, since only the newest 2 are kept); 4, 5 survive untouched.
        assert_eq!(BuildAttempt::scan(&config_dir).unwrap(), vec![0, 4, 5]);
    }

    /// `build log` prefers the frozen `STDOUT_FILE`/`STDERR_FILE` vars over
    /// the attempt's own conventional paths.
    #[test]
    fn build_log_sources_prefers_frozen_var_paths() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        let mut attempt = make_attempt(&config_dir, &cactus, "sim", 0, NO_JOB_ID, false, None);

        let sources = build_log_sources(&attempt);
        assert_eq!(sources, [("stdout", attempt.out_path()), ("stderr", attempt.err_path())]);

        let custom_out = tmp.path().join("elsewhere.out");
        let custom_err = tmp.path().join("elsewhere.err");
        attempt
            .meta
            .vars
            .insert("STDOUT_FILE".to_owned(), toml::Value::String(custom_out.display().to_string()));
        attempt
            .meta
            .vars
            .insert("STDERR_FILE".to_owned(), toml::Value::String(custom_err.display().to_string()));

        let sources = build_log_sources(&attempt);
        assert_eq!(sources, [("stdout", custom_out), ("stderr", custom_err)]);
    }

    // ---- in_flight_build ----------------------------------------------------

    /// No attempts at all — the ordinary "genuinely never built" case — must
    /// yield `None`, with or without a machine to query.
    #[test]
    fn in_flight_build_none_when_no_attempts_exist() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("configs/sim");
        assert!(in_flight_build(&config_dir, "sim", None).is_none());

        let machine = fake_submit_machine(&tmp.path().join("mdb/fake"), "", "", false);
        assert!(in_flight_build(&config_dir, "sim", Some(&machine)).is_none());
    }

    /// A finished attempt (outcome recorded, success or failure) is done —
    /// whatever the queue would say now — so it is not "in flight".
    #[test]
    fn in_flight_build_none_once_an_outcome_is_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        make_attempt(
            &config_dir,
            &cactus,
            "sim",
            0,
            "JOB-1",
            true,
            Some(BuildOutcomeRecord { exit_status: Some(0), complete: true }),
        );
        assert!(in_flight_build(&config_dir, "sim", None).is_none());
    }

    /// Unreadable/corrupt attempt metadata must yield `None`, never an error
    /// — this helper's whole point is to be safe to call from anywhere.
    #[test]
    fn in_flight_build_none_on_unreadable_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let config_dir = tmp.path().join("configs/sim");
        let attempt_dir = BuildAttempt::attempt_dir(&config_dir, 0);
        fs::create_dir_all(&attempt_dir).unwrap();
        fs::write(BuildAttempt::meta_path(&attempt_dir), b"not valid toml{{{").unwrap();
        assert!(in_flight_build(&config_dir, "sim", None).is_none());
    }

    /// A submitted attempt with no recorded outcome and a real job id is
    /// in flight — the metadata-only (no machine) path a per-row `config
    /// list` relies on, per the attempt's own record alone.
    #[test]
    fn in_flight_build_submitted_no_outcome_without_a_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        make_attempt(&config_dir, &cactus, "sim", 0, "JOB-1", true, None);

        let phrase = in_flight_build(&config_dir, "sim", None).expect("submitted, no outcome yet");
        assert!(phrase.contains("sim"), "{phrase}");
        assert!(phrase.contains("JOB-1"), "{phrase}");
    }

    /// The same attempt, but with a machine handy: a live scheduler query
    /// disambiguates the state word (QUEUED here, via `always_queued`).
    #[test]
    fn in_flight_build_submitted_queries_the_scheduler_when_given_a_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        make_attempt(&config_dir, &cactus, "sim", 0, "JOB-1", true, None);

        let machine = fake_submit_machine(&tmp.path().join("mdb/fake"), "", "", true);
        let phrase =
            in_flight_build(&config_dir, "sim", Some(&machine)).expect("queued per the fake scheduler");
        assert!(phrase.contains("QUEUED"), "{phrase}");
        assert!(phrase.contains("JOB-1"), "{phrase}");
    }

    /// A foreground build's `job_id` is a pid, not a scheduler id —
    /// `running.lock` held live is the only signal, and it needs no machine.
    #[test]
    fn in_flight_build_foreground_running_needs_no_machine() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        let attempt = make_attempt(&config_dir, &cactus, "sim", 0, "12345", false, None);
        let _held = crate::lock::LinkLock::acquire(&attempt.running_lock_path()).unwrap();

        let phrase = in_flight_build(&config_dir, "sim", None).expect("lock held live");
        assert!(phrase.contains("foreground"), "{phrase}");
        assert!(phrase.contains("12345"), "{phrase}");
    }

    /// A foreground attempt with no held lock and no outcome died without
    /// reporting back — there is no way to tell that from a dead one without
    /// a machine, so this must not be reported as in flight.
    #[test]
    fn in_flight_build_none_for_a_foreground_attempt_with_no_live_lock() {
        let tmp = tempfile::tempdir().unwrap();
        let cactus = tmp.path().join("inst/Cactus");
        let config_dir = cactus.join("configs/sim");
        make_attempt(&config_dir, &cactus, "sim", 0, "12345", false, None);
        assert!(in_flight_build(&config_dir, "sim", None).is_none());
    }
}
