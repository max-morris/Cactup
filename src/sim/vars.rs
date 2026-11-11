//! TOPOLOGY → process layout (§8.5) and assembly of the canonical §6.3
//! variable set for a restart.

use crate::args::TopologyFlags;
use crate::build::{ConfigMeta, OptionlistSource};
use crate::database::Database;
use crate::mdb::optionlist::RunsOn;
use crate::mdb::{Hardware, Machine, Phase, Queue, HOST_UNIVERSE};
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
    /// How a node count nobody gave was settled, where it follows from the
    /// rank count (§8.5): so `apply_tasks_default` settles it again for a
    /// script's `tasks`, and `nodes_note` can say why.
    pub derived_nodes: Option<DerivedNodes>,
    pub queue: String,
    /// The scheduler-facing queue name (§4.2): the queue's `name` override
    /// when set, else `queue` itself. `@QUEUE@` resolves to this; everything
    /// cactup-internal (script selection, walltime ceilings, records) keys
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

/// What a job's *binary* can run on (D12 / §4.4): the universe it was built
/// in and the flavor's `runs-on` facts. Taken off a built config's record by
/// `from_config`; sim and test jobs hand it to `resolve_topology`, build jobs
/// never do — they run `make`, not the binary (§7.9).
pub struct BinaryFit {
    pub universe: Option<String>,
    pub runs_on: RunsOn,
    /// Named in the queue-compatibility errors below.
    pub label: String,
}

impl BinaryFit {
    /// The fit exactly as the config recorded it at build time.
    pub fn from_config(cfg: &ConfigMeta) -> Self {
        BinaryFit { universe: cfg.universe.clone(), runs_on: cfg.runs_on.clone(), label: cfg.name.clone() }
    }

    /// The fit a sim or test job uses: the recorded one, except that a
    /// config built from one of the machine's flavors takes its
    /// `default-queue` from that flavor's current header. Where a binary may
    /// run (`queues`, `gpu`) is a fact about what was compiled, so it stays
    /// as recorded; which of those queues a job lands on without `-q` is a
    /// routing preference of the MDB, so an MDB edit reaches existing configs
    /// without a rebuild. A config built from an `--optionlist` file, or from
    /// a flavor the machine has since dropped, keeps its recorded one.
    pub fn for_config(machine: &Machine, cfg: &ConfigMeta) -> Res<Self> {
        let mut fit = BinaryFit::from_config(cfg);
        if let OptionlistSource::Flavor(flavor) = &cfg.optionlist_source
            && machine.flavors()?.contains(flavor)
        {
            let live = crate::mdb::optionlist::load_header(&machine.optionlist_path(flavor))?.runs_on.default_queue;
            // Only a queue this binary was built to run on: an MDB edit that
            // moved the flavor's `queues` and `default-queue` together must
            // not steer a config built before it onto a queue its own record
            // refuses. Such a config falls through to the machine default.
            let recorded = &fit.runs_on.queues;
            fit.runs_on.default_queue =
                live.filter(|q| recorded.is_empty() || recorded.iter().any(|r| r == q));
        }
        Ok(fit)
    }
}

/// Where a job's ranks land, which decides what a missing `--nodes` means
/// (§8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Handed to a batch system, which allocates as many nodes as the job
    /// asks for: a missing node count is derived from the rank count, and a
    /// layout that cannot fit on the nodes asked for is refused before
    /// anything is submitted.
    Scheduled,
    /// Run here, inside a job allocation. `nodes` is the allocation's own
    /// node count where its environment says: a missing `--nodes` is that,
    /// but never more than the ranks need; else it is derived as for a
    /// submit. Nothing is refused — what fits is the allocation's business.
    Allocation { nodes: Option<u32> },
    /// Run on this host alone — a login node, or a machine whose "jobs" are
    /// processes on the submitting host (`job-ids = "pids"`): one node unless
    /// `--nodes` says otherwise, and nothing refused, since oversubscribing
    /// a single host's CPUs is a choice a user may make.
    Host,
}

impl Placement {
    /// A job handed to the machine's scheduler (`sim submit`, `test submit`,
    /// `build submit`). On a `job-ids = "pids"` machine the "scheduler"
    /// backgrounds the script on this host, so it is still one host.
    pub fn submit(machine: &Machine) -> Self {
        match machine.meta.scheduler.job_ids {
            crate::mdb::meta::JobIds::Scheduler => Placement::Scheduled,
            crate::mdb::meta::JobIds::Pids => Placement::Host,
        }
    }

    /// A foreground run (`sim run`, `test run`): inside one of the machine's
    /// job allocations (`allocation-env`) the ranks may span its nodes;
    /// anywhere else they run on this host.
    pub fn foreground(machine: &Machine) -> Self {
        let batch = machine.meta.scheduler.job_ids == crate::mdb::meta::JobIds::Scheduler;
        match (batch, machine.meta.in_allocation()) {
            (true, Some(true)) => Placement::Allocation { nodes: machine.meta.allocation_nodes() },
            _ => Placement::Host,
        }
    }

    fn derives_nodes(self) -> bool {
        self != Placement::Host
    }
}

/// What sets how many ranks a node has room for (§8.5), named where a
/// derived node count is reported or a layout refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomLimit {
    /// `--tpn`.
    Tpn,
    /// The node's CPUs, `per_task` to a rank.
    Cpus { per_node: u32, per_task: u32 },
    /// The node's GPUs, `per_task` to a rank.
    Gpus { per_node: u32, per_task: u32 },
}

impl RoomLimit {
    fn describe(self) -> String {
        match self {
            RoomLimit::Tpn => "--tpn".to_owned(),
            RoomLimit::Cpus { per_node, per_task } => {
                format!("{} per node at {} per task", count(per_node, "CPU"), count(per_task, "CPU"))
            }
            RoomLimit::Gpus { per_node, per_task } => {
                format!("{} per node at {} per task", count(per_node, "GPU"), count(per_task, "GPU"))
            }
        }
    }
}

/// How a node count nobody gave follows from the rank count (§8.5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DerivedNodes {
    /// The ranks a node holds: `--tpn`, else the node's room.
    pub per_node: u32,
    pub limit: RoomLimit,
    pub placement: Placement,
}

impl DerivedNodes {
    /// `NODES` for `tasks` ranks: as many as they need at `per_node`, and
    /// inside an allocation of known size never more than it has — unless an
    /// explicit `--tpn` sets `per_node`, which the ranks then keep.
    fn nodes_for(self, tasks: u32) -> u32 {
        let needed = tasks.max(1).div_ceil(self.per_node.max(1));
        match (self.placement, self.limit) {
            (_, RoomLimit::Tpn) => needed,
            (Placement::Allocation { nodes: Some(a) }, _) => needed.min(a.max(1)),
            _ => needed,
        }
    }
}

impl Topology {
    /// The one line that tells the user a node count was chosen for them, and
    /// why: printed before a job is submitted or run whenever a derived count
    /// is above 1. `None` for one node, an explicit `--nodes`, or a single
    /// host.
    pub fn nodes_note(&self) -> Option<String> {
        let d = self.derived_nodes?;
        if self.nodes <= 1 {
            return None;
        }
        let (n, tasks) = (self.nodes, self.tasks);
        let verb = if d.placement == Placement::Scheduled { "Asking for" } else { "Using" };
        if let Placement::Allocation { nodes: Some(a) } = d.placement
            && n == a
        {
            return Some(format!(
                "{verb} {n} nodes, all of this job allocation's, for {}; pass -n to choose",
                count(tasks, "task")
            ));
        }
        let per = match d.limit {
            RoomLimit::Tpn => format!("{} per node (--tpn)", count(d.per_node, "task")),
            limit => {
                let per_node = count(d.per_node, "task");
                format!("at most {per_node} per node (the queue's {})", limit.describe())
            }
        };
        Some(format!("{verb} {n} nodes: {per} for {}; pass -n to choose", count(tasks, "task")))
    }
}

