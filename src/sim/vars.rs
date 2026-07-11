//! TOPOLOGY → process layout (§8.5) and assembly of the canonical §6.3
//! variable set for a restart.

use crate::args::TopologyFlags;
use crate::build::ConfigMeta;
use crate::database::Database;
use crate::mdb::{Machine, Phase};
use crate::sim::{restart, Simulation};
use crate::template::VarSet;
use crate::walltime::Walltime;
use crate::Res;
use anyhow::{anyhow, bail};

/// The resolved §8.5 topology for one submit/run invocation.
#[derive(Debug, Clone)]
pub struct Topology {
    pub nodes: u32,
    pub tasks: u32,
    pub tpn: u32,
    pub cpus: u32,
    pub gpu: bool,
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

/// Resolve the §8.5 topology: flag → knob → machine defaults, with the
/// queue-compatibility and GPU cross-checks (§4.4 / D12).
pub fn resolve_topology(
    flags: &TopologyFlags,
    machine: &Machine,
    db: &Database,
    cfg: &ConfigMeta,
    force_queue: bool,
) -> Res<Topology> {
    let m = &machine.name;

    // Queue: -q → knob → machine default queue.
    let queue = flags
        .queue
        .clone()
        .or_else(|| db.knob("queue").map(str::to_owned))
        .or_else(|| machine.meta.default_queue().map(str::to_owned))
        .ok_or_else(|| {
            anyhow!("no queue given (-q), no `queue` knob set, and machine \"{m}\" has no default queue")
        })?;
    let queue_def = machine.meta.queue(&queue)?;

    // compatible-queues guard (§4.4 / D12).
    if !cfg.compatible_queues.is_empty() && !cfg.compatible_queues.contains(&queue) && !force_queue {
        bail!(
            "config \"{}\" is only compatible with queue(s) {} (not \"{queue}\"); \
             use --force-queue to override",
            cfg.name,
            cfg.compatible_queues.join(", ")
        );
    }

    // GPU: -g or the queue's gpu flag, cross-checked against the binary.
    let gpu = flags.gpu || queue_def.gpu;
    if gpu != cfg.gpu && !force_queue {
        bail!(
            "config \"{}\" was built {} GPU support but this run is {} (queue \"{queue}\"); \
             use --force-queue to override (§8.5)",
            cfg.name,
            if cfg.gpu { "with" } else { "without" },
            if gpu { "GPU" } else { "non-GPU" },
        );
    }

    // Process layout (§8.5 derivation), from the queue-effective hardware
    // (per-queue overrides falling back to [hardware] — §4.2).
    let max_tpn = machine.meta.effective_hardware(&queue)?.max_tasks_per_node.unwrap_or(1);
    let cpus = flags.cpus.unwrap_or(1).max(1);
    let tpn = flags.tpn.unwrap_or_else(|| (max_tpn / cpus).max(1));
    let nodes = flags.nodes.unwrap_or(1);
    let tasks = flags.tasks.unwrap_or(nodes * tpn);

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
        scheduler_queue: queue_def.name.clone().unwrap_or_else(|| queue.clone()),
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

/// Apply the default-tasks chain to a resolved topology: the selected script
/// variant's `tasks` setting (§4.2) first, then the caller's fallback (2 for
/// testsuite runs — §11.6), else keep the §8.5 fill-the-node value. Any
/// explicit process-layout flag (-n/-T/-t) disables the whole chain; tpn is
/// capped so the recorded layout stays self-consistent.
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
    /// `@FROM_RESTART_COMMAND@` for the submit template (§8.3.1).
    pub from_restart_command: &'a str,
    pub hostname: &'a str,
    pub user: &'a str,
    pub email: &'a str,
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
    v.set("ALLOCATION", topo.allocation.as_deref().unwrap_or(""));
    v.set("QUEUE", topo.scheduler_queue.as_str());
    v.set("MAIL", topo.mail.as_deref().unwrap_or(""));
    v.set("MAIL_TYPE", topo.mail_type.as_str());
    v.set("JOB_NAME", topo.job_name.clone().unwrap_or_else(|| default_job_name.to_owned()));
}

/// The two walltimes (§6.3): hard wall + checkpoint hint = wall − buffer.
pub fn set_walltime_vars(v: &mut VarSet, wall: Walltime, buffer: Walltime) {
    v.set("WALLTIME", wall.canonical());
    v.set("WALLTIME_HH", wall.component_hours());
    v.set("WALLTIME_MM", wall.component_minutes());
    v.set("WALLTIME_SS", wall.component_seconds());
    v.set("WALLTIME_SECONDS", wall.total_seconds());
    v.set("WALLTIME_MINUTES", wall.total_minutes());
    v.set("WALLTIME_HOURS", wall.total_hours());
    let hint = Walltime(wall.0.saturating_sub(buffer.0));
    v.set("CHECKPOINT_WALLTIME", hint.canonical());
    v.set("CHECKPOINT_WALLTIME_SECONDS", hint.total_seconds());
    v.set("CHECKPOINT_WALLTIME_HOURS", hint.total_hours());
}

/// The machine-derived block (§6.3): the queue-effective hardware facts
/// (per-queue overrides falling back to [hardware] — §4.2) + the RUN-phase
/// `ENV_SETUP` (submit-phase artifacts override it — §6.1).
pub fn set_machine_vars(v: &mut VarSet, machine: &Machine, queue: &str) -> Res<()> {
    let hw = machine.meta.effective_hardware(queue)?;
    v.set("MAX_TASKS_PER_NODE", hw.max_tasks_per_node.unwrap_or(1) as u64);
    v.set("MEMORY", hw.memory.unwrap_or(0));
    v.set("THREADS_PER_CPU", hw.threads_per_cpu() as u64);
    v.set("ENV_SETUP", machine.meta.environment.effective(Phase::Run));
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
    v.set("FROM_RESTART_COMMAND", input.from_restart_command);

    set_machine_vars(&mut v, machine, &topo.queue)?;

    // Debug (§8.4).
    v.set("RUNDEBUG", input.debug);
    v.set("DEBUGGER", "gdb");

    Ok(v)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdb::{Layer, Machine, Meta};
    use crate::sim::SimulationMeta;
    use std::path::PathBuf;

    fn test_machine() -> Machine {
        let meta: Meta = toml::from_str(
            r#"
            [machine]
            name = "testbox"

            [hardware]
            max-tasks-per-node = 16
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
            max-tasks-per-node = 32

            [variants.submitscript]
            "default" = { queues = ["batch", "gpuq"], default = true }

            [variants.runscript]
            "default" = { queues = ["batch", "gpuq"], default = true }

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
            thornlist = "einsteintoolkit.th"
            machine = "testbox"
            config-id = "c1"
            build-id = "b1"
            "#,
            compat.iter().map(|q| format!("\"{q}\"")).collect::<Vec<_>>().join(", ")
        );
        toml::from_str(&toml_text).unwrap()
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
        let topo = resolve_topology(&flags(), &machine, &db, &test_cfg(false, &[]), false).unwrap();
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
        let topo = resolve_topology(&f, &machine, &db, &test_cfg(true, &[]), false).unwrap();
        assert_eq!(topo.queue, "gpuq");
        // The queue's `name` override is what the scheduler (@QUEUE@) sees.
        assert_eq!(topo.scheduler_queue, "gpu_part");
        assert!(topo.gpu, "queue gpu flag infers GPU");
        // The queue's max-tasks-per-node override (32) drives the layout, not [hardware]'s
        // 16: tpn = floor(32/4) = 8; tasks = 4 nodes * 8.
        assert_eq!((topo.tpn, topo.tasks), (8, 32));
        assert_eq!(topo.allocation.as_deref(), Some("hpc_alloc"));
        assert_eq!(topo.mail_type, "all");
    }

    #[test]
    fn tasks_default_chain() {
        let machine = test_machine();
        let db = Database::new();
        let base = || resolve_topology(&flags(), &machine, &db, &test_cfg(false, &[]), false).unwrap();

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
            let mut topo = resolve_topology(&f, &machine, &db, &test_cfg(false, &[]), false).unwrap();
            let before = (topo.tasks, topo.tpn);
            apply_tasks_default(&mut topo, &f, Some(4), Some(2));
            assert_eq!((topo.tasks, topo.tpn), before, "flags win over defaults");
        }
    }

