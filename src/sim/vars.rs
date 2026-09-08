//! TOPOLOGY → process layout (§8.5) and assembly of the canonical §6.3
//! variable set for a restart.

use crate::args::TopologyFlags;
use crate::build::ConfigMeta;
use crate::database::Database;
use crate::mdb::{Hardware, Machine, Phase, HOST_UNIVERSE};
use crate::sim::{restart, Simulation};
use crate::template::VarSet;
use crate::walltime::Walltime;
use crate::Res;
use anyhow::{anyhow, bail};
use colored::Colorize;

/// The resolved §8.5 topology for one submit/run invocation.
#[derive(Debug, Clone)]
pub struct Topology {
    pub nodes: u32,
    pub tasks: u32,
    pub tpn: u32,
    pub cpus: u32,
    pub gpu: bool,
    /// GPUs per task (§8.5); always 0 on a non-GPU run.
    pub gpus_per_task: u32,
    pub queue: String,
    /// The scheduler-facing queue name (§4.2): the queue's `name` override
    /// when set, else `queue` itself. `@QUEUE@` resolves to this; everything
    /// cactup-internal (variant selection, walltime ceilings, records) keys
    /// on `queue`.
    pub scheduler_queue: String,
    pub allocation: Option<String>,
    pub mail: Option<String>,
    pub mail_type: String,
    pub job_name: Option<String>,
    /// The TOTAL wall the user wants for the whole simulation (§8.5); split
    /// into per-job segments by chaining (§8.8).
    pub total_wall: Walltime,
    pub out: Option<String>,
    pub err: Option<String>,
}

/// Queue-compatibility facts (D12 / §4.4): the four fields `resolve_topology`
/// actually reads off a build. Available from a built config's `ConfigMeta`
/// via `from_config`; a caller with no `ConfigMeta` yet (not yet built) can
/// still build one directly from these public fields — but should prefer
/// constructing a `ConfigMeta` first and going through `from_config`, so
/// there is exactly one place these facts are derived from an optionlist
/// header and a stale `--variant` can't disagree with the fresh one.
pub struct QueueFit {
    pub universe: Option<String>,
    pub compatible_queues: Vec<String>,
    pub gpu: bool,
    /// Named in the queue-compatibility errors below.
    pub label: String,
}

impl QueueFit {
    pub fn from_config(cfg: &ConfigMeta) -> Self {
        QueueFit {
            universe: cfg.universe.clone(),
            compatible_queues: cfg.compatible_queues.clone(),
            gpu: cfg.gpu,
            label: cfg.name.clone(),
        }
    }
}

/// Resolve the §8.5 topology: flag → knob → machine defaults, with the
/// queue-compatibility and GPU cross-checks (§4.4 / D12).
pub fn resolve_topology(
    flags: &TopologyFlags,
    machine: &Machine,
    db: &Database,
    fit: &QueueFit,
    force_queue: bool,
) -> Res<Topology> {
    let m = &machine.name;

    // The config's build universe gates which queues (and script variants) are
    // reachable (§4.4); configs recording none run in the implicit host.
    let cfg_universe = fit.universe.as_deref().unwrap_or(HOST_UNIVERSE);

    // Queue: -q → knob → machine default queue (the default is chosen among the
    // queues compatible with the build universe).
    let queue = flags
        .queue
        .clone()
        .or_else(|| db.knob("queue").map(str::to_owned))
        .or_else(|| machine.meta.default_queue(cfg_universe).map(str::to_owned))
        .ok_or_else(|| {
            anyhow!("no queue given (-q), no `queue` knob set, and machine \"{m}\" has no default queue compatible with build universe \"{cfg_universe}\"")
        })?;
    let queue_def = machine.meta.queue(&queue)?;

    // Build-universe gate (§4.4): a queue restricted to other build universes
    // cannot serve this config. Structural like variant compatibility — no
    // --force-queue escape (unlike the compatible-queues/GPU guards below).
    if !queue_def.compatible_with(cfg_universe) {
        bail!(
            "queue \"{queue}\" is not compatible with build universe \"{cfg_universe}\" \
             (build-universes = [{}])",
            queue_def.build_universes.as_deref().unwrap_or_default().join(", ")
        );
    }

    // compatible-queues guard (§4.4 / D12).
    if !fit.compatible_queues.is_empty() && !fit.compatible_queues.contains(&queue) && !force_queue {
        bail!(
            "config \"{}\" is only compatible with queue(s) {} (not \"{queue}\"); \
             use --force-queue to override",
            fit.label,
            fit.compatible_queues.join(", ")
        );
    }

    // GPU: -g or the queue's gpu flag, cross-checked one-directionally
    // against the binary (D12): only a GPU config in a non-GPU context is
    // refused. A non-GPU config on a GPU queue is allowed — some machines'
    // only real partition is GPU-flagged — with an advisory when a non-GPU
    // queue existed.
    let gpu = flags.gpu || queue_def.gpu;
    if fit.gpu && !gpu && !force_queue {
        // §8.5
        bail!(
            "config \"{}\" was built with GPU support but this run is non-GPU (queue \"{queue}\"); \
             use --force-queue to override",
            fit.label,
        );
    }
    if !fit.gpu && gpu && machine.meta.queues.values().any(|q| !q.gpu) {
        eprintln!(
            "{} config \"{}\" was built without GPU support but queue \"{queue}\" is GPU-flagged; \
             this machine also has non-GPU queue(s) (D12)",
            "note:".yellow(),
            fit.label,
        );
    }
    // `--gpus-per-task` is meaningful only once GPU is on: refused rather than
    // silently ignored, so a user who asked for GPUs per task and landed on a
    // CPU queue finds out here instead of from the scheduler.
    if flags.gpus_per_task.is_some() && !gpu {
        // §8.5
        bail!(
            "--gpus-per-task is only meaningful on a GPU run, but queue \"{queue}\" is not \
             GPU-flagged and --gpu was not given"
        );
    }

    // Process layout (§8.5 derivation), from the queue-effective hardware
    // (per-queue overrides falling back to [hardware] — §4.2).
    let hw = machine.meta.effective_hardware(&queue)?;
    let cpus_per_node = hw.max_cpus_per_node.unwrap_or(1);
    // Request-side CPUS_PER_TASK: -c wins, else the machine/queue
    // `default-cpus-per-task` (simfactory's num-threads), else 1.
    let cpus = flags.cpus.or(hw.default_cpus_per_task).unwrap_or(1).max(1);
    // GPUS_PER_TASK is settled before the rank count because a derived rank
    // count is bounded by it (below); it needs no layout of its own (§8.5).
    let gpus_per_task = derive_gpus_per_task(flags, &hw, gpu);
    // Fill the node — but a fully *derived* TASKS_PER_NODE is also bounded by
    // the node's GPU budget (§8.5): with GPUs indivisible and one per rank, a
    // node holding fewer devices than the CPU split implies cannot run that
    // many ranks, and the ceiling below would refuse the layout outright.
    // Bounding it here is what lets a GPU machine declare a polite
    // `default-cpus-per-task` (a rank wanting 8 of omnia's 72 cores) without
    // the CPU rule inferring 9 ranks for its single MI210. GPUs still never
    // *build* a layout: an explicit `--tpn` is authoritative, and an explicit
    // `--tasks` is a requested rank count — on a single-node machine with no
    // batch system every one of those ranks lands on the node, so shrinking
    // tpn under it would only bless a layout the node cannot run. Both keep
    // the un-bounded CPU-rule value and reach the ceiling check instead.
    let mut tpn = flags.tpn.unwrap_or_else(|| {
        let by_cpu = (cpus_per_node / cpus).max(1);
        match hw.max_gpus_per_node {
            Some(max) if gpu && flags.tasks.is_none() => by_cpu.min((max / gpus_per_task).max(1)),
            _ => by_cpu,
        }
    });
    let nodes = flags.nodes.unwrap_or(1);
    let tasks = flags.tasks.unwrap_or(nodes * tpn);
    // An explicit `--tasks` overrides the fill-the-node `TASKS`, so the derived
    // fill-the-node `TASKS_PER_NODE` has to be capped to keep the layout
    // self-consistent (`--tasks=1` must not report 2 tasks/node) — the same
    // capping `apply_tasks_default` does for the script-variant default. A
    // user-given `--tpn` stays authoritative. No-op in the default case, where
    // `tasks == nodes * tpn`.
    if flags.tpn.is_none() {
        tpn = tpn.min(tasks.div_ceil(nodes).max(1));
    }
    check_gpus_per_node(gpus_per_task, tpn, &hw, &queue)?;

    let total_wall = match flags.wall_time {
        Some(w) => w,
        None => machine.meta.effective_max_walltime(&queue)?,
    };

    Ok(Topology {
        nodes,
        tasks,
        tpn,
        cpus,
        gpu,
        gpus_per_task,
        scheduler_queue: machine.meta.scheduler_queue_name(&queue)?.to_owned(),
        queue,
        allocation: flags.allocation.clone().or_else(|| db.knob("allocation").map(str::to_owned)),
        mail: flags.mail.clone().or_else(|| db.knob("mail").map(str::to_owned)),
        mail_type: flags
            .mail_type
            .clone()
            .or_else(|| db.knob_or_default("mail-type"))
            .unwrap_or_else(|| "all".to_owned()),
        job_name: flags.job_name.clone(),
        total_wall,
        out: flags.out.clone(),
        err: flags.err.clone(),
    })
}