/// Resolve the §8.5 topology: flag → knob → machine defaults, with the
/// queue-compatibility and GPU cross-checks (§4.4 / D12).
///
/// `fit` is what the job's binary can run on. A sim or test job passes its
/// config's; a build job passes `None` — it runs `make`, not the binary, so
/// the queue it lands on is checked against the machine only (§7.9).
/// `placement` says where the ranks land, which is what a missing `--nodes`
/// is derived from.
pub fn resolve_topology(
    flags: &TopologyFlags,
    machine: &Machine,
    db: &Database,
    fit: Option<&BinaryFit>,
    force_queue: bool,
    placement: Placement,
) -> Res<Topology> {
    let m = &machine.name;

    // `-a` is shorthand for `-K allocation=` (§5.1): `main` lays it into the
    // knob overlay every snapshot carries, which is what the allocation is
    // read from below. A flag the overlay never saw (a path that bypassed
    // `Args::knob_overlay`) would be silently dropped, so it is refused.
    let knob_allocation = db.knob("allocation").filter(|a| !a.is_empty());
    if let Some(flag) = flags.allocation.as_deref().filter(|a| !a.is_empty())
        && knob_allocation != Some(flag)
    {
        bail!(
            "Internal error: --allocation {flag} did not reach the allocation knob for this command ({}); \
             pass -K allocation={flag} instead",
            knob_allocation.unwrap_or("unset")
        );
    }

    // The config's build universe gates which queues (and scripts) are
    // reachable (§4.4); configs recording none run in the implicit host.
    let cfg_universe = fit.map(|f| f.universe.as_deref().unwrap_or(HOST_UNIVERSE));

    // Queue: -q → the `queue` knob → the flavor's runs-on default-queue →
    // the machine default (for a binary, the machine default is chosen among
    // the queues compatible with its build universe). The knob is the user's
    // own standing choice, so it outranks what the MDB suggests — unless this
    // binary may not run on the queue it names, in which case it is passed
    // over with a note rather than failing every submit of a flavor it cannot
    // serve (`--force-queue` keeps it regardless). Build jobs pass no fit and
    // never read the `queue` knob: their queue comes from the `build-queue`
    // knob or `[build].queue`, resolved by the caller into `flags.queue`.
    let knob = fit.and_then(|_| db.knob("queue"));
    let skipped_knob = match (knob, fit) {
        (Some(k), Some(f)) if !force_queue && !binary_may_use(machine, f, k, flags.gpu) => Some(k),
        _ => None,
    };
    let queue = flags
        .queue
        .clone()
        .or_else(|| knob.filter(|_| skipped_knob.is_none()).map(str::to_owned))
        .or_else(|| fit.and_then(|f| f.runs_on.default_queue.clone()))
        .or_else(|| machine.meta.default_queue(cfg_universe).map(str::to_owned))
        .ok_or_else(|| match cfg_universe {
            Some(u) => anyhow!(
                "No queue given (-q), no usable `queue` knob, and machine \"{m}\" has no default queue compatible with build universe \"{u}\""
            ),
            None => anyhow!("No queue given (-q) and machine \"{m}\" has no default queue"),
        })?;
    if let (Some(k), None, Some(f)) = (skipped_knob, &flags.queue, fit) {
        eprintln!(
            "{} not using your `queue` knob (\"{k}\"): config \"{}\" may not run there; using \"{queue}\" instead",
            "note:".yellow(),
            f.label,
        );
    }
    let queue_def = machine.meta.queue(&queue)?;
    let gpu = flags.gpu || queue_def.gpu;

    if let Some(fit) = fit {
        check_binary_fit(machine, fit, &queue, queue_def, gpu, force_queue)?;
    }
    // `--gpus-per-task` is meaningful only once GPU is on: refused rather than
    // silently ignored, so a user who asked for GPUs per task and landed on a
    // CPU queue finds out here instead of from the scheduler.
    if flags.gpus_per_task.is_some() && !gpu {
        // §8.5
        bail!(
            "--gpus-per-task needs a GPU run, but queue \"{queue}\" is not a GPU queue and \
             --gpu was not given"
        );
    }

    // Process layout (§8.5 derivation), from the queue-effective hardware
    // (per-queue overrides falling back to [hardware] — §4.2).
    let hw = machine.meta.effective_hardware(&queue)?;
    // Request-side CPUS_PER_TASK: -c wins, else the machine/queue
    // `default-cpus-per-task` (simfactory's num-threads), else 1.
    let cpus = flags.cpus.or(hw.default_cpus_per_task).unwrap_or(1).max(1);
    // GPUS_PER_TASK is settled before the rank count because a derived rank
    // count is bounded by it (below); it needs no layout of its own (§8.5).
    let gpus_per_task = derive_gpus_per_task(flags, &hw, gpu);
    // More GPUs per task than a node has: no layout, and no node count, can
    // help, so this is said before any is worked out.
    check_gpus_per_task(gpus_per_task, &hw, &queue)?;
    let layout = derive_layout(flags, &hw, cpus, gpus_per_task, placement, &queue)?;
    let (nodes, tasks, tpn) = (layout.nodes, layout.tasks, layout.tpn);
    check_gpus_per_node(gpus_per_task, tpn, &hw, &queue, layout.nodes_that_fit)?;

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
        derived_nodes: layout.derived,
        scheduler_queue: machine.meta.scheduler_queue_name(&queue)?.to_owned(),
        queue,
        // `-a` reaches here through the knob overlay it is shorthand for
        // (`Args::knob_overlay`, §5.1), never as a flag of its own, so
        // @ALLOCATION@ and @KNOB(allocation)@ can never disagree. A
        // simulation's own knob is laid over this by `sim::start`.
        allocation: knob_allocation.map(str::to_owned),
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

/// What `derive_layout` settles.
struct Layout {
    nodes: u32,
    tasks: u32,
    tpn: u32,
    derived: Option<DerivedNodes>,
    /// For an explicit `--nodes` with a derived tasks-per-node: the node
    /// count the ranks fit on, which the GPU ceiling names if it refuses.
    nodes_that_fit: Option<u32>,
}

/// The §8.5 process layout `(NODES, TASKS, TASKS_PER_NODE)` from whichever
/// of `-n`/`-T`/`-t` were given and the room a node has for ranks.
///
/// A node's room is its CPUs split among ranks, `floor(MAX_CPUS_PER_NODE /
/// CPUS_PER_TASK)` and at least 1, and on a GPU run whose queue declares
/// `max-gpus-per-node` no more ranks than its GPUs can feed at
/// `GPUS_PER_TASK` each. Then:
///
/// - no `--nodes`, no `--tasks`: one node, filled — inside an allocation of
///   known size, all of its nodes;
/// - `--tasks` without `--nodes`, where the placement derives nodes: as many
///   nodes as the ranks need at the room (or `--tpn`) per node, the ranks
///   spread evenly over them; on a single host, one node as always;
/// - `--nodes` without `--tasks`: that many nodes, filled;
/// - both: the ranks spread evenly over the nodes, at most the CPU room per
///   node, and a scheduled job they cannot fit on is refused here rather
///   than by the scheduler.
///
/// Nodes are derived, and layouts refused, only on a declared
/// `max-cpus-per-node`: one detected on this host (§4.6) is a login node's.
fn derive_layout(
    flags: &TopologyFlags,
    hw: &Hardware,
    cpus: u32,
    gpus_per_task: u32,
    placement: Placement,
    queue: &str,
) -> Res<Layout> {
    // Zeros are refused by the parser; a zero from the MDB (`[build]
    // nodes`/`tasks`) or a script means "the least there is".
    let nodes_flag = flags.nodes.map(|n| n.max(1));
    let tasks_flag = flags.tasks.map(|t| t.max(1));
    let tpn_flag = flags.tpn.map(|t| t.max(1));
    let cpus = cpus.max(1);
    let cpus_per_node = hw.max_cpus_per_node.unwrap_or(1).max(1);
    let declared = hw.max_cpus_per_node.is_some() && !hw.max_cpus_per_node_detected;
    let by_cpu = (cpus_per_node / cpus).max(1);
    // `gpus_per_task` is 0 exactly when this is not a GPU run.
    let gpus = hw.max_gpus_per_node.filter(|_| gpus_per_task > 0);
    let by_gpu = gpus.map(|max| (max / gpus_per_task).max(1));
    // With GPUs indivisible and one per rank, a node holding fewer devices
    // than the CPU split implies cannot run that many ranks. Bounding the
    // room by them is what lets a GPU machine declare a polite
    // `default-cpus-per-task` (a rank wanting 8 of omnia's 72 cores) without
    // the CPU rule inferring 9 ranks for its single MI210. GPUs still never
    // *build* a layout: an explicit `--tpn` is authoritative.
    let (room, room_limit) = match (by_gpu, gpus) {
        (Some(g), Some(max)) if g < by_cpu => {
            (g, RoomLimit::Gpus { per_node: max, per_task: gpus_per_task })
        }
        _ => (by_cpu, RoomLimit::Cpus { per_node: cpus_per_node, per_task: cpus }),
    };
    // Nodes follow from the ranks at an explicit `--tpn`, which needs no node
    // size, or at the room of a declared one.
    let derived = (placement.derives_nodes() && (declared || tpn_flag.is_some())).then(|| {
        let (per_node, limit) = tpn_flag.map_or((room, room_limit), |t| (t, RoomLimit::Tpn));
        DerivedNodes { per_node, limit, placement }
    });
    // A known allocation's own size, which a run inside it with no `--nodes`
    // uses even where the node size is unknown.
    let allocation = match placement {
        Placement::Allocation { nodes: Some(a) } => Some(a.max(1)),
        _ => None,
    };

    match (nodes_flag, tasks_flag, derived) {
        (None, Some(tasks), Some(d)) => {
            // §8.5: the node count follows from the rank count.
            let nodes = d.nodes_for(tasks);
            let tpn = tpn_flag.unwrap_or_else(|| tasks.div_ceil(nodes).max(1));
            return Ok(Layout { nodes, tasks, tpn, derived, nodes_that_fit: None });
        }
        (None, None, Some(d)) => {
            let nodes = allocation.unwrap_or(1);
            let tasks = nodes.saturating_mul(d.per_node);
            return Ok(Layout { nodes, tasks, tpn: d.per_node, derived, nodes_that_fit: None });
        }
        _ => {}
    }

    let nodes = nodes_flag.or(allocation).unwrap_or(1);
    // An explicit `--tasks` on one host is a requested rank count: every one
    // of those ranks lands on the node, so it keeps the CPU-rule room (not
    // the GPU-bounded one) and meets the GPU ceiling instead of slipping
    // past it on the strength of a quietly shrunk TASKS_PER_NODE. With
    // `--nodes` given too, the same holds per node.
    let fill = tpn_flag.unwrap_or(if tasks_flag.is_none() { room } else { by_cpu });
    let tasks = tasks_flag.unwrap_or_else(|| nodes.saturating_mul(fill));
    // An explicit `--tasks` overrides the fill-the-node `TASKS`, so a derived
    // `TASKS_PER_NODE` is capped to keep the layout self-consistent
    // (`--tasks=1` must not report 2 tasks/node) — the same capping
    // `apply_tasks_default` does for the script default. A user-given
    // `--tpn` stays authoritative. No-op in the default case, where
    // `tasks == nodes * tpn`.
    let tpn = tpn_flag.unwrap_or_else(|| fill.min(tasks.div_ceil(nodes).max(1)));
    // The node count these ranks fit on, at the GPU-bounded room: what an
    // explicit `--nodes` that cannot hold them is told to ask for.
    let fit_room = tpn_flag.unwrap_or(room);
    let nodes_that_fit = (nodes_flag.is_some() && tpn_flag.is_none() && derived.is_some())
        .then(|| tasks.div_ceil(fit_room));

    // A batch system refuses a job whose ranks do not fit on the nodes it
    // asks for, and does not say why; say so here, before anything is
    // submitted.
    if placement == Placement::Scheduled && u64::from(tasks) > u64::from(nodes) * u64::from(tpn) {
        let needed = tasks.div_ceil(fit_room);
        let on = count(nodes, "node");
        if tpn_flag.is_some() {
            // §8.5
            bail!(
                "{tasks} tasks do not fit on {on} at {} per node (--tpn); \
                 ask for -n {needed}, or leave out -n to have it derived",
                count(tpn, "task"),
            );
        }
        if declared {
            // §8.5
            bail!(
                "{tasks} tasks of {} each do not fit on {on}: a node of queue \"{queue}\" has room \
                 for {room} of them ({}); ask for -n {needed}, or leave out -n to have it derived",
                count(cpus, "CPU"),
                room_limit.describe(),
            );
        }
    }
    Ok(Layout { nodes, tasks, tpn, derived: None, nodes_that_fit })
}

/// `1 node`, `4 nodes`.
fn count(n: u32, noun: &str) -> String {
    if n == 1 { format!("1 {noun}") } else { format!("{n} {noun}s") }
}

/// Whether the binary `fit` describes may run on `queue` at all — the same
/// facts `check_binary_fit` enforces, as a question: the queue exists, serves
/// the build universe, is one of `runs-on.queues` (when listed), and is not a
/// non-GPU context for a GPU binary.
pub(crate) fn binary_may_use(machine: &Machine, fit: &BinaryFit, queue: &str, gpu_flag: bool) -> bool {
    let Ok(def) = machine.meta.queue(queue) else { return false };
    let universe = fit.universe.as_deref().unwrap_or(HOST_UNIVERSE);
    let allowed = &fit.runs_on.queues;
    def.compatible_with(universe)
        && (allowed.is_empty() || allowed.iter().any(|q| q == queue))
        && !(fit.runs_on.gpu && !(gpu_flag || def.gpu))
}

/// The checks that keep a binary off a queue it cannot run on: the queue's
/// build-universe gate (§4.4), the config's compatible-queues (D12), and the
/// one-directional GPU cross-check (D12). Sim and test jobs only — a build
/// job's queue is where `make` runs, not the binary (§7.9).
fn check_binary_fit(
    machine: &Machine,
    fit: &BinaryFit,
    queue: &str,
    queue_def: &Queue,
    gpu: bool,
    force_queue: bool,
) -> Res<()> {
    let cfg_universe = fit.universe.as_deref().unwrap_or(HOST_UNIVERSE);

    // Build-universe gate (§4.4): a queue restricted to other build universes
    // cannot serve this config. Structural like script compatibility — no
    // --force-queue escape (unlike the compatible-queues/GPU guards below).
    if !queue_def.compatible_with(cfg_universe) {
        bail!(
            "Queue \"{queue}\" is not compatible with build universe \"{cfg_universe}\" \
             (build-universes = [{}])",
            queue_def.build_universes.as_deref().unwrap_or_default().join(", ")
        );
    }

    // compatible-queues guard (§4.4 / D12).
    let allowed = &fit.runs_on.queues;
    if !allowed.is_empty() && !allowed.iter().any(|q| q == queue) && !force_queue {
        bail!(
            "Config \"{}\" is only compatible with queue(s) {} (not \"{queue}\"); \
             use --force-queue to override",
            fit.label,
            allowed.join(", ")
        );
    }

    // GPU: -g or the queue's gpu flag, cross-checked one-directionally
    // against the binary (D12): only a GPU config in a non-GPU context is
    // refused. A non-GPU config on a GPU queue is allowed — some machines'
    // only real partition is GPU-flagged — with an advisory when a non-GPU
    // queue existed.
    if fit.runs_on.gpu && !gpu && !force_queue {
        // §8.5
        bail!(
            "Config \"{}\" was built with GPU support but this run is non-GPU (queue \"{queue}\"); \
             use --force-queue to override",
            fit.label,
        );
    }
    if !fit.runs_on.gpu && gpu && machine.meta.queues.values().any(|q| !q.gpu) {
        // D12
        eprintln!(
            "{} config \"{}\" was built without GPU support but queue \"{queue}\" is GPU-flagged; \
             this machine also has non-GPU queues",
            "note:".yellow(),
            fit.label,
        );
    }
    Ok(())
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

/// The per-task half of the §8.5 GPU ceiling: one task asking for more GPUs
/// than a node of `queue` holds (or a GPU run on a queue declaring none).
fn check_gpus_per_task(per_task: u32, hw: &Hardware, queue: &str) -> Res<()> {
    match hw.max_gpus_per_node {
        Some(max) if per_task > max => {
            let fix = if max == 0 {
                format!("queue \"{queue}\" has no GPUs at all; run without GPUs or pick another queue")
            } else {
                format!("--gpus-per-task {max} or lower fits this layout")
            };
            // §8.5
            bail!(
                "This layout needs {} per node for even one task, but queue \"{queue}\" has {max}; \
                 {fix}",
                count(per_task, "GPU"),
            );
        }
        _ => Ok(()),
    }
}

/// The GPUs-per-node ceiling (§8.5): `GPUS_PER_TASK × TASKS_PER_NODE` may not
/// exceed `max-gpus-per-node`. Checked once the whole layout is settled, and
/// only for a GPU run (`per_task` is 0 otherwise, so the product is 0).
///
/// A derived `TASKS_PER_NODE` (no `--tpn`, and no `--tasks` the node count
/// cannot absorb) is already bounded by the GPU budget in `derive_layout`, so
/// what reaches here and fails is an explicitly requested layout:
/// `--tpn`/`--tasks` asking for more ranks than the nodes have devices for,
/// or a `--gpus-per-task`/`default-gpus-per-task` above the node's count.
fn check_gpus_per_node(
    per_task: u32,
    tpn: u32,
    hw: &Hardware,
    queue: &str,
    nodes_that_fit: Option<u32>,
) -> Res<()> {
    if per_task == 0 {
        return Ok(());
    }
    let tpn = tpn.max(1);
    if let Some(max) = hw.max_gpus_per_node {
        let needed = u64::from(per_task) * u64::from(tpn);
        if needed > u64::from(max) {
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
            // An explicit `--nodes` the ranks would fit on more of: the same
            // advice as for one the CPUs cannot hold. Not where no rank count
            // helps.
            let more_nodes = match nodes_that_fit {
                Some(n) if max > 0 && per_task <= max => {
                    format!("; or ask for -n {n}, or leave out -n to have it derived")
                }
                _ => String::new(),
            };
            // §8.5
            bail!(
                "This layout needs {needed} GPUs per node ({per_task} per task × {tpn} tasks/node) \
                 but queue \"{queue}\" has {max}; {fix}{more_nodes}"
            );
        }
    }
    Ok(())
}

/// Apply the default-tasks chain to a resolved topology: the selected script
/// script's `tasks` setting (§4.2) first, then the caller's fallback (2 for
/// testsuite runs — §11.6), else keep the §8.5 fill-the-node value. Any
/// explicit process-layout flag (-n/-T/-t) disables the whole chain. Where
/// the placement derives nodes, the node count follows from the new rank
/// count as from a `--tasks`; tpn is capped so the recorded layout stays
/// self-consistent.
///
/// `GPUS_PER_TASK` needs no revisiting here: it is per-*task*, so shrinking the
/// rank count leaves it untouched, and `tpn` stays within a node's room —
/// which can only relax the §8.5 GPUs-per-node ceiling, never breach it.
/// (The one exception is a run inside an allocation with fewer nodes than
/// the ranks need, where what fits is the allocation's business.)
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
        // A script's own `tasks` is a rank count like `--tasks`: where nodes
        // are derived they follow from it (§8.5). The caller's fallback (a
        // testsuite's 2) is not one — those ranks stay on one node, as they
        // always have. Else tpn is capped to match.
        match (topo.derived_nodes, script_tasks.is_some()) {
            (Some(d), true) => {
                topo.nodes = d.nodes_for(t);
                topo.tpn = t.div_ceil(topo.nodes);
            }
            (_, false) => {
                topo.nodes = 1;
                topo.tpn = topo.tpn.min(t);
            }
            (None, true) => topo.tpn = topo.tpn.min(t.div_ceil(topo.nodes.max(1))),
        }
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
    /// `@SCRATCH_HOME@`: the `scratch-home` knob over the machine's,
    /// resolved (`Meta::paths_for`); empty when neither is set.
    pub scratch_home: &'a str,
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
    v.set("ENV_SETUP", machine.meta.phase_env(run_universe, Phase::Run));
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
    v.set("SCRATCH_HOME", input.scratch_home);
    v.set("ALIAS", sim.meta.alias.as_str());
    // A distribution build names its versioned binary, so the job keeps the
    // exact build it was submitted with across a self-update.
    v.set("CACTUP", crate::freeze::frozen_cactup());

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
    use crate::args::Args;
    use crate::mdb::{Layer, Machine, Meta, ScriptKind};
    use crate::sim::SimulationMeta;
    use clap::Parser;
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

            [submitscripts]
            "default" = { queues = ["batch", "gpuq", "fillq", "gpucap", "gpudef"] }

            [runscripts]
            "default" = { queues = ["batch", "gpuq", "fillq", "gpucap", "gpudef"] }

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
            flavor = "default"
            thornlist = "installation-default.th"
            machine = "testbox"
            config-id = "c1"
            build-id = "b1"

            [runs-on]
            gpu = {gpu}
            queues = [{}]
            "#,
            compat.iter().map(|q| format!("\"{q}\"")).collect::<Vec<_>>().join(", ")
        );
        toml::from_str(&toml_text).unwrap()
    }

    fn test_fit(gpu: bool, compat: &[&str]) -> BinaryFit {
        BinaryFit::from_config(&test_cfg(gpu, compat))
    }

    /// The submit path, which most tests here exercise: `Placement::submit`.
    fn resolve_topology(
        f: &TopologyFlags,
        machine: &Machine,
        db: &Database,
        fit: Option<&BinaryFit>,
        force_queue: bool,
    ) -> Res<Topology> {
        super::resolve_topology(f, machine, db, fit, force_queue, Placement::submit(machine))
    }

    /// `super::resolve_topology` with an empty DB and no `--force-queue`.
    fn resolve_at(
        f: &TopologyFlags,
        machine: &Machine,
        fit: &BinaryFit,
        placement: Placement,
    ) -> Res<Topology> {
        super::resolve_topology(f, machine, &Database::new(), Some(fit), false, placement)
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
        let topo = resolve_topology(&flags(), &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
        assert_eq!(topo.queue, "batch");
        // No `name` override: the scheduler-facing name is the key itself.
        assert_eq!(topo.scheduler_queue, "batch");
        assert_eq!((topo.nodes, topo.cpus, topo.tpn, topo.tasks), (1, 1, 16, 16));
        assert_eq!(topo.total_wall, Walltime::parse("24:00:00").unwrap());
    }

    /// A flavor-built config takes `default-queue` from the flavor's current
    /// header, not its record, so an MDB edit reaches it without a rebuild;
    /// `queues`/`gpu` stay as recorded, and an `--optionlist` config keeps
    /// its recorded default.
    #[test]
    fn default_queue_is_read_live_for_flavor_built_configs() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("optionlists")).unwrap();
        std::fs::write(
            dir.path().join("optionlists/default.toml"),
            "[cactup.runs-on]\nqueues = [\"fillq\"]\ndefault-queue = \"fillq\"\n[options]\nVERSION = \"1\"\n",
        )
        .unwrap();
        let mut machine = test_machine();
        machine.dir = dir.path().to_owned();
        let mut cfg = test_cfg(false, &["batch", "fillq"]);
        let fit = BinaryFit::for_config(&machine, &cfg).unwrap();
        assert_eq!(fit.runs_on.default_queue.as_deref(), Some("fillq"));
        assert_eq!(fit.runs_on.queues, ["batch", "fillq"], "where the binary may run stays as recorded");

        // A live default the recorded binary may not use is ignored.
        let mut narrow = test_cfg(false, &["batch"]);
        narrow.runs_on.default_queue = None;
        assert_eq!(BinaryFit::for_config(&machine, &narrow).unwrap().runs_on.default_queue, None);

        cfg.optionlist_source = OptionlistSource::Optionlist("/x/my.cfg".to_owned());
        cfg.runs_on.default_queue = Some("batch".to_owned());
        let fit = BinaryFit::for_config(&machine, &cfg).unwrap();
        assert_eq!(fit.runs_on.default_queue.as_deref(), Some("batch"));
    }

    /// The queue chain with a flavor default: `-q` → `queue` knob → the
    /// flavor's `[cactup.runs-on] default-queue` → the machine default. A
    /// CPU flavor confined to `fillq` lands there with no flag, where the
    /// machine default (`batch`) would have been refused.
    #[test]
    fn queue_chain_knob_then_flavor_default_then_machine_default() {
        let machine = test_machine();
        let fit = |default_queue: Option<&str>| BinaryFit {
            universe: None,
            runs_on: RunsOn {
                queues: vec!["fillq".to_owned(), "batch".to_owned()],
                gpu: false,
                default_queue: default_queue.map(str::to_owned),
            },
            label: "sim".to_owned(),
        };
        let queue = |f: &TopologyFlags, db: &Database, fit: &BinaryFit| {
            resolve_topology(f, &machine, db, Some(fit), false).unwrap().queue
        };
        let db = Database::new();
        assert_eq!(queue(&flags(), &db, &fit(Some("fillq"))), "fillq");
        assert_eq!(queue(&flags(), &db, &fit(None)), "batch", "no flavor default: the machine's");
        let mut f = flags();
        f.queue = Some("batch".to_owned());
        assert_eq!(queue(&f, &db, &fit(Some("fillq"))), "batch", "-q wins");
        let mut db = Database::new();
        db.set_knob("queue", "batch".to_owned());
        assert_eq!(queue(&flags(), &db, &fit(Some("fillq"))), "batch", "a usable knob wins");
        // A knob naming a queue this binary may not use is passed over for
        // the flavor's default — unless --force-queue says to keep it.
        db.set_knob("queue", "gpuq".to_owned());
        assert_eq!(queue(&flags(), &db, &fit(Some("fillq"))), "fillq", "an unusable knob is skipped");
        let forced = resolve_topology(&flags(), &machine, &db, Some(&fit(Some("fillq"))), true).unwrap();
        assert_eq!(forced.queue, "gpuq", "--force-queue keeps the knob");
        // A build job has no binary fit: no flavor default, and the `queue`
        // knob (sims and tests only) is never read.
        let topo = resolve_topology(&flags(), &machine, &db, None, false).unwrap();
        assert_eq!(topo.queue, "batch");
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
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(true, &[])), false).unwrap();
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
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn, topo.cpus), (1, 2, 2, 24));

        // An explicit `-c 1` request overrides the machine default → full-node
        // single-threaded fill: tpn = floor(48/1) = 48.
        f.cpus = Some(1);
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
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
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn), (1, 1, 1));

        // Below the fill-the-node value but above 1.
        f.tasks = Some(3);
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn), (1, 3, 3));

        // Multi-node: the cap is per node, and never below the ranks a node holds.
        f.nodes = Some(2);
        f.tasks = Some(3);
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn), (2, 3, 2));

        // The fill-the-node default is untouched (tasks == nodes * tpn).
        f.nodes = Some(2);
        f.tasks = None;
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
        assert_eq!((topo.nodes, topo.tasks, topo.tpn), (2, 32, 16));

        // An explicit `--tpn` stays authoritative even when it exceeds `--tasks`.
        f.nodes = None;
        f.tasks = Some(1);
        f.tpn = Some(4);
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
        assert_eq!((topo.tasks, topo.tpn), (1, 4));
    }

    #[test]
    fn tasks_default_chain() {
        let machine = test_machine();
        let db = Database::new();
        let base = || resolve_topology(&flags(), &machine, &db, Some(&test_fit(false, &[])), false).unwrap();

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
            let mut topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
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
        let topo = |f: &TopologyFlags| resolve_topology(f, &machine, &db, Some(&cfg), false).unwrap();

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
        let err = |f: &TopologyFlags| resolve_topology(f, &machine, &db, Some(&cfg), false).unwrap_err().to_string();

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
        let t = resolve_topology(&f, &machine, &db, Some(&cfg), false).unwrap();
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
        assert!(resolve_topology(&f, &machine, &db, Some(&cfg), false).is_ok());

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
        let mut topo = resolve_topology(&f, &machine, &db, Some(&cfg), false).unwrap();
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
        let err = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--gpus-per-task"), "{err}");
        // `--gpu` alone is enough to make it meaningful again.
        f.gpu = true;
        assert_eq!(resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap().gpus_per_task, 2);
    }

    #[test]
    fn queue_and_gpu_guards() {
        let machine = test_machine();
        let db = Database::new();

        // compatible-queues mismatch refused (§4.4)…
        let cfg = test_fit(false, &["gpuq"]);
        let err = resolve_topology(&flags(), &machine, &db, Some(&cfg), false).unwrap_err().to_string();
        assert!(err.contains("--force-queue"), "{err}");
        // …unless forced.
        assert!(resolve_topology(&flags(), &machine, &db, Some(&cfg), true).is_ok());

        // GPU cross-check is one-directional (D12): a GPU binary in a non-GPU
        // context (default queue batch) is refused…
        let err = resolve_topology(&flags(), &machine, &db, Some(&test_fit(true, &[])), false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("GPU"), "{err}");
        // …unless forced.
        assert!(resolve_topology(&flags(), &machine, &db, Some(&test_fit(true, &[])), true).is_ok());
        // A non-GPU binary on a GPU queue is allowed (advisory only).
        let mut f = flags();
        f.queue = Some("gpuq".to_owned());
        let topo = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
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

            [submitscripts]
            "s" = { queues = ["host-only", "sing"] }

            [runscripts]
            "r" = { queues = ["host-only", "sing"] }


            [universes.et-sing]
            wrapper-argv = ["apptainer", "exec", "et.sif"]
            "#,
        )
        .unwrap();
        meta.validate("gq").unwrap();
        let machine =
            Machine { name: "gq".to_owned(), dir: PathBuf::from("/nonexistent"), layer: Layer::System, meta };
        let db = Database::new();

        let cfg_in = |universe: &str| -> BinaryFit {
            let cfg: ConfigMeta = toml::from_str(&format!(
                r#"
                name = "sim"
                flavor = "default"
                thornlist = "installation-default.th"
                machine = "gq"
                universe = "{universe}"
                config-id = "c1"
                build-id = "b1"
                "#
            ))
            .unwrap();
            BinaryFit::from_config(&cfg)
        };

        // A host build cannot be routed to the et-sing-gated queue — a hard
        // error even with --force-queue (unlike the compatible-queues guard).
        let mut f = flags();
        f.queue = Some("sing".to_owned());
        let err = resolve_topology(&f, &machine, &db, Some(&cfg_in("host")), true).unwrap_err().to_string();
        assert!(err.contains("build universe") && err.contains("\"sing\""), "{err}");

        // Each build flavor's default resolves to its sole compatible queue.
        assert_eq!(resolve_topology(&flags(), &machine, &db, Some(&cfg_in("host")), false).unwrap().queue, "host-only");
        assert_eq!(resolve_topology(&flags(), &machine, &db, Some(&cfg_in("et-sing")), false).unwrap().queue, "sing");
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
        let sing: crate::mdb::Universe = toml::from_str("run-env-setup = \"module load sing\"").unwrap();
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
        let topo = resolve_topology(&flags(), &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
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
            scratch_home: "/scratch/alice",
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
            "name=\"s\"\nflavor=\"default\"\nthornlist=\"t.th\"\n\
             machine=\"qbd\"\nconfig-id=\"c\"\nbuild-id=\"b\"\n[runs-on]\ngpu=true",
        )
        .unwrap();
        let fit = BinaryFit::from_config(&cfg);

        for (queue, gpus) in [("gpu2", 2), ("gpu4", 4)] {
            let mut f = flags();
            f.queue = Some(queue.to_owned());
            let t = resolve_topology(&f, &machine, &db, Some(&fit), false).unwrap();
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
        // bound on a derived tpn is a no-op here: a two-node rank count keeps
        // the per-node fill, on the two nodes it needs.
        let mut f = flags();
        f.queue = Some("gpu4".to_owned());
        f.tasks = Some(8);
        let t = resolve_topology(&f, &machine, &db, Some(&fit), false).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn, t.cpus, t.gpus_per_task), (2, 8, 4, 16, 1));
    }

    fn qbd() -> Machine {
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        );
        mdb.load("qbd").unwrap()
    }

    fn qbd_fit(gpu: bool) -> BinaryFit {
        let cfg: ConfigMeta = toml::from_str(&format!(
            "name=\"s\"\nflavor=\"default\"\nthornlist=\"t.th\"\n\
             machine=\"qbd\"\nconfig-id=\"c\"\nbuild-id=\"b\"\n[runs-on]\ngpu={gpu}"
        ))
        .unwrap();
        BinaryFit::from_config(&cfg)
    }

    /// `(nodes, tasks, tpn)` for `-q queue -T tasks -c cpus` on qbd, placed
    /// as `placement`.
    fn qbd_layout(queue: &str, tasks: u32, cpus: u32, placement: Placement) -> Res<(u32, u32, u32)> {
        qbd_topo(queue, Some(tasks), cpus, placement).map(|t| (t.nodes, t.tasks, t.tpn))
    }

    fn qbd_topo(queue: &str, tasks: Option<u32>, cpus: u32, placement: Placement) -> Res<Topology> {
        let mut f = flags();
        f.queue = Some(queue.to_owned());
        f.tasks = tasks;
        f.cpus = Some(cpus);
        let machine = qbd();
        let fit = qbd_fit(machine.meta.queue(queue).unwrap().gpu);
        resolve_at(&f, &machine, &fit, placement)
    }

    /// The QB4 report: `-T 256 -c 1` on a 64-core partition went out as one
    /// node, which Slurm refuses. Without `-n` a submitted job gets as many
    /// nodes as its ranks need.
    #[test]
    fn a_submit_without_nodes_gets_as_many_as_its_tasks_need() {
        let s = Placement::Scheduled;
        assert_eq!(Placement::submit(&qbd()), s, "qbd's jobs go to Slurm");
        assert_eq!(qbd_layout("checkpt", 256, 1, s).unwrap(), (4, 256, 64));
        // Not a multiple of a node: the ranks spread evenly over 2 nodes.
        assert_eq!(qbd_layout("checkpt", 100, 1, s).unwrap(), (2, 100, 50));
        // Threads count: 64 ranks of 2 CPUs fill 2 nodes.
        assert_eq!(qbd_layout("workq", 64, 2, s).unwrap(), (2, 64, 32));
        // A rank count that fits on one node stays on one.
        assert_eq!(qbd_layout("checkpt", 64, 1, s).unwrap(), (1, 64, 64));
        assert_eq!(qbd_layout("checkpt", 3, 1, s).unwrap(), (1, 3, 3));
        // An uneven CPU split: 21 ranks of 3 CPUs fit a 64-core node, so 64
        // of them need 4 nodes, not ceil(64 * 3 / 64) = 3.
        assert_eq!(qbd_layout("checkpt", 64, 3, s).unwrap(), (4, 64, 16));

        // An explicit --tpn sets the per-node count the nodes follow from.
        let machine = qbd();
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.tasks = Some(256);
        f.tpn = Some(32);
        let t = resolve_at(&f, &machine, &qbd_fit(false), s)
            .unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn), (8, 256, 32));
    }

    /// The node size comes from the queue when it has its own
    /// `max-cpus-per-node`, else from the machine.
    #[test]
    fn derived_nodes_use_the_queues_cpus_over_the_machines() {
        let machine = test_machine();
        let db = Database::new();
        let mut f = flags();
        f.tasks = Some(64);
        f.cpus = Some(1);
        // batch inherits [hardware]'s 16 CPUs: 4 nodes.
        let t = resolve_topology(&f, &machine, &db, Some(&test_fit(false, &[])), false).unwrap();
        assert_eq!((t.nodes, t.tpn), (4, 16));
        // gpuq has 32 of its own (and declares no GPUs to bound it): 2 nodes.
        f.queue = Some("gpuq".to_owned());
        let t = resolve_topology(&f, &machine, &db, Some(&test_fit(true, &[])), false).unwrap();
        assert_eq!((t.nodes, t.tpn), (2, 32));
    }

    /// On a GPU run a node holds no more ranks than its GPUs feed, so that is
    /// what the node count follows from — and with `--tasks` too, now that
    /// the nodes absorb the bound instead of the GPU ceiling refusing it.
    #[test]
    fn derived_nodes_on_a_gpu_run_follow_the_gpus() {
        let s = Placement::Scheduled;
        // gpu2: 2 GPUs a node; the queue default of 32 CPUs a rank agrees.
        assert_eq!(qbd_layout("gpu2", 8, 32, s).unwrap(), (4, 8, 2));
        // -c 8 leaves room for 8 ranks by CPU, but only 2 GPUs: still 4 nodes.
        assert_eq!(qbd_layout("gpu2", 8, 8, s).unwrap(), (4, 8, 2));
        // gpu4: 4 GPUs a node.
        assert_eq!(qbd_layout("gpu4", 8, 8, s).unwrap(), (2, 8, 4));
        // Two GPUs a rank on gpu4: 2 ranks a node.
        let machine = qbd();
        let mut f = flags();
        f.queue = Some("gpu4".to_owned());
        f.tasks = Some(8);
        f.gpus_per_task = Some(2);
        let t = resolve_at(&f, &machine, &qbd_fit(true), s)
            .unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn, t.gpus_per_task), (4, 8, 2, 2));
    }

    /// An explicit `-n` always wins; one the ranks cannot fit on is refused
    /// before anything is submitted, naming the node count that would.
    #[test]
    fn explicit_nodes_win_and_an_impossible_count_is_refused() {
        let machine = qbd();
        let fit = qbd_fit(false);
        let resolve = |nodes: u32, tasks: u32, tpn: Option<u32>, placement: Placement| {
            let mut f = flags();
            f.queue = Some("checkpt".to_owned());
            f.nodes = Some(nodes);
            f.tasks = Some(tasks);
            f.tpn = tpn;
            f.cpus = Some(1);
            resolve_at(&f, &machine, &fit, placement)
                .map(|t| (t.nodes, t.tasks, t.tpn))
                .map_err(|e| e.to_string())
        };
        let s = Placement::Scheduled;
        // More nodes than needed: as asked, ranks spread evenly.
        assert_eq!(resolve(8, 256, None, s).unwrap(), (8, 256, 32));
        assert_eq!(resolve(4, 256, None, s).unwrap(), (4, 256, 64));

        // The QB4 case with -n 1: refused, not handed to Slurm.
        let err = resolve(1, 256, None, s).unwrap_err();
        assert!(err.contains("256 tasks of 1 CPU each do not fit on 1 node"), "{err}");
        assert!(
            err.contains("has room for 64 of them (64 CPUs per node at 1 CPU per task)")
                && err.contains("ask for -n 4"),
            "{err}"
        );
        let err = resolve(3, 256, None, s).unwrap_err();
        assert!(err.contains("on 3 nodes") && err.contains("-n 4"), "{err}");
        // An explicit --tpn that cannot hold the ranks contradicts itself.
        let err = resolve(2, 256, Some(32), s).unwrap_err();
        assert!(
            err.contains("256 tasks do not fit on 2 nodes at 32 tasks per node") && err.contains("-n 8"),
            "{err}"
        );
        // An explicit --tpn above the CPU room is still authoritative.
        assert_eq!(resolve(2, 256, Some(128), s).unwrap(), (2, 256, 128));

        // A run in an allocation or on this host refuses nothing: what fits
        // there is not cactup's to know.
        assert!(resolve(1, 256, None, Placement::Allocation { nodes: None }).is_ok());
        assert!(resolve(1, 256, None, Placement::Allocation { nodes: Some(1) }).is_ok());
        assert!(resolve(1, 256, None, Placement::Host).is_ok());
    }

    /// On a GPU queue the node count an explicit `-n` is told to ask for is
    /// the one the GPUs allow, and the GPU ceiling gives the same advice.
    #[test]
    fn a_refused_node_count_names_one_the_gpus_allow() {
        let machine = qbd();
        let fit = qbd_fit(true);
        let err = |nodes: u32, tasks: u32, placement: Placement| {
            let mut f = flags();
            f.queue = Some("gpu2".to_owned());
            f.nodes = Some(nodes);
            f.tasks = Some(tasks);
            f.cpus = Some(1);
            resolve_at(&f, &machine, &fit, placement).unwrap_err().to_string()
        };
        // 128 ranks do not fit one node's 64 CPUs; at one GPU each on 2-GPU
        // nodes they need 64 nodes, not the 2 the CPUs alone suggest.
        let e = err(1, 128, Placement::Scheduled);
        assert!(e.contains("has room for 2 of them (2 GPUs per node at 1 GPU per task)"), "{e}");
        assert!(e.contains("ask for -n 64,"), "{e}");
        // 4 ranks fit one node's CPUs but not its GPUs: the ceiling refuses,
        // and names the node count too.
        let e = err(1, 4, Placement::Scheduled);
        assert!(e.contains("needs 4 GPUs per node"), "{e}");
        assert!(e.contains("ask for -n 2, or leave out -n"), "{e}");
        // So it does inside an allocation (which refuses nothing else)…
        let e = err(1, 4, Placement::Allocation { nodes: None });
        assert!(e.contains("ask for -n 2"), "{e}");
        // …but not on a single host, where more nodes are no answer.
        let e = err(1, 4, Placement::Host);
        assert!(e.contains("needs 4 GPUs per node") && !e.contains("-n 2"), "{e}");
    }

    /// Zero is no count: the parser refuses it, and a zero that reaches the
    /// layout anyway (from the MDB's `[build]` table, or a script) is taken
    /// as 1 rather than divided by.
    #[test]
    fn zero_counts_are_refused_or_taken_as_one() {
        for flag in ["-n", "-T", "-t", "-c"] {
            let argv = ["cactup", "sim", "submit", "s", flag, "0"];
            let err = Args::try_parse_from(argv).unwrap_err().to_string();
            assert!(err.contains("0"), "{flag}: {err}");
        }
        for flag in ["-n", "-T", "-c"] {
            let argv = ["cactup", "import", "s.sim", flag, "0"];
            assert!(Args::try_parse_from(argv).is_err(), "import {flag} 0");
        }
        assert!(Args::try_parse_from(["cactup", "sim", "submit", "s", "-n", "1", "-t", "1"]).is_ok());

        let machine = qbd();
        let fit = qbd_fit(false);
        for placement in [Placement::Scheduled, Placement::Allocation { nodes: None }, Placement::Host] {
            let mut f = flags();
            f.queue = Some("checkpt".to_owned());
            f.nodes = Some(1);
            f.tasks = Some(4);
            f.tpn = Some(0);
            // The reported crash: -n 1 -T 4 -t 0. At the 1 task per node it
            // stands for, a scheduled job is refused; elsewhere it runs.
            let t = resolve_at(&f, &machine, &fit, placement);
            match placement {
                Placement::Scheduled => assert!(t.unwrap_err().to_string().contains("ask for -n 4")),
                _ => {
                    assert_eq!(t.map(|t| (t.nodes, t.tasks, t.tpn)).unwrap(), (1, 4, 1), "{placement:?}")
                }
            }
            f.nodes = Some(0);
            f.tasks = Some(0);
            f.tpn = None;
            f.cpus = Some(0);
            let t = resolve_at(&f, &machine, &fit, placement).unwrap();
            assert_eq!((t.nodes, t.tasks, t.tpn, t.cpus), (1, 1, 1, 1), "{placement:?}");
        }
    }

    /// Huge counts saturate or compare wide; they never overflow.
    #[test]
    fn huge_counts_do_not_overflow() {
        let machine = qbd();
        let fit = qbd_fit(false);
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.nodes = Some(100_000);
        f.tasks = Some(5);
        f.tpn = Some(100_000);
        let t = resolve_at(&f, &machine, &fit, Placement::Scheduled).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn), (100_000, 5, 100_000));
        // No --tasks: nodes × the fill saturates.
        f.tasks = None;
        f.nodes = Some(u32::MAX);
        let t = resolve_at(&f, &machine, &fit, Placement::Scheduled).unwrap();
        assert_eq!(t.tasks, u32::MAX);
        // GPUs per task × tasks per node, on a GPU queue.
        let mut f = flags();
        f.queue = Some("gpu4".to_owned());
        f.tpn = Some(u32::MAX);
        f.gpus_per_task = Some(u32::MAX);
        let r = resolve_at(&f, &machine, &qbd_fit(true), Placement::Scheduled);
        assert!(r.unwrap_err().to_string().contains("GPUs per node"));
    }

    /// A `max-cpus-per-node` detected on this host is a login node's: no node
    /// count is derived from it, and no layout refused on it.
    #[test]
    fn a_detected_node_size_derives_and_refuses_nothing() {
        let mut machine = qbd();
        machine.meta.hardware.max_cpus_per_node_detected = true;
        let fit = qbd_fit(false);
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.tasks = Some(256);
        f.cpus = Some(1);
        let resolve = |f: &TopologyFlags, machine: &Machine| {
            resolve_at(f, machine, &fit, Placement::Scheduled)
        };
        let t = resolve(&f, &machine).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn, t.derived_nodes), (1, 256, 64, None));
        f.nodes = Some(1);
        assert!(resolve(&f, &machine).is_ok(), "not refused on a detected size");
        // A queue declaring its own size is declared, whatever [hardware] says.
        machine.meta.queues.get_mut("checkpt").unwrap().max_cpus_per_node = Some(64);
        f.nodes = None;
        let t = resolve(&f, &machine).unwrap();
        assert_eq!((t.nodes, t.tpn), (4, 64));
    }

    /// Inside an allocation whose size its environment gives, a missing `-n`
    /// is that size — never more nodes than the ranks need, and never more
    /// than it has, whatever the queue's node size says.
    #[test]
    fn a_run_inside_an_allocation_uses_its_nodes() {
        let one = Placement::Allocation { nodes: Some(1) };
        let four = Placement::Allocation { nodes: Some(4) };
        // `salloc` of one big node, a 64-core default queue: srun -N 1.
        assert_eq!(qbd_layout("checkpt", 128, 1, one).unwrap(), (1, 128, 128));
        // Four nodes, ranks for two: two.
        assert_eq!(qbd_layout("checkpt", 128, 1, four).unwrap(), (2, 128, 64));
        // No ranks asked for: all four nodes, filled.
        let t = qbd_topo("checkpt", None, 1, four).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn), (4, 256, 64));
        // A testsuite's 2 ranks stay on one of them.
        let machine = qbd();
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.cpus = Some(1);
        let mut t =
            resolve_at(&f, &machine, &qbd_fit(false), four).unwrap();
        apply_tasks_default(&mut t, &f, None, Some(2));
        assert_eq!((t.nodes, t.tasks, t.tpn), (1, 2, 2));
        // A script's 1000 ranks in a 4-node allocation: 4 nodes, packed.
        let mut t =
            resolve_at(&f, &machine, &qbd_fit(false), four).unwrap();
        apply_tasks_default(&mut t, &f, Some(1000), None);
        assert_eq!((t.nodes, t.tasks, t.tpn), (4, 1000, 250));
        // Size unknown: derived as for a submit.
        let unknown = Placement::Allocation { nodes: None };
        assert_eq!(qbd_layout("checkpt", 256, 1, unknown).unwrap(), (4, 256, 64));
    }

    /// A testsuite's fallback of 2 ranks is no rank count to derive nodes
    /// from: rostam's `spark` nodes hold one GPU, and a no-flag test run
    /// stays on one node as it always did. A script's own `tasks` does derive.
    #[test]
    fn a_testsuite_fallback_stays_on_one_node() {
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        );
        let machine = mdb.load("rostam").unwrap();
        let cfg: ConfigMeta = toml::from_str(
            "name=\"s\"\nflavor=\"arm-cuda-native\"\nthornlist=\"t.th\"\nmachine=\"rostam\"\n\
             config-id=\"c\"\nbuild-id=\"b\"\nuniverse=\"arm-conda\"\n[runs-on]\ngpu=true",
        )
        .unwrap();
        let fit = BinaryFit::from_config(&cfg);
        let mut f = flags();
        f.queue = Some("spark".to_owned());
        for placement in [Placement::Scheduled, Placement::Allocation { nodes: Some(4) }] {
            let mut t = resolve_at(&f, &machine, &fit, placement).unwrap();
            apply_tasks_default(&mut t, &f, None, Some(2));
            assert_eq!((t.nodes, t.tasks, t.tpn), (1, 2, 1), "{placement:?}");
            assert_eq!(t.nodes_note(), None);
            let mut t = resolve_at(&f, &machine, &fit, placement).unwrap();
            apply_tasks_default(&mut t, &f, Some(2), Some(2));
            assert_eq!((t.nodes, t.tasks, t.tpn), (2, 2, 1), "{placement:?}");
        }
    }

    /// Inside a known allocation an explicit `--tpn` sets the ranks per node
    /// and the nodes follow from it, past the allocation's size if need be
    /// (it is not refused there either); and the allocation's own size is the
    /// default even where the node size is not declared.
    #[test]
    fn an_allocation_keeps_an_explicit_tpn_and_its_size_without_a_node_size() {
        let two = Placement::Allocation { nodes: Some(2) };
        let machine = qbd();
        let fit = qbd_fit(false);
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.tasks = Some(256);
        f.tpn = Some(32);
        let t = resolve_at(&f, &machine, &fit, two).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn), (8, 256, 32));

        let mut machine = qbd();
        machine.meta.hardware.max_cpus_per_node_detected = true;
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.cpus = Some(1);
        let t = resolve_at(&f, &machine, &fit, two).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn), (2, 128, 64), "all of it, filled");
        // With ranks: still its size — capping by ranks needs a declared room.
        f.tasks = Some(64);
        let t = resolve_at(&f, &machine, &fit, two).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn), (2, 64, 32));
        // Outside an allocation a detected size derives nothing.
        let t = resolve_at(&f, &machine, &fit, Placement::Scheduled).unwrap();
        assert_eq!(t.nodes, 1);
        // An explicit --tpn needs no node size to derive from.
        f.tpn = Some(16);
        let t = resolve_at(&f, &machine, &fit, Placement::Scheduled).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn), (4, 64, 16));
    }

    /// More GPUs per task than a node has is said as that, with no `-n`
    /// advice: no node count can help.
    #[test]
    fn too_many_gpus_per_task_is_said_first() {
        let machine = qbd();
        let fit = qbd_fit(true);
        let mut f = flags();
        f.queue = Some("gpu4".to_owned());
        f.gpus_per_task = Some(5);
        for (nodes, tasks) in [(None, None), (Some(1), Some(4)), (None, Some(4))] {
            f.nodes = nodes;
            f.tasks = tasks;
            let e = resolve_at(&f, &machine, &fit, Placement::Scheduled).unwrap_err().to_string();
            assert!(e.contains("needs 5 GPUs per node for even one task"), "{e}");
            assert!(e.contains("has 4"), "{e}");
            assert!(e.contains("--gpus-per-task 4 or lower") && !e.contains("-n "), "{e}");
        }
    }

    /// Counts of one read as one.
    #[test]
    fn notes_and_refusals_say_one_in_the_singular() {
        let machine = qbd();
        let fit = qbd_fit(false);
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.tasks = Some(4);
        f.tpn = Some(1);
        let t = resolve_at(&f, &machine, &fit, Placement::Scheduled).unwrap();
        let note = t.nodes_note().unwrap();
        assert!(note.contains(": 1 task per node (--tpn) for 4 tasks"), "{note}");
        f.nodes = Some(2);
        let e = resolve_at(&f, &machine, &fit, Placement::Scheduled).unwrap_err().to_string();
        assert!(e.contains("4 tasks do not fit on 2 nodes at 1 task per node"), "{e}");
        // gpu2 at 1 GPU per task: "1 GPU per task".
        let note = qbd_topo("gpu2", Some(8), 1, Placement::Scheduled).unwrap().nodes_note().unwrap();
        assert!(note.contains("2 GPUs per node at 1 GPU per task"), "{note}");
    }

    /// A derived node count above 1 is said, with what decided it.
    #[test]
    fn a_derived_node_count_is_announced_with_its_reason() {
        let s = Placement::Scheduled;
        let note = |queue, tasks, cpus, placement| {
            qbd_topo(queue, tasks, cpus, placement).unwrap().nodes_note()
        };
        assert_eq!(
            note("gpu2", Some(100), 1, s).as_deref(),
            Some(
                "Asking for 50 nodes: at most 2 tasks per node \
                 (the queue's 2 GPUs per node at 1 GPU per task) for 100 tasks; pass -n to choose"
            )
        );
        assert_eq!(
            note("checkpt", Some(256), 1, s).as_deref(),
            Some(
                "Asking for 4 nodes: at most 64 tasks per node \
                 (the queue's 64 CPUs per node at 1 CPU per task) for 256 tasks; pass -n to choose"
            )
        );
        assert_eq!(
            note("checkpt", None, 1, Placement::Allocation { nodes: Some(4) }).as_deref(),
            Some("Using 4 nodes, all of this job allocation's, for 256 tasks; pass -n to choose")
        );
        let t = note("checkpt", Some(256), 1, Placement::Allocation { nodes: None }).unwrap();
        assert!(t.starts_with("Using 4 nodes: at most 64 tasks per node"), "{t}");
        // Quiet: one node, a single host, an explicit -n.
        assert_eq!(note("checkpt", Some(64), 1, s), None);
        assert_eq!(note("checkpt", Some(256), 1, Placement::Host), None);
        let machine = qbd();
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.tasks = Some(256);
        f.cpus = Some(1);
        f.nodes = Some(4);
        let t = resolve_at(&f, &machine, &qbd_fit(false), s).unwrap();
        assert_eq!(t.nodes_note(), None);
        // --tpn decides it.
        f.nodes = None;
        f.tpn = Some(32);
        let t = resolve_at(&f, &machine, &qbd_fit(false), s).unwrap();
        assert_eq!(
            t.nodes_note().as_deref(),
            Some("Asking for 8 nodes: 32 tasks per node (--tpn) for 256 tasks; pass -n to choose")
        );
    }

    /// A run on this host (a login node, or a machine without a batch
    /// system) keeps one node; inside an allocation the nodes are derived as
    /// for a submit.
    #[test]
    fn nodes_are_derived_only_where_ranks_can_span_nodes() {
        assert_eq!(qbd_layout("checkpt", 256, 1, Placement::Host).unwrap(), (1, 256, 64));
        let unknown = Placement::Allocation { nodes: None };
        assert_eq!(qbd_layout("checkpt", 256, 1, unknown).unwrap(), (4, 256, 64));

        // A `job-ids = "pids"` machine's submit backgrounds the script here.
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        );
        let omnia = mdb.load("omnia").unwrap();
        assert_eq!(Placement::submit(&omnia), Placement::Host);
        assert_eq!(Placement::foreground(&omnia), Placement::Host);
        // A batch machine's foreground run outside an allocation is on this
        // host (qbd declares allocation-env; the test env has none of it set,
        // or this is a compute node and the answer is Allocation).
        let inside = qbd().meta.in_allocation() == Some(true);
        let nodes = qbd().meta.allocation_nodes();
        let expect = if inside { Placement::Allocation { nodes } } else { Placement::Host };
        assert_eq!(Placement::foreground(&qbd()), expect);
    }

    /// A script's `tasks` setting is a rank count like `--tasks`: where nodes
    /// are derived, they follow from it.
    #[test]
    fn a_script_tasks_default_derives_nodes_too() {
        let machine = qbd();
        let mut f = flags();
        f.queue = Some("checkpt".to_owned());
        f.cpus = Some(1);
        let fit = qbd_fit(false);
        let at = |placement, script_tasks| {
            let mut t =
                resolve_at(&f, &machine, &fit, placement).unwrap();
            apply_tasks_default(&mut t, &f, Some(script_tasks), None);
            (t.nodes, t.tasks, t.tpn)
        };
        assert_eq!(at(Placement::Scheduled, 256), (4, 256, 64));
        assert_eq!(at(Placement::Scheduled, 100), (2, 100, 50));
        assert_eq!(at(Placement::Scheduled, 2), (1, 2, 2));
        assert_eq!(at(Placement::Host, 256), (1, 256, 64));
    }

    /// The QB4 report, rendered through qbd's own CPU submit script: the job
    /// that went out as `-N 1 -n 256` (what a single host still gets) now goes
    /// out on the 4 nodes it needs.
    #[test]
    fn qbd_submit_script_asks_for_the_nodes_the_tasks_need() {
        let tmp = tempfile::tempdir().unwrap();
        let machine = qbd();
        let sim = Simulation {
            name: "bhns".to_owned(),
            dir: tmp.path().join("bhns"),
            meta: SimulationMeta {
                parfile: "bhns.par".to_owned(),
                configuration: "bhns".to_owned(),
                simulation_id: "simulation-bhns-x".to_owned(),
                alias: "bhns".to_owned(),
                sourcedir: PathBuf::from("/work/u/bhns/Cactus"),
                ..Default::default()
            },
        };
        let render = |placement| {
            let mut f = flags();
            f.queue = Some("checkpt".to_owned());
            f.tasks = Some(256);
            f.cpus = Some(1);
            let fit = qbd_fit(false);
            let topo =
                resolve_at(&f, &machine, &fit, placement).unwrap();
            let mut vars = assemble(&RestartVarsInput {
                sim: &sim,
                machine: &machine,
                topo: &topo,
                sim_home: tmp.path(),
                restart_id: 0,
                job_wall: Walltime::parse("24:00:00").unwrap(),
                checkpt_buffer: Walltime(3600),
                chained_job_id: "",
                hostname: "qbd1",
                user: "u",
                email: "",
                run_universe: None,
                debug: false,
                scratch_home: "",
            })
            .unwrap();
            vars.set("ALLOCATION", "alloc");
            let script = crate::sim::start::generate_script(
                &machine,
                ScriptKind::Submit,
                "cpu",
                &vars,
                Phase::Submit,
                None,
            )
            .unwrap();
            let lines: Vec<String> = script
                .lines()
                .filter(|l| l.starts_with("#SBATCH -N") || l.starts_with("#SBATCH --ntasks-per-node"))
                .map(str::to_owned)
                .collect();
            eprintln!("{placement:?}:\n{}", lines.join("\n"));
            lines
        };
        assert_eq!(render(Placement::Host), ["#SBATCH -N 1 -n 256", "#SBATCH --ntasks-per-node=64"]);
        assert_eq!(
            render(Placement::Scheduled),
            ["#SBATCH -N 4 -n 256", "#SBATCH --ntasks-per-node=64"]
        );
    }

    /// db1 (Deep Bayou) has 2 GPUs per node and fills a node as 2 ranks of 24
    /// cores, one GPU each, so the GPU bound on a derived tpn and the §8.5
    /// ceiling leave its usual layouts alone: the CPU rule fills the node,
    /// with or without explicit rank flags. Only a layout that puts more
    /// ranks on a node than it has GPUs is refused.
    #[test]
    fn db1_layouts_are_untouched_by_the_gpu_bound() {
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent-user-mdb"),
        );
        let machine = mdb.load("db1.hpc.lsu.edu").unwrap();
        let db = Database::new();
        let cfg: ConfigMeta = toml::from_str(
            "name=\"s\"\nflavor=\"default\"\nthornlist=\"t.th\"\n\
             machine=\"db1.hpc.lsu.edu\"\nconfig-id=\"c\"\nbuild-id=\"b\"\n[runs-on]\ngpu=true",
        )
        .unwrap();
        let fit = BinaryFit::from_config(&cfg);
        let hw = machine.meta.effective_hardware("gpu").unwrap();
        assert_eq!(hw.max_gpus_per_node, Some(2));

        // No flags: 48 cores at 24/task = 2 ranks, one GPU each.
        let t = resolve_topology(&flags(), &machine, &db, Some(&fit), false).unwrap();
        assert_eq!((t.tasks, t.tpn, t.cpus, t.gpus_per_task), (2, 2, 24, 1));

        // Explicit rank counts resolve as they always did: 2 per node, on
        // as many nodes as that takes.
        let mut f = flags();
        f.tasks = Some(4);
        let t = resolve_topology(&f, &machine, &db, Some(&fit), false).unwrap();
        assert_eq!((t.nodes, t.tasks, t.tpn), (2, 4, 2));

        // 4 ranks of 12 cores on one node would put 4 ranks on its 2 GPUs.
        let mut f = flags();
        f.cpus = Some(12);
        f.tpn = Some(4);
        assert!(resolve_topology(&f, &machine, &db, Some(&fit), false).is_err());
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
            "name=\"s\"\nflavor=\"default\"\nthornlist=\"t.th\"\n\
             machine=\"omnia\"\nconfig-id=\"c\"\nbuild-id=\"b\"\n[runs-on]\ngpu=true",
        )
        .unwrap();
        let fit = BinaryFit::from_config(&cfg);
        let hw = machine.meta.effective_hardware("local").unwrap();
        assert_eq!(
            (hw.max_cpus_per_node, hw.default_cpus_per_task, hw.max_gpus_per_node),
            (Some(72), Some(8), Some(1))
        );

        // No flags: one rank on its one GPU, 8 of the node's 72 cores, the
        // rest left for whoever else is on the box.
        let t = resolve_topology(&flags(), &machine, &db, Some(&fit), false).unwrap();
        assert_eq!((t.tasks, t.tpn, t.cpus, t.gpus_per_task), (1, 1, 8, 1));
        assert!(t.tpn * t.cpus < hw.max_cpus_per_node.unwrap(), "the whole point: idle cores");

        // -c still fills the node on request, and still lands on one rank.
        let mut f = flags();
        f.cpus = Some(72);
        let t = resolve_topology(&f, &machine, &db, Some(&fit), false).unwrap();
        assert_eq!((t.tasks, t.tpn, t.cpus), (1, 1, 72));

        // An explicit --tpn is authoritative, so it reaches the ceiling and is
        // refused there rather than being silently bounded.
        let mut f = flags();
        f.tpn = Some(4);
        let err = resolve_topology(&f, &machine, &db, Some(&fit), false).unwrap_err().to_string();
        assert!(err.contains("needs 4 GPUs per node") && err.contains("--tpn/--tasks"), "{err}");

        // An explicit --tasks is a rank count too: on this no-batch single
        // node (`job-ids = "pids"`, so no nodes are derived) every rank lands
        // on the box, so it must not slip past the
        // ceiling on the strength of a quietly-bounded derived tpn (which
        // would let 10 ranks fight over the one MI210 via `mpirun -np 10`).
        let mut f = flags();
        f.tasks = Some(10);
        let err = resolve_topology(&f, &machine, &db, Some(&fit), false).unwrap_err().to_string();
        assert!(err.contains("needs 9 GPUs per node") && err.contains("--tpn/--tasks"), "{err}");

        // …while a --tasks the GPU budget can honor still resolves.
        let mut f = flags();
        f.tasks = Some(1);
        let t = resolve_topology(&f, &machine, &db, Some(&fit), false).unwrap();
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
                for script_name in machine.meta.scripts(kind).routes.keys() {
                    let script = machine.script_path(kind, script_name).unwrap();
                    if script.python {
                        continue; // .py scripts read globals, not @NAME@ tokens
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
                        Some(&test_fit(false, &[])),
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
                        panic!("{name} {kind:?} {script_name} failed to substitute: {e:#}")
                    });
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "expected at least one template using @GPUS_PER_TASK@");
    }
}