    #[test]
    fn queue_and_gpu_guards() {
        let machine = test_machine();
        let db = Database::new();

        // compatible-queues mismatch refused (§4.4)…
        let cfg = test_cfg(false, &["gpuq"]);
        let err = resolve_topology(&flags(), &machine, &db, &cfg, false).unwrap_err().to_string();
        assert!(err.contains("--force-queue"), "{err}");
        // …unless forced.
        assert!(resolve_topology(&flags(), &machine, &db, &cfg, true).is_ok());

        // GPU cross-check: non-gpu binary on a gpu queue refused.
        let mut f = flags();
        f.queue = Some("gpuq".to_owned());
        let err = resolve_topology(&f, &machine, &db, &test_cfg(false, &[]), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GPU"), "{err}");
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
    fn assembles_full_var_set() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = test_machine();
        let mut meta = SimulationMeta::default();
        meta.parfile = "bbh.par".to_owned();
        meta.configuration = "sim".to_owned();
        meta.simulation_id = "simulation-bbh-x".to_owned();
        meta.alias = "et".to_owned();
        meta.sourcedir = PathBuf::from("/opt/Cactus");
        let sim = Simulation {
            name: "bbh".to_owned(),
            dir: tmp.path().join("sim").join("bbh"),
            meta,
        };
        let db = Database::new();
        let topo = resolve_topology(&flags(), &machine, &db, &test_cfg(false, &[]), false).unwrap();
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
            from_restart_command: "",
            hostname: "host.example",
            user: "alice",
            email: "a@example.org",
            debug: false,
        })
        .unwrap();

        // Every §6.3 family present; sh substitution of a realistic template works.
        let line = vars
            .substitute(
                "@CACTUP@ sim run @SIMULATION_NAME@ --installation=@ALIAS@ \
                 --sim-dir=@SIMULATION_DIR@ --machine=@MACHINE@ --restart-id=@RESTART_ID@ \
                 @FROM_RESTART_COMMAND@ # @WALLTIME@ @CHECKPOINT_WALLTIME@ w=@WALLTIME_HH@:@WALLTIME_MM@ \
                 q=@QUEUE@ n=@NODES@ t=@TASKS@ chained=@CHAINED_JOB_ID@ smt=@THREADS_PER_CPU@ mem=@MEMORY@",
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
}