/// `GPUS_PER_TASK` (§8.5): `--gpus-per-task` → the queue-effective
/// `default-gpus-per-task` → **1**. Always 0 on a non-GPU run (the
/// explicit-flag-without-GPU case is refused in `resolve_topology`).
///
/// One GPU per rank is the default everywhere, deliberately unlike the CPU
/// chain's fill-the-node rule. A GPU is not divisible the way a core is: the
/// overwhelmingly common shape is one device per rank, and a machine that wants
/// otherwise says so with `default-gpus-per-task`. Deriving it from the node's
/// GPU count instead would silently hand extra devices to a job that shrank its
/// rank count for unrelated reasons, and would fight any machine whose
/// scheduler reserves GPUs on a different axis than it binds them.
///
/// `max-gpus-per-node` therefore does not feed this at all — it is a ceiling
/// (`check_gpus_per_node`) and a bound on a *derived* rank count
/// (`resolve_topology`), never a source of GPUs per task. The ceiling is the one
/// place the GPU chain is stricter than the CPU chain: CPUs oversubscribe
/// harmlessly (threads time-share a core), while a job asking for GPUs a
/// partition does not have either never schedules or lands with ranks fighting
/// over one device.
fn derive_gpus_per_task(flags: &TopologyFlags, hw: &Hardware, gpu: bool) -> u32 {
    if !gpu {
        return 0;
    }
    flags.gpus_per_task.or(hw.default_gpus_per_task).unwrap_or(1).max(1)
}

/// The GPUs-per-node ceiling (§8.5): `GPUS_PER_TASK × TASKS_PER_NODE` may not
/// exceed `max-gpus-per-node`. Checked once the whole layout is settled, and
/// only for a GPU run (`per_task` is 0 otherwise, so the product is 0).
///
/// A fully *derived* `TASKS_PER_NODE` (no `--tpn`, no `--tasks`) is already
/// bounded by the GPU budget in `resolve_topology`, so what reaches here and
/// fails is an explicitly requested layout: `--tpn`/`--tasks` asking for more
/// ranks than the node has devices for, or a
/// `--gpus-per-task`/`default-gpus-per-task` above the node's count.
fn check_gpus_per_node(per_task: u32, tpn: u32, hw: &Hardware, queue: &str) -> Res<()> {
    if per_task == 0 {
        return Ok(());
    }
    let tpn = tpn.max(1);
    if let Some(max) = hw.max_gpus_per_node {
        let needed = per_task * tpn;
        if needed > max {
            // Point at whichever knob can actually help: telling someone whose
            // single rank already over-asks to lower the rank count is noise,
            // and at 1 GPU per rank there is no per-task knob left to lower.
            let fix = if max == 0 {
                format!("queue \"{queue}\" has no GPUs at all; run without GPUs or pick another queue")
            } else if per_task > max {
                // No rank count helps — one rank already exceeds the node.
                format!("--gpus-per-task {max} or lower fits this layout")
            } else if per_task > 1 && max / tpn >= 1 {
                format!("--gpus-per-task {} or lower fits this layout", max / tpn)
            } else if per_task > 1 {
                // Lowering GPUs per task cannot save this rank count: even 1
                // per rank overshoots, so the rank count is what has to move.
                format!("lower --tpn/--tasks to put at most {} ranks on a node", max / per_task)
            } else {
                format!("each rank already takes one GPU, so lower --tpn/--tasks to put at most {max} on a node")
            };
            // §8.5
            bail!(
                "this layout needs {needed} GPUs per node ({per_task} per task × {tpn} tasks/node) \
                 but queue \"{queue}\" has {max}; {fix}"
            );
        }
    }
    Ok(())
}

/// Apply the default-tasks chain to a resolved topology: the selected script
/// variant's `tasks` setting (§4.2) first, then the caller's fallback (2 for
/// testsuite runs — §11.6), else keep the §8.5 fill-the-node value. Any
/// explicit process-layout flag (-n/-T/-t) disables the whole chain; tpn is
/// capped so the recorded layout stays self-consistent.
///
/// `GPUS_PER_TASK` needs no revisiting here: it is per-*task*, so shrinking the
/// rank count leaves it untouched, and `tpn` only ever shrinks below — which
/// can only relax the §8.5 GPUs-per-node ceiling, never breach it.
pub fn apply_tasks_default(
    topo: &mut Topology,
    flags: &TopologyFlags,
    script_tasks: Option<u32>,
    fallback: Option<u32>,
) {
    if flags.nodes.is_some() || flags.tasks.is_some() || flags.tpn.is_some() {
        return;
    }
    if let Some(t) = script_tasks.or(fallback) {
        let t = t.max(1);
        topo.tasks = t;
        topo.tpn = topo.tpn.min(t);
    }
}

/// Automatic walltime chaining math (§8.8): `(segments, per-job wall)`.
/// Each chained segment reserves the ceiling as its scheduler wall.
pub fn chain_segments(total: Walltime, ceiling: Walltime) -> (u32, Walltime) {
    if total.0 <= ceiling.0 || ceiling.0 == 0 {
        (1, total)
    } else {
        (total.0.div_ceil(ceiling.0) as u32, ceiling)
    }
}

/// The default checkpoint buffer: `max(reserved-walltime / 24, 10 minutes)`
/// (§8.8); the hint variable is `hard wall − buffer`.
pub fn default_checkpt_buffer(job_wall: Walltime) -> Walltime {
    Walltime((job_wall.0 / 24).max(600))
}

/// `@SHORT_SIMULATION_NAME@` (§9.1): printable-only, whitespace→`_`, ensure a
/// leading letter, truncate to 15 chars. Default input `<SimName>-<RestartID>`.
pub fn short_sim_name(name: &str, restart_id: u32) -> String {
    let raw = format!("{name}-{restart_id}");
    let mut out = String::new();
    for c in raw.chars() {
        if c.is_whitespace() {
            out.push('_');
        } else if c.is_ascii_graphic() {
            out.push(c);
        }
    }
    if !out.chars().next().map(|c| c.is_ascii_alphabetic()).unwrap_or(false) {
        out.insert(0, 'J');
    }
    out.truncate(15);
    out
}

/// Everything needed to assemble one restart's §6.3 variable set.
pub struct RestartVarsInput<'a> {
    pub sim: &'a Simulation,
    pub machine: &'a Machine,
    pub topo: &'a Topology,
    /// The per-alias simulation home (§8.1) — NOT derivable from `sim.dir`,
    /// which may be a custom `--sim-dir`.
    pub sim_home: &'a std::path::Path,
    pub restart_id: u32,
    /// The hard scheduler wall for THIS job (post-chaining, §8.8).
    pub job_wall: Walltime,
    pub checkpt_buffer: Walltime,
    /// The job id this job depends on ("" = unchained).
    pub chained_job_id: &'a str,
    pub hostname: &'a str,
    pub user: &'a str,
    pub email: &'a str,
    /// The resolved RUN universe name: its env keys override the machine's
    /// for the run-phase `ENV_SETUP` (§6.1); `None` = bare / implicit host.
    pub run_universe: Option<&'a str>,
    /// `sim run --debug` (§8.4): `@RUNDEBUG@` = 1.
    pub debug: bool,
}

/// The §8.5 topology block — one variable per flag. Shared by sim restarts
/// and test runs (§11.9 keeps `TASKS` etc. as the existing §6.3 names).
pub fn set_topology_vars(v: &mut VarSet, topo: &Topology, default_job_name: &str) {
    v.set("NODES", topo.nodes as u64);
    v.set("TASKS", topo.tasks as u64);
    v.set("TASKS_PER_NODE", topo.tpn as u64);
    v.set("CPUS_PER_TASK", topo.cpus as u64);
    v.set("GPU", topo.gpu);
    v.set("GPUS_PER_TASK", topo.gpus_per_task as u64);
    v.set("ALLOCATION", topo.allocation.as_deref().unwrap_or(""));
    v.set("QUEUE", topo.scheduler_queue.as_str());
    v.set("MAIL", topo.mail.as_deref().unwrap_or(""));
    v.set("MAIL_TYPE", topo.mail_type.as_str());
    v.set("JOB_NAME", topo.job_name.clone().unwrap_or_else(|| default_job_name.to_owned()));
}

/// The WALLTIME family alone (§6.3), with no checkpoint pair: a build never
/// checkpoints, so exposing CHECKPOINT_WALLTIME* to a buildsubmitscript would
/// be a name it could reference and never have made sense of — the same
/// no-leak rule the testsuite var set already follows for simulation-only
/// names. `set_walltime_vars` below is `set_wall_only_vars` plus that pair,
/// for the two subsystems (sim, testsuite) that do checkpoint.
pub fn set_wall_only_vars(v: &mut VarSet, wall: Walltime) {
    v.set("WALLTIME", wall.canonical());
    v.set("WALLTIME_HH", wall.component_hours());
    v.set("WALLTIME_MM", wall.component_minutes());
    v.set("WALLTIME_SS", wall.component_seconds());
    v.set("WALLTIME_SECONDS", wall.total_seconds());
    v.set("WALLTIME_MINUTES", wall.total_minutes());
    v.set("WALLTIME_HOURS", wall.total_hours());
}

/// The two walltimes (§6.3): hard wall + checkpoint hint = wall − buffer.
pub fn set_walltime_vars(v: &mut VarSet, wall: Walltime, buffer: Walltime) {
    set_wall_only_vars(v, wall);
    let hint = Walltime(wall.0.saturating_sub(buffer.0));
    v.set("CHECKPOINT_WALLTIME", hint.canonical());
    v.set("CHECKPOINT_WALLTIME_SECONDS", hint.total_seconds());
    v.set("CHECKPOINT_WALLTIME_HOURS", hint.total_hours());
}

/// The machine-derived block (§6.3): the queue-effective hardware facts
/// (per-queue overrides falling back to [hardware] — §4.2) + the RUN-phase
/// `ENV_SETUP` under the resolved run universe (§6.1; submit-phase artifacts
/// override it).
pub fn set_machine_vars(v: &mut VarSet, machine: &Machine, queue: &str, run_universe: Option<&str>) -> Res<()> {
    let hw = machine.meta.effective_hardware(queue)?;
    v.set("MAX_CPUS_PER_NODE", hw.max_cpus_per_node.unwrap_or(1) as u64);
    // 0, not 1, when undeclared: unlike CPUs (where every node has at least
    // one) an absent GPU count means "none or unknown", and a script reading
    // this must not mistake that for a real one-GPU node.
    v.set("MAX_GPUS_PER_NODE", hw.max_gpus_per_node.unwrap_or(0) as u64);
    v.set("MEMORY", hw.memory.unwrap_or(0));
    v.set("THREADS_PER_CPU", hw.threads_per_cpu() as u64);
    v.set("ENV_SETUP", machine.meta.effective_env(run_universe, Phase::Run));
    Ok(())
}

/// Assemble the canonical variable set (§6.3) for one restart. `ENV_SETUP` is
/// set to the RUN-phase effective block — callers substituting a
/// submit-phase artifact override it first (§6.1).
pub fn assemble(input: &RestartVarsInput) -> Res<VarSet> {
    let RestartVarsInput { sim, machine, topo, restart_id, .. } = input;
    let restart_dir = restart::restart_dir(&sim.dir, *restart_id);
    let mut v = VarSet::new();

    set_topology_vars(&mut v, topo, &sim.name);
    set_walltime_vars(&mut v, input.job_wall, input.checkpt_buffer);

    let out_default = || restart_dir.join(format!("{}.out", sim.name)).display().to_string();
    let err_default = || restart_dir.join(format!("{}.err", sim.name)).display().to_string();
    v.set("STDOUT_FILE", topo.out.clone().unwrap_or_else(out_default));
    v.set("STDERR_FILE", topo.err.clone().unwrap_or_else(err_default));

    // Identity / paths.
    v.set("SIMULATION_NAME", sim.name.as_str());
    v.set("SHORT_SIMULATION_NAME", short_sim_name(&sim.name, *restart_id));
    v.set("SIMULATION_ID", sim.meta.simulation_id.as_str());
    v.set("RESTART_ID", *restart_id as u64);
    v.set("RUNDIR", restart_dir.display().to_string());
    v.set("SOURCEDIR", sim.meta.sourcedir.display().to_string());
    v.set("EXECUTABLE", sim.exe().display().to_string());
    v.set(
        "PARFILE",
        restart_dir.join(format!("{}.par", sim.parfile_stem())).display().to_string(),
    );
    v.set(
        "SCRIPTFILE",
        restart_dir.join(".cactup").join("submit-script").display().to_string(),
    );
    v.set("CONFIGURATION", sim.meta.configuration.as_str());
    v.set("SIM_HOME", input.sim_home.display().to_string());
    v.set("SIMULATION_DIR", sim.dir.display().to_string());
    v.set(
        "SCRATCH_HOME",
        machine.meta.resolved_paths()?.scratch_home.unwrap_or_default(),
    );
    v.set("ALIAS", sim.meta.alias.as_str());
    let cactup = std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "cactup".to_owned());
    v.set("CACTUP", cactup);

    // Identity / machine.
    v.set("MACHINE", machine.name.as_str());
    v.set("HOSTNAME", input.hostname);
    v.set("USER", input.user);
    v.set("EMAIL", input.email);
    v.set("EXECHOST", "");
    v.set("JOB_ID", "");
    v.set("CHAINED_JOB_ID", input.chained_job_id);

    set_machine_vars(&mut v, machine, &topo.queue, input.run_universe)?;

    // Debug (§8.4).
    v.set("RUNDEBUG", input.debug);
    v.set("DEBUGGER", "gdb");

    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::template::Syntax;
    use crate::mdb::{Layer, Machine, Meta};
    use crate::sim::SimulationMeta;
    use std::path::PathBuf;

    fn test_machine() -> Machine {
        let meta: Meta = toml::from_str(
            r#"
            [machine]
            name = "testbox"

            [hardware]
            max-cpus-per-node = 16
            memory = 64000
            threads-per-cpu = 2

            [queues.batch]
            default = true
            max-walltime = "24:00:00"

            [queues.gpuq]
            gpu = true
            max-walltime = "12:00:00"
            # Real scheduler name override (§4.2): @QUEUE@ resolves to this.
            name = "gpu_part"
            # Per-queue hardware override (§4.2): the GPU nodes are fatter.
            max-cpus-per-node = 32

            [queues.fillq]
            max-walltime = "24:00:00"
            # Deep-Bayou-style node: 48 CPUs, 24 CPUs/task by default → the
            # no-`-c` fill resolves to 2 tasks/node × 24 CPUs (§8.5).
            max-cpus-per-node = 48
            default-cpus-per-task = 24

            [queues.gpucap]
            gpu = true
            max-walltime = "24:00:00"
            # qbd-gpu4 shape: 64 CPUs at 32/task = 2 tasks/node, on a 4-GPU
            # node. `max-gpus-per-node` never feeds the §8.5 GPUs-per-task
            # default, which is a flat 1 GPU/task; it is a ceiling on the layout
            # and a bound on a derived tasks-per-node (here 2 < 4, so no-op).
            max-cpus-per-node = 64
            default-cpus-per-task = 32
            max-gpus-per-node = 4

            [queues.gpudef]
            gpu = true
            max-walltime = "24:00:00"
            # Same node, but this partition hands each rank two devices — the
            # one way to depart from the global 1 (§8.5).
            max-cpus-per-node = 64
            default-cpus-per-task = 32
            max-gpus-per-node = 4
            default-gpus-per-task = 2

            [variants.submitscript]
            "default" = { queues = ["batch", "gpuq", "fillq", "gpucap", "gpudef"], default = true }

            [variants.runscript]
            "default" = { queues = ["batch", "gpuq", "fillq", "gpucap", "gpudef"], default = true }

            [variants.optionlist]
            variants = ["default"]
            "#,
        )
        .unwrap();
        Machine {
            name: "testbox".to_owned(),
            dir: PathBuf::from("/nonexistent"),
            layer: Layer::System,
            meta,
        }
    }

    fn test_cfg(gpu: bool, compat: &[&str]) -> ConfigMeta {
        let toml_text = format!(
            r#"
            name = "sim"
            variant = "default"
            gpu = {gpu}
            compatible-queues = [{}]
            thornlist = "installation-default.th"
            machine = "testbox"
            config-id = "c1"
            build-id = "b1"
            "#,
            compat.iter().map(|q| format!("\"{q}\"")).collect::<Vec<_>>().join(", ")
        );
        toml::from_str(&toml_text).unwrap()
    }

    fn test_fit(gpu: bool, compat: &[&str]) -> QueueFit {
        QueueFit::from_config(&test_cfg(gpu, compat))
    }

    fn flags() -> TopologyFlags {
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

    #[test]
    fn topology_defaults_fill_the_node() {
        let machine = test_machine();
        let db = Database::new();
        let topo = resolve_topology(&flags(), &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert_eq!(topo.queue, "batch");
        // No `name` override: the scheduler-facing name is the key itself.
        assert_eq!(topo.scheduler_queue, "batch");
        assert_eq!((topo.nodes, topo.cpus, topo.tpn, topo.tasks), (1, 1, 16, 16));
        assert_eq!(topo.total_wall, Walltime::parse("24:00:00").unwrap());
    }

    #[test]
    fn topology_derivation_with_flags_and_knobs() {
        let machine = test_machine();
        let mut db = Database::new();
        db.set_knob("allocation", "hpc_alloc".to_owned());
        db.set_knob("queue", "gpuq".to_owned());

        let mut f = flags();
        f.nodes = Some(4);
        f.cpus = Some(4);
        // Knob queue is gpuq; the config must be gpu-built to pass the check.
        let topo = resolve_topology(&f, &machine, &db, &test_fit(true, &[]), false).unwrap();
        assert_eq!(topo.queue, "gpuq");
        // The queue's `name` override is what the scheduler (@QUEUE@) sees.
        assert_eq!(topo.scheduler_queue, "gpu_part");
        assert!(topo.gpu, "queue gpu flag infers GPU");
        // The queue's max-cpus-per-node override (32) drives the layout, not [hardware]'s
        // 16: tpn = floor(32/4) = 8; tasks = 4 nodes * 8.
        assert_eq!((topo.tpn, topo.tasks), (8, 32));
        assert_eq!(topo.allocation.as_deref(), Some("hpc_alloc"));
        assert_eq!(topo.mail_type, "all");
    }

    #[test]
    fn topology_fills_node_via_default_cpus_per_task() {
        // §8.5 availability-vs-request: a machine/queue `default-cpus-per-task`
        // seeds CPUS_PER_TASK when `-c` is omitted, so the fill-the-node default
        // divides max-cpus-per-node by it (Deep Bayou: 48 / 24 = 2 tasks/node).
        let machine = test_machine();
        let db = Database::new();
        let mut f = flags();
        f.queue = Some("fillq".to_owned());

        // No `-c`: cpus defaults to the queue's 24 → tpn = floor(48/24) = 2.
        let topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn, topo.cpus), (1, 2, 2, 24));

        // An explicit `-c 1` request overrides the machine default → full-node
        // single-threaded fill: tpn = floor(48/1) = 48.
        f.cpus = Some(1);
        let topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn, topo.cpus), (1, 48, 48, 1));
    }

    #[test]
    fn explicit_tasks_caps_tasks_per_node() {
        // An explicit `--tasks` replaces the fill-the-node TASKS, so the derived
        // TASKS_PER_NODE must be capped with it: `--tasks=1` is 1 task on 1 node,
        // never `TASKS=1, TASKS_PER_NODE=16`.
        let machine = test_machine();
        let db = Database::new();

        let mut f = flags();
        f.tasks = Some(1);
        let topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn), (1, 1, 1));

        // Below the fill-the-node value but above 1.
        f.tasks = Some(3);
        let topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn), (1, 3, 3));

        // Multi-node: the cap is per node, and never below the ranks a node holds.
        f.nodes = Some(2);
        f.tasks = Some(3);
        let topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn), (2, 3, 2));

        // The fill-the-node default is untouched (tasks == nodes * tpn).
        f.nodes = Some(2);
        f.tasks = None;
        let topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn), (2, 32, 16));

        // An explicit `--tpn` stays authoritative even when it exceeds `--tasks`.
        f.nodes = None;
        f.tasks = Some(1);
        f.tpn = Some(4);
        let topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert_eq!((topo.tasks, topo.tpn), (1, 4));
    }

    #[test]
    fn tasks_default_chain() {
        let machine = test_machine();
        let db = Database::new();
        let base = || resolve_topology(&flags(), &machine, &db, &test_fit(false, &[]), false).unwrap();

        // Script setting wins over the fallback; tpn is capped to match.
        let mut topo = base();
        apply_tasks_default(&mut topo, &flags(), Some(4), Some(2));
        assert_eq!((topo.tasks, topo.tpn), (4, 4));

        // No script setting → fallback (the testsuite 2).
        let mut topo = base();
        apply_tasks_default(&mut topo, &flags(), None, Some(2));
        assert_eq!((topo.tasks, topo.tpn), (2, 2));

        // Neither → the §8.5 fill-the-node value stays.
        let mut topo = base();
        apply_tasks_default(&mut topo, &flags(), None, None);
        assert_eq!((topo.tasks, topo.tpn), (16, 16));

        // Any explicit process-layout flag disables the chain entirely.
        for set in [
            (&|f: &mut TopologyFlags| f.tasks = Some(8)) as &dyn Fn(&mut TopologyFlags),
            &|f| f.tpn = Some(8),
            &|f| f.nodes = Some(2),
        ] {
            let mut f = flags();
            set(&mut f);
            let mut topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
            let before = (topo.tasks, topo.tpn);
            apply_tasks_default(&mut topo, &f, Some(4), Some(2));
            assert_eq!((topo.tasks, topo.tpn), before, "flags win over defaults");
        }
    }

    #[test]
    fn gpus_per_task_derivation() {
        let machine = test_machine();
        let db = Database::new();
        let cfg = test_fit(false, &[]);
        let topo = |f: &TopologyFlags| resolve_topology(f, &machine, &db, &cfg, false).unwrap();

        // Non-GPU run: the variable is 0, never 1 — a script must be able to
        // tell "no GPUs" from "one GPU" using this alone.
        assert_eq!(topo(&flags()).gpus_per_task, 0);

        // One GPU per rank is the default, even on a 4-GPU node running 2
        // ranks: `max-gpus-per-node` is a ceiling, NOT a target to fill.
        let mut f = flags();
        f.queue = Some("gpucap".to_owned());
        let t = topo(&f);
        assert_eq!((t.tpn, t.gpus_per_task), (2, 1));

        // …and it stays 1 regardless of how the ranks are laid out, so a job
        // that shrinks for unrelated reasons never silently grabs more devices.
        f.tpn = Some(1);
        assert_eq!(topo(&f).gpus_per_task, 1);
        f.tpn = Some(4);
        assert_eq!(topo(&f).gpus_per_task, 1);

        // An explicit flag wins, up to what the node holds.
        f.tpn = None;
        f.gpus_per_task = Some(2);
        assert_eq!(topo(&f).gpus_per_task, 2);

        // `default-gpus-per-task` is the one way to depart from the global 1.
        let mut f = flags();
        f.queue = Some("gpudef".to_owned());
        assert_eq!(topo(&f).gpus_per_task, 2, "queue default outranks the global 1");
        f.gpus_per_task = Some(1);
        assert_eq!(topo(&f).gpus_per_task, 1, "the flag still wins");

        // A GPU queue that declares no GPU count also gets 1 — and with no
        // ceiling to check, a big explicit request is nobody's business to
        // refuse.
        let mut f = flags();
        f.queue = Some("gpuq".to_owned());
        assert_eq!(topo(&f).gpus_per_task, 1);
        f.gpus_per_task = Some(64);
        assert_eq!(topo(&f).gpus_per_task, 64);
    }

    #[test]
    fn gpus_per_node_capacity_is_enforced() {
        // Unlike CPUs, GPUs cannot be oversubscribed: a layout needing more of
        // them per node than the queue has is refused up front rather than
        // submitted to sit unschedulable (or to land with ranks fighting over
        // one device).
        let machine = test_machine();
        let db = Database::new();
        let cfg = test_fit(false, &[]);
        let err = |f: &TopologyFlags| resolve_topology(f, &machine, &db, &cfg, false).unwrap_err().to_string();

        // 4 GPUs, but 8 ranks per node want one each.
        let mut f = flags();
        f.queue = Some("gpucap".to_owned());
        f.tpn = Some(8);
        let e = err(&f);
        assert!(e.contains("8 GPUs per node") && e.contains("has 4"), "{e}");

        // At 1 GPU per rank there is nothing left to lower, so the advice must
        // point at the layout instead of at --gpus-per-task.
        assert!(e.contains("already takes one GPU") && e.contains("--tpn"), "{e}");

        // An explicit `--gpus-per-task` the node can still honor is NOT an
        // error any more: 3 each × the CPU rule's 2 ranks would need 6, but the
        // derived rank count is bounded by the GPU budget (4/3 = 1 rank), so it
        // resolves to a layout that fits instead of being refused.
        let mut f = flags();
        f.queue = Some("gpucap".to_owned());
        f.gpus_per_task = Some(3);
        let t = resolve_topology(&f, &machine, &db, &cfg, false).unwrap();
        assert_eq!((t.tpn, t.gpus_per_task), (1, 3));

        // An over-request no layout can satisfy — more GPUs per rank than the
        // whole node holds — is still refused, and there the advice DOES name
        // the flag the user turned.
        f.gpus_per_task = Some(5);
        let e = err(&f);
        assert!(e.contains("5 GPUs per node") && e.contains("--gpus-per-task 4"), "{e}");

        // Exactly filling the node is fine — the check is a ceiling, not a cap
        // on using everything (2 each × 2 ranks = 4).
        f.gpus_per_task = Some(2);
        assert!(resolve_topology(&f, &machine, &db, &cfg, false).is_ok());

        // A queue whose `default-gpus-per-task` overshoots its own node is
        // caught too: gpudef hands out 2 each, so 4 ranks would need 8.
        let mut f = flags();
        f.queue = Some("gpudef".to_owned());
        f.tpn = Some(4);
        assert!(err(&f).contains("8 GPUs per node"), "{}", err(&f));

        // Shrinking the layout afterwards leaves GPUS_PER_TASK alone — it is
        // per-task, so fewer ranks simply need fewer GPUs.
        let mut f = flags();
        f.queue = Some("gpudef".to_owned());
        let mut topo = resolve_topology(&f, &machine, &db, &cfg, false).unwrap();
        apply_tasks_default(&mut topo, &f, Some(1), None);
        assert_eq!((topo.tpn, topo.gpus_per_task), (1, 2));
    }

    #[test]
    fn gpus_per_task_requires_a_gpu_run() {
        let machine = test_machine();
        let db = Database::new();
        let mut f = flags();
        f.gpus_per_task = Some(2);
        // Default queue `batch` is not GPU-flagged: refused, not ignored.
        let err = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--gpus-per-task"), "{err}");
        // `--gpu` alone is enough to make it meaningful again.
        f.gpu = true;
        assert_eq!(resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap().gpus_per_task, 2);
    }

    #[test]
    fn queue_and_gpu_guards() {
        let machine = test_machine();
        let db = Database::new();

        // compatible-queues mismatch refused (§4.4)…
        let cfg = test_fit(false, &["gpuq"]);
        let err = resolve_topology(&flags(), &machine, &db, &cfg, false).unwrap_err().to_string();
        assert!(err.contains("--force-queue"), "{err}");
        // …unless forced.
        assert!(resolve_topology(&flags(), &machine, &db, &cfg, true).is_ok());

        // GPU cross-check is one-directional (D12): a GPU binary in a non-GPU
        // context (default queue batch) is refused…
        let err = resolve_topology(&flags(), &machine, &db, &test_fit(true, &[]), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GPU"), "{err}");
        // …unless forced.
        assert!(resolve_topology(&flags(), &machine, &db, &test_fit(true, &[]), true).is_ok());
        // A non-GPU binary on a GPU queue is allowed (advisory only).
        let mut f = flags();
        f.queue = Some("gpuq".to_owned());
        let topo = resolve_topology(&f, &machine, &db, &test_fit(false, &[]), false).unwrap();
        assert!(topo.gpu);
    }

    #[test]
    fn queue_build_universe_gate() {
        // A machine whose two queues are gated by build universe (§4.4):
        // "host-only" serves host builds, "sing" serves et-sing builds.
        let meta: Meta = toml::from_str(
            r#"
            [machine]
            name = "gq"

            [queues.host-only]
            default = true
            build-universes = ["host"]

            [queues.sing]
            build-universes = ["et-sing"]

            [variants.submitscript]
            "s" = { queues = ["host-only", "sing"] }

            [variants.runscript]
            "r" = { queues = ["host-only", "sing"] }

            [variants.optionlist]
            variants = ["default"]

            [universes.et-sing]
            wrapper-argv = ["apptainer", "exec", "et.sif"]
            "#,
        )
        .unwrap();
        meta.validate("gq").unwrap();
        let machine =
            Machine { name: "gq".to_owned(), dir: PathBuf::from("/nonexistent"), layer: Layer::System, meta };
        let db = Database::new();

        let cfg_in = |universe: &str| -> QueueFit {
            let cfg: ConfigMeta = toml::from_str(&format!(
                r#"
                name = "sim"
                variant = "default"
                thornlist = "installation-default.th"
                machine = "gq"
                universe = "{universe}"
                config-id = "c1"
                build-id = "b1"
                "#
            ))
            .unwrap();
            QueueFit::from_config(&cfg)
        };

        // A host build cannot be routed to the et-sing-gated queue — a hard
        // error even with --force-queue (unlike the compatible-queues guard).
        let mut f = flags();
        f.queue = Some("sing".to_owned());
        let err = resolve_topology(&f, &machine, &db, &cfg_in("host"), true).unwrap_err().to_string();
        assert!(err.contains("build universe") && err.contains("\"sing\""), "{err}");

        // Each build flavor's default resolves to its sole compatible queue.
        assert_eq!(resolve_topology(&flags(), &machine, &db, &cfg_in("host"), false).unwrap().queue, "host-only");
        assert_eq!(resolve_topology(&flags(), &machine, &db, &cfg_in("et-sing"), false).unwrap().queue, "sing");
    }

    #[test]
    fn chaining_math() {
        let h24 = Walltime::parse("24:00:00").unwrap();
        let h100 = Walltime::parse("100:00:00").unwrap();
        // 100 h on a 24 h queue → 5 segments of 24 h (§8.8's own example).
        assert_eq!(chain_segments(h100, h24), (5, h24));
        // Fits → one segment at the requested wall.
        let h2 = Walltime::parse("2:00:00").unwrap();
        assert_eq!(chain_segments(h2, h24), (1, h2));
        assert_eq!(chain_segments(h24, h24), (1, h24));
    }

    #[test]
    fn checkpoint_buffer_default() {
        // 24 h reservation → 1 h margin; 2 h → the 10-minute floor.
        assert_eq!(default_checkpt_buffer(Walltime(24 * 3600)), Walltime(3600));
        assert_eq!(default_checkpt_buffer(Walltime(2 * 3600)), Walltime(600));
    }

    #[test]
    fn short_name_rules() {
        assert_eq!(short_sim_name("bbh", 3), "bbh-3");
        assert_eq!(short_sim_name("my sim", 0), "my_sim-0");
        assert_eq!(short_sim_name("0numeric", 1), "J0numeric-1");
        assert_eq!(short_sim_name("averylongsimulationname", 12).len(), 15);
    }

    #[test]
    fn env_setup_follows_the_run_universe() {
        let mut machine = test_machine();
        machine.meta.environment.env_setup = Some("export M=1".to_owned());
        // An identity universe carrying only a run-phase env override (§6.1).
        let sing: crate::mdb::Universe = toml::from_str("env-run-setup = \"module load sing\"").unwrap();
        machine.meta.universes.insert("sing".to_owned(), sing);

        let mut v = VarSet::new();
        set_machine_vars(&mut v, &machine, "batch", None).unwrap();
        assert_eq!(v.get("ENV_SETUP").unwrap().canonical(), "export M=1");
        let mut v = VarSet::new();
        set_machine_vars(&mut v, &machine, "batch", Some("sing")).unwrap();
        assert_eq!(v.get("ENV_SETUP").unwrap().canonical(), "export M=1\nmodule load sing");
    }

    #[test]
    fn assembles_full_var_set() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = test_machine();
        let meta = SimulationMeta {
            parfile: "bbh.par".to_owned(),
            configuration: "sim".to_owned(),
            simulation_id: "simulation-bbh-x".to_owned(),
            alias: "et".to_owned(),
            sourcedir: PathBuf::from("/opt/Cactus"),
            ..Default::default()
        };
        let sim = Simulation {
            name: "bbh".to_owned(),
            dir: tmp.path().join("sim").join("bbh"),
            meta,
        };
        let db = Database::new();
        let topo = resolve_topology(&flags(), &machine, &db, &test_fit(false, &[]), false).unwrap();
        let wall = Walltime::parse("1-01:30:45").unwrap();
        let vars = assemble(&RestartVarsInput {
            sim: &sim,
            machine: &machine,
            topo: &topo,
            sim_home: tmp.path(),
            restart_id: 2,
            job_wall: wall,
            checkpt_buffer: Walltime(3600),
            chained_job_id: "1234",
            hostname: "host.example",
            user: "alice",
            email: "a@example.org",
            run_universe: None,
            debug: false,
        })
        .unwrap();

        // Every §6.3 family present; sh substitution of a realistic template works.
        let line = vars
            .substitute(
                "@CACTUP@ sim run @SIMULATION_NAME@ --installation=@ALIAS@ \
                 --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ --restart-id=@RESTART_ID@ \
                 # @WALLTIME@ @CHECKPOINT_WALLTIME@ w=@WALLTIME_HH@:@WALLTIME_MM@ \
                 q=@QUEUE@ n=@NODES@ t=@TASKS@ chained=@CHAINED_JOB_ID@ smt=@THREADS_PER_CPU@ mem=@MEMORY@",
                Syntax::Plain, // the `#` is part of the probe, not a comment
            )
            .unwrap();
        assert!(line.contains("sim run bbh --installation=et"), "{line}");
        assert!(line.contains("--restart-id=2"), "{line}");
        // 1-01:30:45 = 25h30m45s hard wall; hint = wall − 1 h; WALLTIME_HH folds days.
        assert!(line.contains("# 01-01:30:45 01-00:30:45 w=25:30"), "{line}");
        assert!(line.contains("chained=1234 smt=2 mem=64000"), "{line}");

        assert_eq!(vars.get("RUNDIR").unwrap().canonical(), restart::restart_dir(&sim.dir, 2).display().to_string());
        assert_eq!(vars.get("SIM_HOME").unwrap().canonical(), tmp.path().display().to_string());
        assert!(vars.get("EXECUTABLE").unwrap().canonical().ends_with(".cactup/exe"));
        assert!(vars.get("PARFILE").unwrap().canonical().ends_with("output-0002/bbh.par"));
    }

    /// A no-flag `sim submit` should use the whole node it was given. On a
    /// machine with GPU partitions of different widths that is not automatic:
    /// `default-cpus-per-task` sets the rank count, `max-gpus-per-node` sets
    /// how many ranks the GPUs can feed, and the two are declared in different
    /// places — so an inherited CPU default silently underfills the wider
    /// partition. qbd carries both shapes, so it is the regression test.
    #[test]
    fn qbd_defaults_fill_each_partition() {
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        );
        let machine = mdb.load("qbd").unwrap();
        let db = Database::new();
        let cfg: ConfigMeta = toml::from_str(
            "name=\"s\"\nvariant=\"default\"\ngpu=true\nthornlist=\"t.th\"\n\
             machine=\"qbd\"\nconfig-id=\"c\"\nbuild-id=\"b\"",
        )
        .unwrap();
        let fit = QueueFit::from_config(&cfg);

        for (queue, gpus) in [("gpu2", 2), ("gpu4", 4)] {
            let mut f = flags();
            f.queue = Some(queue.to_owned());
            let t = resolve_topology(&f, &machine, &db, &fit, false).unwrap();
            let hw = machine.meta.effective_hardware(queue).unwrap();
            assert_eq!(hw.max_gpus_per_node, Some(gpus), "{queue}");
            // One rank per GPU, every GPU busy…
            assert_eq!(t.gpus_per_task, 1, "{queue}");
            assert_eq!(t.tpn, gpus, "{queue}: a rank per GPU");
            assert_eq!(t.tpn * t.gpus_per_task, gpus, "{queue}: no idle GPUs");
            // …and the node's cores split evenly among them, none left over.
            assert_eq!(t.tpn * t.cpus, hw.max_cpus_per_node.unwrap(), "{queue}: no idle cores");
        }

        // qbd's queues derive exactly their GPU count of ranks, so the GPU
        // bound on a derived tpn is a no-op here — and an explicit --tasks
        // skips it entirely: a two-node rank count keeps the per-node fill and
        // passes the ceiling exactly as it always did.
        let mut f = flags();
        f.queue = Some("gpu4".to_owned());
        f.tasks = Some(8);
        let t = resolve_topology(&f, &machine, &db, &fit, false).unwrap();
        assert_eq!((t.tasks, t.tpn, t.cpus, t.gpus_per_task), (8, 4, 16, 1));
    }

    /// db1 (Deep Bayou) declares no `max-gpus-per-node` at all, so both the
    /// GPU bound on a derived tpn and the §8.5 ceiling are inert there: the
    /// CPU rule alone fills the node, with or without explicit rank flags.
    #[test]
    fn db1_layouts_are_untouched_by_the_gpu_bound() {
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        );
        let machine = mdb.load("db1.hpc.lsu.edu").unwrap();
        let db = Database::new();
        let cfg: ConfigMeta = toml::from_str(
            "name=\"s\"\nvariant=\"default\"\ngpu=true\nthornlist=\"t.th\"\n\
             machine=\"db1.hpc.lsu.edu\"\nconfig-id=\"c\"\nbuild-id=\"b\"",
        )
        .unwrap();
        let fit = QueueFit::from_config(&cfg);
        let hw = machine.meta.effective_hardware("gpu").unwrap();
        assert_eq!(hw.max_gpus_per_node, None);

        // No flags: 48 cores at 24/task = 2 ranks, one GPU each, no ceiling.
        let t = resolve_topology(&flags(), &machine, &db, &fit, false).unwrap();
        assert_eq!((t.tasks, t.tpn, t.cpus, t.gpus_per_task), (2, 2, 24, 1));

        // Explicit rank counts resolve as they always did, never refused on
        // GPU grounds (nothing is declared to check against).
        let mut f = flags();
        f.tasks = Some(4);
        let t = resolve_topology(&f, &machine, &db, &fit, false).unwrap();
        assert_eq!((t.tasks, t.tpn), (4, 2));
    }

    /// The mirror case of `qbd_defaults_fill_each_partition`: a machine that
    /// deliberately does NOT fill its node. omnia has 72 cores and one MI210,
    /// and asks for 8 CPUs per rank — the CPU rule alone would infer 9 ranks
    /// for one device and the §8.5 ceiling would refuse the submit outright, so
    /// the derived rank count is bounded by the GPU budget instead.
    #[test]
    fn omnia_default_is_one_rank_and_does_not_fill_the_node() {
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        );
        let machine = mdb.load("omnia").unwrap();
        let db = Database::new();
        let cfg: ConfigMeta = toml::from_str(
            "name=\"s\"\nvariant=\"default\"\ngpu=true\nthornlist=\"t.th\"\n\
             machine=\"omnia\"\nconfig-id=\"c\"\nbuild-id=\"b\"",
        )
        .unwrap();
        let fit = QueueFit::from_config(&cfg);
        let hw = machine.meta.effective_hardware("local").unwrap();
        assert_eq!(
            (hw.max_cpus_per_node, hw.default_cpus_per_task, hw.max_gpus_per_node),
            (Some(72), Some(8), Some(1))
        );

        // No flags: one rank on its one GPU, 8 of the node's 72 cores, the
        // rest left for whoever else is on the box.
        let t = resolve_topology(&flags(), &machine, &db, &fit, false).unwrap();
        assert_eq!((t.tasks, t.tpn, t.cpus, t.gpus_per_task), (1, 1, 8, 1));
        assert!(t.tpn * t.cpus < hw.max_cpus_per_node.unwrap(), "the whole point: idle cores");

        // -c still fills the node on request, and still lands on one rank.
        let mut f = flags();
        f.cpus = Some(72);
        let t = resolve_topology(&f, &machine, &db, &fit, false).unwrap();
        assert_eq!((t.tasks, t.tpn, t.cpus), (1, 1, 72));

        // An explicit --tpn is authoritative, so it reaches the ceiling and is
        // refused there rather than being silently bounded.
        let mut f = flags();
        f.tpn = Some(4);
        let err = resolve_topology(&f, &machine, &db, &fit, false).unwrap_err().to_string();
        assert!(err.contains("needs 4 GPUs per node") && err.contains("--tpn/--tasks"), "{err}");

        // An explicit --tasks is a rank count too: on this no-batch single
        // node every rank lands on the box, so it must not slip past the
        // ceiling on the strength of a quietly-bounded derived tpn (which
        // would let 10 ranks fight over the one MI210 via `mpirun -np 10`).
        let mut f = flags();
        f.tasks = Some(10);
        let err = resolve_topology(&f, &machine, &db, &fit, false).unwrap_err().to_string();
        assert!(err.contains("needs 9 GPUs per node") && err.contains("--tpn/--tasks"), "{err}");

        // …while a --tasks the GPU budget can honor still resolves.
        let mut f = flags();
        f.tasks = Some(1);
        let t = resolve_topology(&f, &machine, &db, &fit, false).unwrap();
        assert_eq!((t.tasks, t.tpn), (1, 1));
    }

    /// The bundled `.sh` scripts are only checked at substitution time, so a
    /// template naming a variable cactup does not set fails at submit — on the
    /// user's cluster, not here. db1's runscripts reference `@GPUS_PER_TASK@`;
    /// this pins that the assembled set actually carries it.
    #[test]
    fn real_templates_naming_gpus_per_task_substitute() {
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        );
        let mut checked = 0;
        for (name, _layer) in mdb.machines().unwrap() {
            let machine = mdb.load(&name).unwrap();
            for kind in [
                crate::mdb::ScriptKind::Run,
                crate::mdb::ScriptKind::Submit,
                crate::mdb::ScriptKind::BuildSubmit,
            ] {
                for variant in machine.meta.script_variants(kind).variants.keys() {
                    let script = machine.script_path(kind, variant).unwrap();
                    if script.python {
                        continue; // .py variants read globals, not @NAME@ tokens
                    }
                    let body = std::fs::read_to_string(&script.path).unwrap();
                    if !body.contains("@GPUS_PER_TASK@") {
                        continue;
                    }
                    // A test var set exercises the same `set_topology_vars`
                    // block the sim path uses, and reaches the test scripts too.
                    let mut v = VarSet::new();
                    let topo = resolve_topology(
                        &flags(),
                        &test_machine(),
                        &Database::new(),
                        &test_fit(false, &[]),
                        false,
                    )
                    .unwrap();
                    set_topology_vars(&mut v, &topo, "job");
                    for token in body.split('@').skip(1).step_by(2) {
                        // Fill everything this template names that isn't ours,
                        // so the assertion is about GPUS_PER_TASK alone.
                        if v.get(token).is_none() {
                            v.set(token, "x");
                        }
                    }
                    v.substitute(&body, Syntax::Shell).unwrap_or_else(|e| {
                        panic!("{name} {kind:?} {variant} failed to substitute: {e:#}")
                    });
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "expected at least one template using @GPUS_PER_TASK@");
    }
}



