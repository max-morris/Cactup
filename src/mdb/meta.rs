//! `<machine>/meta.toml` model, load-time validation, and script/queue/
//! universe resolution (spec §4.2, §4.4, §4.8, §11.2).
//!
//! Every table below is `deny_unknown_fields`: a key cactup does not model is
//! a hard parse error naming it, so a typo (`max-cpu-per-node`) fails loudly
//! at load instead of silently doing nothing. simfactory's informational-only
//! keys were therefore either modeled here or dropped from the ported
//! machines — a fact worth keeping is a TOML comment, not a dead key.

use crate::template::{Syntax, VarSet};
use crate::walltime::Walltime;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use indexmap::IndexMap;
use serde::Deserialize;

/// Effective walltime ceiling when neither the queue nor the machine sets one:
/// one year (§4.2).
const FALLBACK_MAX_WALLTIME: Walltime = Walltime(365 * 86400);

/// The reserved universe name for bare host execution (§4.8): always a legal
/// reference, resolving to `[universes.host]` when declared, else to the
/// implicit identity universe.
pub const HOST_UNIVERSE: &str = "host";

/// The implicit `host` universe (§4.8): identity wrapping, machine-level env.
static IMPLICIT_HOST: Universe = Universe {
    wrapper_argv: None,
    wrapper: None,
    environment: Environment {
        env_setup: None,
        build_env_setup: None,
        submit_env_setup: None,
        run_env_setup: None,
        provision: None,
        build_provision: None,
        submit_provision: None,
        run_provision: None,
    },
};

/// The three execution phases a `<phase>-env-setup` or `<phase>-provision`
/// can target (§4.2, §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Build,
    Submit,
    Run,
}

impl Phase {
    /// The phase's own provisioning key, as a generated script names it.
    pub fn provision_key(&self) -> &'static str {
        match self {
            Phase::Build => "build-provision",
            Phase::Submit => "submit-provision",
            Phase::Run => "run-provision",
        }
    }
}

/// The per-queue script kinds (§4.4). `BuildSubmit` is opt-in: a machine
/// that never queues a build declares no `[buildsubmitscripts]`, so it must
/// validate and display as cleanly absent everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    Submit,
    Run,
    BuildSubmit,
}

impl ScriptKind {
    /// The script directory stem, which is also the kind's `meta.toml` table.
    pub fn dir_name(&self) -> &'static str {
        match self {
            ScriptKind::Submit => "submitscripts",
            ScriptKind::Run => "runscripts",
            ScriptKind::BuildSubmit => "buildsubmitscripts",
        }
    }

    fn table_name(&self) -> &'static str {
        self.dir_name()
    }

    /// What one script of this kind is called in a message.
    pub fn noun(&self) -> &'static str {
        match self {
            ScriptKind::Submit => "submit script",
            ScriptKind::Run => "run script",
            ScriptKind::BuildSubmit => "build-submit script",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Meta {
    pub machine: MachineInfo,
    #[serde(default)]
    pub paths: Paths,
    #[serde(default)]
    pub hardware: Hardware,
    #[serde(default)]
    pub build: Build,
    #[serde(default)]
    pub environment: Environment,
    #[serde(default)]
    pub scheduler: Scheduler,
    #[serde(default)]
    pub queues: IndexMap<String, Queue>,
    /// Script routes per kind: which script file serves which queue.
    // §4.4
    #[serde(default = "ScriptTable::submit", deserialize_with = "ScriptTable::de_submit")]
    pub submitscripts: ScriptTable,
    #[serde(default = "ScriptTable::run", deserialize_with = "ScriptTable::de_run")]
    pub runscripts: ScriptTable,
    #[serde(default = "ScriptTable::build_submit", deserialize_with = "ScriptTable::de_build_submit")]
    pub buildsubmitscripts: ScriptTable,
    #[serde(default)]
    pub universes: IndexMap<String, Universe>,
    /// cactup-written metadata; today only the `machine create --from-existing`
    /// provenance record.
    // §4.7
    #[serde(default)]
    pub cactup: CactupMeta,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct MachineInfo {
    pub name: Option<String>,
    pub nickname: Option<String>,
    /// personal | experimental | production | storage | outdated.
    pub status: Option<String>,
    pub hostname: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
    /// Purely informational (as are `location`/`description`) — modeled so a
    /// ported machine can keep the fact rather than lose it to the strict
    /// schema.
    pub webpage: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Paths {
    /// Default install prefix; fallback `~/.cactup/cacti`.
    // §4.2
    pub install_home: Option<String>,
    /// Simulation-output root; fallback `~/.cactup/simulations`.
    // §4.2, §8.1
    pub simulation_home: Option<String>,
    /// Testsuite-output root; fallback `~/.cactup/tests`.
    // §4.2, §11.5
    pub test_home: Option<String>,
    /// Exposed to scripts as @SCRATCH_HOME@ (empty when unset); otherwise
    /// unused by cactup itself.
    pub scratch_home: Option<String>,
    /// Where the build cache keeps its objects; fallback
    /// `<install-home>/.cactup-build-cache`, beside the installations whose builds
    /// fill it. For a site whose builds belong somewhere else.
    // §18.7
    pub build_cache_home: Option<String>,
}

// §4.2, §8.5 (the layout rules these keys feed).
/// The hardware keys cactup feeds to submit and run scripts. They split along
/// the **availability vs. request** axis: the `max-` keys are *availability*
/// facts (what a node/queue physically has), while the process-layout flags
/// (`--tasks`, `--cpus`, …) are the *request*.
///
/// - `max-cpus-per-node` (simfactory's `ppn`; the CPUs/cores available per node
///   — an availability fact, **not** MPI ranks; it feeds the job layout and
///   @MAX_CPUS_PER_NODE@),
/// - `default-cpus-per-task` (simfactory's `num-threads`; the request-side
///   default for `CPUS_PER_TASK` when `--cpus` is omitted),
/// - `max-gpus-per-node` (the GPUs available per node; a **ceiling** a layout
///   may not exceed and a bound on a *derived* `TASKS_PER_NODE`, plus
///   @MAX_GPUS_PER_NODE@ — it never sets `GPUS_PER_TASK`, which defaults to 1)
///   and `default-gpus-per-task` (the machine's own per-task default, for the
///   rare partition that is not 1),
/// - `memory` (@MEMORY@), and `threads-per-cpu` (simfactory's `num-smt`;
///   @THREADS_PER_CPU@).
///
/// The two GPU keys are routinely absent: a CPU-only machine — or a CPU
/// partition on a machine whose GPU partitions declare their own — has no GPUs
/// to describe, and cactup falls back to one GPU per task rather than treating
/// a missing value as an error.
///
/// Simfactory's other capacity keys (`min-ppn`, `spn`, `mpn`, `nodes`, `max-*`,
/// `cpu-freq`, `flop-per-cycle`, …) were dropped — nothing consumed them.
///
/// The whole table is optional: each key may instead (or additionally) be set
/// per-queue in `[queues.<name>]`; queue values override these machine-wide
/// ones (`Meta::effective_hardware`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Hardware {
    /// Fill missing core/memory values from the OS at load time.
    // §4.6
    #[serde(default)]
    pub autodetect: bool,
    // §8.5
    /// CPUs/cores available per node — the availability fact the fill-the-node
    /// rule divides by `CPUS_PER_TASK` to get `TASKS_PER_NODE` (on a GPU run a
    /// fully derived result — no `--tpn`/`--tasks` — is bounded by
    /// `max-gpus-per-node`, so a machine may declare a `default-cpus-per-task`
    /// that deliberately leaves CPUs idle).
    pub max_cpus_per_node: Option<u32>,
    /// Request-side default for `CPUS_PER_TASK` when `--cpus` is omitted.
    // §8.5
    pub default_cpus_per_task: Option<u32>,
    // §8.5
    /// GPUs available per node. Never a target: cactup refuses a layout whose
    /// `GPUS_PER_TASK × TASKS_PER_NODE` exceeds it, and bounds a *derived*
    /// `TASKS_PER_NODE` by it (a node cannot run more one-GPU ranks than it has
    /// devices), but never derives `GPUS_PER_TASK` from it — GPUs are not
    /// oversubscribable the way CPUs are. Legitimately unset — a queue, or a
    /// whole machine, may have no GPUs at all — in which case nothing is
    /// checked and nothing is bounded.
    pub max_gpus_per_node: Option<u32>,
    /// Request-side default for `GPUS_PER_TASK` when `--gpus-per-task` is
    /// omitted. Only worth setting on a partition that wants something other
    /// than the global default of one GPU per rank.
    // §8.5
    pub default_gpus_per_task: Option<u32>,
    pub threads_per_cpu: Option<u32>,
    /// MB per node.
    pub memory: Option<u64>,
}

impl Hardware {
    /// SMT default per §8.5's assumption.
    pub fn threads_per_cpu(&self) -> u32 {
        self.threads_per_cpu.unwrap_or(1)
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Build {
    pub make: Option<String>,
    pub make_jobs: Option<u32>,
    #[serde(default)]
    pub enabled_thorns: Vec<String>,
    #[serde(default)]
    pub disabled_thorns: Vec<String>,
    /// Machine-level build-phase default universe (a flavor's own `universe`
    /// wins over it).
    // §4.8 precedence step 4
    pub universe: Option<String>,
    /// `build submit` groundwork (not yet wired to a command): whether an
    /// unqualified build should run in the foreground or go to the scheduler
    /// when the machine can do both; absent leaves the choice to be decided
    /// automatically.
    // Pinned foundation API (build submit groundwork); only tests read these
    // fields so far — the command that consumes them lands later.
    #[cfg_attr(not(test), allow(dead_code))]
    pub default_action: Option<BuildAction>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub queue: Option<String>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub walltime: Option<Walltime>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub nodes: Option<u32>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub tasks: Option<u32>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub cpus_per_task: Option<u32>,
    #[cfg_attr(not(test), allow(dead_code))]
    pub gpus_per_task: Option<u32>,
}

/// `[build].default-action` (`build submit` groundwork, not yet wired to a
/// command): whether an unqualified build runs in the foreground or is
/// handed to the scheduler.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BuildAction {
    Run,
    Submit,
}

// §4.2, §6.1. The doc comments below are rendered into the schema
// reference, so they carry no section references.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Environment {
    /// Shell setup for every phase, usually module loads and exports: run
    /// at the top of the build script, of the submit and build-submit
    /// scripts and of the run script, and before cactup's scheduler
    /// commands. Must only set the environment (see `provision`).
    pub env_setup: Option<String>,
    /// Appended to `env-setup` in the build script only (not in a
    /// build-submit script, which is the submit phase's).
    pub build_env_setup: Option<String>,
    /// Appended to `env-setup` in the submit and build-submit scripts and
    /// before cactup's scheduler commands: `submit` and `blocking-submit`
    /// with the submit universe's keys over these, `get-status`,
    /// `get-status-many`, `stop` and `exec-host` with these alone.
    pub submit_env_setup: Option<String>,
    /// Appended to `env-setup` in the run script only.
    pub run_env_setup: Option<String>,
    /// A provisioning step for every phase: commands that make or fetch
    /// what the phase needs (a software environment, a file), run in the
    /// build, submit, build-submit and run scripts after the setup, in a
    /// subshell of its own under `set -e`. What it exports or changes in
    /// its shell is lost when it ends; a failure stops the script. Never
    /// run before cactup's scheduler commands or by `cache doctor`.
    pub provision: Option<String>,
    /// A provisioning step for the build script only, run after
    /// `provision`.
    pub build_provision: Option<String>,
    /// A provisioning step for the submit and build-submit scripts only,
    /// run after `provision`.
    pub submit_provision: Option<String>,
    /// A provisioning step for the run script only, run after `provision`.
    pub run_provision: Option<String>,
}

impl Environment {
    fn phase_setup(&self, phase: Phase) -> &Option<String> {
        match phase {
            Phase::Build => &self.build_env_setup,
            Phase::Submit => &self.submit_env_setup,
            Phase::Run => &self.run_env_setup,
        }
    }

    fn phase_provision(&self, phase: Phase) -> &Option<String> {
        match phase {
            Phase::Build => &self.build_provision,
            Phase::Submit => &self.submit_provision,
            Phase::Run => &self.run_provision,
        }
    }

    /// The effective env block for a phase: `env-setup` followed by the
    /// matching `<phase>-env-setup`, each empty when unset (§4.2).
    pub fn effective(&self, phase: Phase) -> String {
        let mut out = String::new();
        for block in [&self.env_setup, self.phase_setup(phase)].into_iter().flatten() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(block);
        }
        out
    }

    /// The phase's provisioning steps as a script runs them, after the
    /// effective setup (§6.1): `provision`, then the matching
    /// `<phase>-provision`, each in a subshell of its own under `set -e`
    /// (what it sets cannot reach the phase), each stopping the script with
    /// its status when it fails, whether or not the script runs `set -e`
    /// itself. Empty when the phase has none.
    pub fn provisioning(&self, phase: Phase) -> String {
        let steps = [("provision", &self.provision), (phase.provision_key(), self.phase_provision(phase))];
        let mut out = String::new();
        for (key, text) in steps {
            let Some(text) = text.as_deref().filter(|text| !text.trim().is_empty()) else { continue };
            let newline = if text.ends_with('\n') { "" } else { "\n" };
            // The `:` after the text makes the step's status what `set -e`
            // makes of it: a failing command stops it, `exit N` gives N, and
            // a false `a && b` at its end (which `set -e` passes over) is
            // not a failure. `$?` in the case's arm is still the subshell's
            // status: the word a case tests runs no command. Not
            // `( … ) || exit`, which would turn the subshell's `set -e` off.
            out.push_str(&format!(
                "# {key}, in a subshell: what it sets stays there; a failure stops this script\n\
                 (\nset -e\n{text}{newline}:\n)\ncase $? in 0) ;; *) exit $? ;; esac\n"
            ));
        }
        out
    }

    /// This environment's keys where set, `base`'s where not: a universe's
    /// table over the machine's `[environment]` (§6.1).
    fn over(&self, base: &Environment) -> Environment {
        let pick = |o: &Option<String>, b: &Option<String>| o.clone().or_else(|| b.clone());
        Environment {
            env_setup: pick(&self.env_setup, &base.env_setup),
            build_env_setup: pick(&self.build_env_setup, &base.build_env_setup),
            submit_env_setup: pick(&self.submit_env_setup, &base.submit_env_setup),
            run_env_setup: pick(&self.run_env_setup, &base.run_env_setup),
            provision: pick(&self.provision, &base.provision),
            build_provision: pick(&self.build_provision, &base.build_provision),
            submit_provision: pick(&self.submit_provision, &base.submit_provision),
            run_provision: pick(&self.run_provision, &base.run_provision),
        }
    }
}

// §10 keeps simfactory's *consumed* scheduler keys verbatim. Its unconsumed
// ones were dropped outright — and, the table being closed, a machine that
// still carries one now fails to load rather than parsing it as a no-op.
// `interactive` went with §3.1, `stdout`/`stderr`/`stdout-follow` because
// output filenames are template-owned via @STDOUT_FILE@/@STDERR_FILE@ (§8.3.1)
// and `log` follows natively, and `max-queue-slots` because chain sizing is
// walltime-only (§8.8). simfactory's `allocation` is likewise absent: the
// account to charge is a per-user knob (§5), not a machine fact.
/// The machine's batch system: the commands cactup runs to submit, query and
/// stop jobs, and the patterns it reads their output with. Most keys are
/// simfactory's, where cactup uses them; a few are cactup's own (said so
/// below). Any other key fails to load. The account to charge is not here:
/// it is the per-user `allocation` knob.
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Scheduler {
    pub submit: Option<String>,
    // §10
    /// Optional blocking form of `submit` (a cactup addition): the same
    /// submission, expressed so the command does not return until the job has
    /// finished — `sbatch --wait`, `qsub -W block=true`. `cactup build submit
    /// --block` prefers it; machines that omit it get the emulated wait (the
    /// ordinary `submit`, then polling the attempt until it reports). Its
    /// output is scanned with the same `submit-pattern`, line by line as it
    /// arrives, so a command that prints the job id and only then blocks
    /// still yields the id at submission time.
    pub blocking_submit: Option<String>,
    pub get_status: Option<String>,
    // §10
    /// Optional one-call form of `get-status` (a cactup addition): lists
    /// every live job of `@USER@`, one per line, the job id as the first
    /// whitespace-separated field — e.g. `squeue -h -u @USER@ -o '%i %t (%r)'`.
    /// It lets `sim list` resolve a whole history with a single scheduler
    /// round-trip instead of one per simulation; machines that omit it are
    /// queried per job as before. The rest of each line is classified by the
    /// same `queued`/`running`/`holding` patterns, so the format a machine
    /// chooses has to keep those matching.
    pub get_status_many: Option<String>,
    pub stop: Option<String>,
    pub submit_pattern: Option<String>,
    pub status_pattern: Option<String>,
    pub queued_pattern: Option<String>,
    pub running_pattern: Option<String>,
    pub holding_pattern: Option<String>,
    pub exec_host: Option<String>,
    pub exec_host_pattern: Option<String>,
    /// Machine-level fallback ceiling for queues that omit `max-walltime`.
    // §4.2
    pub max_walltime: Option<Walltime>,
    // §11.6
    /// Env var(s) the scheduler sets inside a job allocation (a cactup
    /// addition) — e.g. `SLURM_JOB_ID` (both `salloc` and `sbatch` set it),
    /// `PBS_JOBID`. When declared, `test run`'s foreground path refuses to
    /// launch unless one is set: the flesh testsuite harness srun/mpirun's
    /// each test, which needs an allocation, so running on a login node just
    /// produces no output. Absent = no check (non-batch machines, or ones
    /// whose launcher self-allocates from the head node). Accepts a string or
    /// a list.
    #[serde(default, deserialize_with = "string_or_seq")]
    pub allocation_env: Vec<String>,
    // §10; the third of the questions decision 35 answers (design/
    // build-cache/DECISIONS.md, 35; not its decision 3), read by
    // `Scheduler::get_status`, `get_statuses` and `stop`.
    /// What a job id is: `"scheduler"` (the default), an id the batch
    /// system hands out, or `"pids"`, the pid of a process on the host the
    /// job was submitted from, where the scheduler is that host's own
    /// processes (`ps`, `kill`). With `"pids"`, cactup records the host and
    /// the process's start time with each job id, and asks `get-status` or
    /// `stop` about the job only on that host while the pid's process is
    /// the one that started then; otherwise the job is gone (or, recorded on
    /// another host, left to that host). `submit` must start each job in a
    /// process group of its own (`setsid`): cactup stops only a job whose
    /// process leads its own group.
    #[serde(default)]
    pub job_ids: JobIds,
    /// A regular expression matching a line of `get-status`'s output
    /// (stdout or stderr) that says the scheduler does not know the job,
    /// `@JOB_ID@` substituted as in the other patterns. Where it is set,
    /// output with no line for the job is a job gone only if a line matches
    /// it; otherwise the query failed: a command that would act on the
    /// answer refuses. After the run recorded its end, listings show that end
    /// and deletes note the failure and take the job as ended, while the commands that act take
    /// the safe reading (a stop is sent anyway; new work refuses, or a
    /// submission chains behind the job). Where it is not set, no line for
    /// the job is a job gone.
    pub unknown_job_pattern: Option<String>,
}

/// `[scheduler].job-ids`: where a machine's job ids come from.
// §10.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum JobIds {
    /// Handed out by a batch system.
    #[default]
    Scheduler,
    /// The pid of a process on the submitting host.
    Pids,
}

/// Deserialize a TOML string OR array-of-strings into a `Vec<String>` — lets
/// single-valued keys like `allocation-env` be written unquoted-list-free.
fn string_or_seq<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(String),
        Many(Vec<String>),
    }
    Ok(match OneOrMany::deserialize(d)? {
        OneOrMany::One(s) => vec![s],
        OneOrMany::Many(v) => v,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Queue {
    #[serde(default)]
    pub gpu: bool,
    pub max_walltime: Option<Walltime>,
    /// The queue used when -q is omitted (at most one may be marked).
    #[serde(default)]
    pub default: bool,
    /// The scheduler's real queue/partition name when it differs from the
    /// cactup queue key — several cactup queues (e.g. cpu/gpu build flavors)
    /// may map onto one real partition. `@QUEUE@` resolves to this.
    pub name: Option<String>,
    // §4.4
    /// Build-universe compatibility list: the queue only serves configs
    /// whose build universe is listed. `None` = compatible with ALL universes;
    /// an explicitly empty list is a validation error. Gates purely on the
    /// config's *build* universe, exactly like the same-named script-route
    /// key — hence the `build-` prefix.
    pub build_universes: Option<Vec<String>>,
    // §4.2
    /// Per-queue hardware overrides: each falls back to the top-level
    /// `[hardware]` value when unset (`Meta::effective_hardware`).
    pub max_cpus_per_node: Option<u32>,
    pub default_cpus_per_task: Option<u32>,
    /// GPUs per node on this partition — the key that lets one machine carry
    /// GPU partitions of different widths (qbd's gpu2/gpu4), each with its own
    /// ceiling on a job's layout.
    // §8.5
    pub max_gpus_per_node: Option<u32>,
    pub default_gpus_per_task: Option<u32>,
    pub threads_per_cpu: Option<u32>,
    /// MB per node.
    pub memory: Option<u64>,
}

impl Queue {
    /// Whether this queue may serve a config built in `universe` (§4.4).
    pub fn compatible_with(&self, universe: &str) -> bool {
        self.build_universes
            .as_ref()
            .map(|list| list.iter().any(|u| u == universe))
            .unwrap_or(true)
    }
}

/// One script kind's table — `[submitscripts]`, `[runscripts]` or
/// `[buildsubmitscripts]` (§4.2, §4.4). Every key names a script file in the
/// kind's directory, and its entry is that script's route: the queues it
/// serves, plus optional selection and wrapping facts. There are no reserved
/// keys, so a script may be named anything, `default` included. A script
/// serves exactly the queues it lists; there is no fallback script.
#[derive(Debug)]
pub struct ScriptTable {
    /// Which table this is, so its errors can say which kind of script is
    /// missing.
    pub kind: ScriptKind,
    pub routes: IndexMap<String, Route>,
}

/// A script's route, normalized from the array shorthand (queues only) or the
/// inline-table form (§4.2).
#[derive(Debug, Clone)]
pub struct Route {
    pub queues: Vec<String>,
    pub universe: Option<String>,
    // §4.4
    /// Build-universe compatibility list: the script only serves configs
    /// whose build universe is listed — for a run or submit script, configs
    /// whose binary it can run; for a buildsubmitscript, builds whose `make`
    /// runs in that universe. `None` = compatible with ALL universes;
    /// an explicitly empty list is a validation error. Gates purely on the
    /// config's *build* universe (never the run/submit universe) — hence the
    /// `build-` prefix on the TOML key.
    pub build_universes: Option<Vec<String>>,
    pub test: bool,
    /// Default total task count for runs launched through this script when no
    /// process-layout flag (-n/-T/-t) is given; overrides the fill-the-node
    /// default of §8.5.
    pub tasks: Option<u32>,
}

impl Route {
    /// Whether this script may serve a config built in `universe` (§4.4).
    pub fn compatible_with(&self, universe: &str) -> bool {
        self.build_universes
            .as_ref()
            .map(|list| list.iter().any(|u| u == universe))
            .unwrap_or(true)
    }
}

/// The inline-table form of a route. Deserialized on its own (rather
/// than as one arm of an untagged enum) so that a mistyped key reports as
/// serde's own "unknown field" error instead of a shapeless no-variant-matched
/// one.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct RouteRaw {
    queues: Vec<String>,
    universe: Option<String>,
    build_universes: Option<Vec<String>>,
    #[serde(default)]
    test: bool,
    tasks: Option<u32>,
}

impl From<RouteRaw> for Route {
    fn from(raw: RouteRaw) -> Self {
        let RouteRaw { queues, universe, build_universes, test, tasks } = raw;
        Route { queues, universe, build_universes, test, tasks }
    }
}

/// The array shorthand: queues only, every other field defaulted.
impl From<Vec<String>> for Route {
    fn from(queues: Vec<String>) -> Self {
        Route { queues, universe: None, build_universes: None, test: false, tasks: None }
    }
}

impl ScriptTable {
    fn empty(kind: ScriptKind) -> ScriptTable {
        ScriptTable { kind, routes: IndexMap::new() }
    }
    fn submit() -> ScriptTable {
        ScriptTable::empty(ScriptKind::Submit)
    }
    fn run() -> ScriptTable {
        ScriptTable::empty(ScriptKind::Run)
    }
    fn build_submit() -> ScriptTable {
        ScriptTable::empty(ScriptKind::BuildSubmit)
    }
    fn de_submit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ScriptTable, D::Error> {
        ScriptTable::deserialize_as(ScriptKind::Submit, d)
    }
    fn de_run<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ScriptTable, D::Error> {
        ScriptTable::deserialize_as(ScriptKind::Run, d)
    }
    fn de_build_submit<'de, D: serde::Deserializer<'de>>(d: D) -> Result<ScriptTable, D::Error> {
        ScriptTable::deserialize_as(ScriptKind::BuildSubmit, d)
    }

    fn deserialize_as<'de, D>(kind: ScriptKind, deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let raw = IndexMap::<String, toml::Value>::deserialize(deserializer)?;
        let mut out = ScriptTable::empty(kind);
        for (key, value) in raw {
            // Dispatch on the shape first so the error can be specific: a
            // wrong *shape* names both accepted forms, while a wrong key
            // inside the table form is serde's own "unknown field" list.
            let entry: Route = match &value {
                toml::Value::Array(_) => value
                    .try_into::<Vec<String>>()
                    .map(Route::from)
                    .map_err(|e| D::Error::custom(format!("script \"{key}\": {e}")))?,
                toml::Value::Table(_) => value
                    .try_into::<RouteRaw>()
                    .map(Route::from)
                    .map_err(|e| D::Error::custom(format!("script \"{key}\": {e}")))?,
                _ => {
                    return Err(D::Error::custom(format!(
                        "script \"{key}\" must be a [\"queue\", …] array or a \
                         {{ queues = […], universe = \"…\", build-universes = […], test = …, tasks = … }} table"
                    )))
                }
            };
            out.routes.insert(key, entry);
        }
        Ok(out)
    }
}

/// A universe (§4.8) as cactup holds it: at most one of the two wrapper forms,
/// plus the env-setup overrides. Deserialized through [`UniverseRaw`], which is
/// where the TOML schema — and the reference docs generated from it — lives.
#[derive(Debug, Clone)]
pub struct Universe {
    /// Prefix form: cactup runs `<wrapper-argv…> /bin/sh -c <inner>`.
    pub wrapper_argv: Option<Vec<String>>,
    /// Template form: one shell command containing exactly one `@COMMAND@`.
    pub wrapper: Option<String>,
    /// Per-universe env-setup overrides (§6.1).
    pub environment: Environment,
}

// §4.8, §6.1
/// A universe declaration: at most one of the two wrapper forms. Neither
/// wrapper = an identity universe (bare `sh -c` execution) — useful purely as
/// a carrier for env-setup overrides, which are written inline in the universe
/// table, alongside the wrapper keys.
// The wire form of `Universe`, and the struct the schema reference is
// generated from: `#[serde(flatten)]` on an `environment` field would read
// exactly the same TOML, but serde cannot combine `flatten` with
// `deny_unknown_fields`, so the table is spelled out flat here and folded back
// into `Universe` below.
#[derive(Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
struct UniverseRaw {
    /// Documentation only — a note to the reader ("apptainer", "docker", …);
    /// cactup never switches on it, so it is accepted and then dropped.
    #[allow(dead_code)]
    kind: Option<String>,
    /// Prefix form: cactup runs `<wrapper-argv…> /bin/sh -c <inner>`.
    wrapper_argv: Option<Vec<String>>,
    /// Template form: one shell command containing exactly one `@COMMAND@`.
    wrapper: Option<String>,
    // Each environment key below replaces the machine `[environment]` key
    // of the same name for work run in this universe (§6.1,
    // `Meta::effective_env`, `Meta::effective_provisioning`).
    /// Replaces the machine `[environment].env-setup` for what runs in
    /// this universe: the script of a phase this universe is that phase's
    /// universe for, and `submit`/`blocking-submit` when it is the submit
    /// universe (`get-status`, `stop` and the other scheduler queries keep
    /// the machine's keys). Each key below likewise; an empty value turns
    /// the machine's off here.
    env_setup: Option<String>,
    /// Replaces the machine `[environment].build-env-setup`.
    build_env_setup: Option<String>,
    /// Replaces the machine `[environment].submit-env-setup`.
    submit_env_setup: Option<String>,
    /// Replaces the machine `[environment].run-env-setup`.
    run_env_setup: Option<String>,
    /// Replaces the machine `[environment].provision`.
    provision: Option<String>,
    /// Replaces the machine `[environment].build-provision`.
    build_provision: Option<String>,
    /// Replaces the machine `[environment].submit-provision`.
    submit_provision: Option<String>,
    /// Replaces the machine `[environment].run-provision`.
    run_provision: Option<String>,
}

impl<'de> Deserialize<'de> for Universe {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let raw = UniverseRaw::deserialize(deserializer)?;
        Ok(Universe {
            wrapper_argv: raw.wrapper_argv,
            wrapper: raw.wrapper,
            environment: Environment {
                env_setup: raw.env_setup,
                build_env_setup: raw.build_env_setup,
                submit_env_setup: raw.submit_env_setup,
                run_env_setup: raw.run_env_setup,
                provision: raw.provision,
                build_provision: raw.build_provision,
                submit_provision: raw.submit_provision,
                run_provision: raw.run_provision,
            },
        })
    }
}

/// A universe-wrapped command, ready to spawn.
#[derive(Debug, Clone, PartialEq)]
pub enum WrappedCommand {
    /// Exec argv[0] with the remaining args directly (prefix form).
    Argv(Vec<String>),
    /// Run via `sh -c` (template form; author owns the quoting).
    Shell(String),
}

/// Shell-quote `s` for safe embedding in a template-form wrapper.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Byte offset of a `@COMMAND@` token sitting inside one of the template's own
/// quotes, if any. `Universe::wrap` fills `@COMMAND@` with an *already
/// shell-quoted* command, so the token must appear OUTSIDE the template's
/// quotes (§4.8); nesting it inside a quoted run collapses that quoting the
/// moment the command carries its own quote characters — the class of bug that
/// broke Deep Bayou's `host` universe. Scans a byte at a time tracking POSIX
/// `sh` quote state (single quotes are literal; `\` escapes outside single
/// quotes).
fn command_token_in_quotes(template: &str) -> Option<usize> {
    #[derive(PartialEq)]
    enum Q {
        None,
        Single,
        Double,
    }
    const TOK: &[u8] = b"@COMMAND@";
    let b = template.as_bytes();
    let mut state = Q::None;
    let mut i = 0;
    while i < b.len() {
        if b[i..].starts_with(TOK) {
            if state != Q::None {
                return Some(i);
            }
            i += TOK.len();
            continue;
        }
        match (&state, b[i]) {
            (Q::None | Q::Double, b'\\') => i += 1, // escape the next byte
            (Q::None, b'\'') => state = Q::Single,
            (Q::None, b'"') => state = Q::Double,
            (Q::Single, b'\'') => state = Q::None,
            (Q::Double, b'"') => state = Q::None,
            _ => {}
        }
        i += 1;
    }
    None
}

impl Universe {
    /// Wrap `inner` (an already-substituted shell snippet) in this universe.
    /// The wrapper itself is `@NAME@`-substituted with `vars` first; the
    /// reserved `@COMMAND@` is filled last and never re-scanned (§4.8).
    pub fn wrap(&self, vars: &VarSet, inner: &str) -> Res<WrappedCommand> {
        match (&self.wrapper_argv, &self.wrapper) {
            (Some(argv), None) => {
                let mut cmd = argv
                    .iter()
                    .map(|arg| vars.substitute(arg, Syntax::Plain))
                    .collect::<Res<Vec<_>>>()
                    .context("Failed to substitute universe wrapper-argv")?;
                cmd.extend(["/bin/sh".to_owned(), "-c".to_owned(), inner.to_owned()]);
                Ok(WrappedCommand::Argv(cmd))
            }
            (None, Some(template)) => {
                // Substituting COMMAND in the same single pass is equivalent
                // to "filled last": replacement text is never re-scanned.
                let mut vars = vars.clone();
                vars.set("COMMAND", shell_quote(inner));
                Ok(WrappedCommand::Shell(
                    vars.substitute(template, Syntax::Shell)
                        .context("Failed to substitute universe wrapper template")?,
                ))
            }
            // Identity universe (§4.8): no wrapper — run the inner command
            // bare, exactly like every caller's no-universe branch.
            (None, None) => Ok(WrappedCommand::Shell(inner.to_owned())),
            // Rejected by Meta::validate.
            (Some(_), Some(_)) => bail!("universe declares both wrapper-argv and wrapper"),
        }
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct CactupMeta {
    pub origin: Option<Origin>,
    /// The MDB generation a user-MDB machine was written for. `cactup machine
    /// create` records it; a user machine from an older generation is refused
    /// at load, with a pointer to what changed since. Absent means "this
    /// cactup's generation", with a warning. System-MDB machines do not carry
    /// it: their generation is the MDB's own.
    #[serde(default)]
    pub mdb_generation: Option<u32>,
}

/// `--from-existing` provenance.
// §4.7
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct Origin {
    pub from: String,
    pub hash: String,
}

impl ScriptTable {
    /// The scripts of one partition — test-marked or normal (§11.2) —
    /// regardless of universe compatibility (validation-side view).
    fn partition_all(&self, test: bool) -> impl Iterator<Item = (&str, &Route)> {
        self.routes
            .iter()
            .filter(move |(_, e)| e.test == test)
            .map(|(n, e)| (n.as_str(), e))
    }

    /// The scripts of one partition that are compatible with `universe`
    /// (§4.4/§11.2) — every selection candidate set is filtered this way.
    pub fn partition<'s>(
        &'s self,
        test: bool,
        universe: &str,
    ) -> impl Iterator<Item = (&'s str, &'s Route)> {
        self.partition_all(test).filter(move |(_, e)| e.compatible_with(universe))
    }

    /// Route `queue` to the script serving it for a config built in
    /// `universe` (§4.4) — pass `HOST_UNIVERSE` when the config records none.
    /// `test` picks the partition (§11.2): a `test …` command looks only at
    /// test-marked scripts, a normal one only at the rest, and neither ever
    /// borrows from the other — a testsuite needs a script written to run it,
    /// and a normal script lacks the variables a test run defines anyway. The
    /// queue is the only thing that picks a script: nothing a user names for
    /// some other purpose (an optionlist flavor) can reach this, and there is
    /// no fallback script — a queue no script lists is an error naming the
    /// queues that are served.
    pub fn route(&self, queue: &str, universe: &str, test: bool) -> Res<(&str, &Route)> {
        if let Some(found) = self.partition(test, universe).find(|(_, e)| e.queues.iter().any(|q| q == queue)) {
            return Ok(found);
        }
        let mut served: Vec<&str> =
            self.partition(test, universe).flat_map(|(_, e)| e.queues.iter().map(String::as_str)).collect();
        served.sort();
        served.dedup();
        let what = format!("{}{}", if test { "test " } else { "" }, self.kind.noun());
        let context = if universe == HOST_UNIVERSE {
            String::new()
        } else {
            format!(" for configs built in universe \"{universe}\"")
        };
        if served.is_empty() {
            bail!("no {what} serves queue \"{queue}\"{context}, nor any other queue");
        }
        bail!("no {what} serves queue \"{queue}\"{context} (served: {}); pick one of those with -q", served.join(", "));
    }
}

impl Meta {
    pub fn scripts(&self, kind: ScriptKind) -> &ScriptTable {
        match kind {
            ScriptKind::Submit => &self.submitscripts,
            ScriptKind::Run => &self.runscripts,
            ScriptKind::BuildSubmit => &self.buildsubmitscripts,
        }
    }

    /// Whether this machine can hand a BUILD to its scheduler: it needs both a
    /// buildsubmitscript to generate and a submit command to run.
    pub fn can_submit_builds(&self) -> bool {
        !self.buildsubmitscripts.routes.is_empty() && self.scheduler.submit.is_some()
    }

    /// The queue used when -q is omitted: the `default = true` queue, or the
    /// sole queue. A sim/test job passes the config's build `universe` to
    /// restrict the pick to queues that serve its binary (§4.4); a build job
    /// passes `None`, since where `make` runs says nothing about where the
    /// binary may (§7.9).
    pub fn default_queue(&self, universe: Option<&str>) -> Option<&str> {
        let serves = |q: &Queue| universe.is_none_or(|u| q.compatible_with(u));
        let mut compatible = self.queues.iter().filter(|(_, q)| serves(q));
        let first = compatible.next()?;
        if compatible.next().is_none() {
            return Some(first.0.as_str());
        }
        self.queues.iter().find(|(_, q)| q.default && serves(q)).map(|(n, _)| n.as_str())
    }

    /// The scheduler-facing name of queue `key` (what `@QUEUE@` resolves to):
    /// the queue's `name` override when set, else the key itself.
    pub fn scheduler_queue_name<'a>(&'a self, key: &'a str) -> Res<&'a str> {
        Ok(self.queue(key)?.name.as_deref().unwrap_or(key))
    }

    pub fn queue(&self, name: &str) -> Res<&Queue> {
        self.queues.get(name).ok_or_else(|| {
            anyhow!(
                "no queue named \"{name}\" (known: {})",
                self.queues.keys().cloned().collect::<Vec<_>>().join(", ")
            )
        })
    }

    /// The hardware in effect on `queue` (§4.2): the queue's own
    /// `max-cpus-per-node`/`default-cpus-per-task`/`max-gpus-per-node`/
    /// `default-gpus-per-task`/`threads-per-cpu`/`memory` where set, inheriting
    /// anything else from the top-level `[hardware]` table (which is itself
    /// optional).
    pub fn effective_hardware(&self, queue: &str) -> Res<Hardware> {
        let q = self.queue(queue)?;
        Ok(Hardware {
            autodetect: self.hardware.autodetect,
            max_cpus_per_node: q.max_cpus_per_node.or(self.hardware.max_cpus_per_node),
            default_cpus_per_task: q.default_cpus_per_task.or(self.hardware.default_cpus_per_task),
            max_gpus_per_node: q.max_gpus_per_node.or(self.hardware.max_gpus_per_node),
            default_gpus_per_task: q.default_gpus_per_task.or(self.hardware.default_gpus_per_task),
            threads_per_cpu: q.threads_per_cpu.or(self.hardware.threads_per_cpu),
            memory: q.memory.or(self.hardware.memory),
        })
    }

    /// The effective per-job walltime ceiling for `queue` (§4.2): the queue's
    /// `max-walltime` → the machine `[scheduler]` fallback → one year.
    pub fn effective_max_walltime(&self, queue: &str) -> Res<Walltime> {
        Ok(self
            .queue(queue)?
            .max_walltime
            .or(self.scheduler.max_walltime)
            .unwrap_or(FALLBACK_MAX_WALLTIME))
    }

    /// Resolve a universe name. `"host"` never errors (§4.8): the declared
    /// `[universes.host]` when present, else the implicit identity universe.
    pub fn universe(&self, name: &str) -> Res<&Universe> {
        if let Some(u) = self.universes.get(name) {
            return Ok(u);
        }
        if name == HOST_UNIVERSE {
            return Ok(&IMPLICIT_HOST);
        }
        let mut known: Vec<&str> = self.universes.keys().map(String::as_str).collect();
        if !self.universes.contains_key(HOST_UNIVERSE) {
            known.push(HOST_UNIVERSE);
        }
        Err(anyhow!(
            "no universe named \"{name}\" on this machine (known: {})",
            known.join(", ")
        ))
    }

    /// The DECLARED `[universes.host]` table, when the machine has one (§4.8):
    /// the resolution chains fall back to it — and only to it; an undeclared
    /// host stays `None` (implicit host ≡ identity ≡ bare execution).
    pub fn declared_host(&self) -> Option<&Universe> {
        self.universes.get(HOST_UNIVERSE)
    }

    /// The environment keys in force in `universe` (§6.1): the universe's
    /// set keys replace the machine `[environment]` ones KEY-BY-KEY; unset
    /// keys inherit. `None` (bare / --no-universe / implicit host) and
    /// unknown names give the machine's.
    fn environment_in(&self, universe: Option<&str>) -> std::borrow::Cow<'_, Environment> {
        match universe.and_then(|u| self.universes.get(u)) {
            None => std::borrow::Cow::Borrowed(&self.environment),
            Some(u) => std::borrow::Cow::Owned(u.environment.over(&self.environment)),
        }
    }

    /// Effective env block for `phase` when executing in `universe` (§6.1):
    /// `env-setup` then `<phase>-env-setup`, as `environment_in` resolves
    /// them. The setup alone, without the provisioning steps: what the build
    /// cache digests and `cache doctor` runs, and what precedes the
    /// scheduler's own commands.
    pub fn effective_env(&self, universe: Option<&str>, phase: Phase) -> String {
        self.environment_in(universe).effective(phase)
    }

    /// The provisioning steps for `phase` in `universe` (§6.1), as a script
    /// runs them right after the effective setup: `provision`, then
    /// `<phase>-provision`, each in a subshell. Empty when there are none.
    pub fn effective_provisioning(&self, universe: Option<&str>, phase: Phase) -> String {
        self.environment_in(universe).provisioning(phase)
    }

    /// What a generated submit or run script gets (§6.1): the effective
    /// setup followed by the provisioning steps. `@ENV_SETUP@` holds this,
    /// and a `.sh` script has it prepended.
    pub fn phase_env(&self, universe: Option<&str>, phase: Phase) -> String {
        let env = self.environment_in(universe);
        let (setup, provisioning) = (env.effective(phase), env.provisioning(phase));
        match (setup.is_empty(), provisioning.is_empty()) {
            (_, true) => setup,
            (true, false) => provisioning.trim_end_matches('\n').to_owned(),
            (false, false) => {
                let newline = if setup.ends_with('\n') { "" } else { "\n" };
                format!("{setup}{newline}{}", provisioning.trim_end_matches('\n'))
            }
        }
    }

    /// Whether the current process is inside one of this machine's job
    /// allocations (§11.6). `None` when the machine declares no
    /// `[scheduler].allocation-env` (unknowable — no check applies); else
    /// `Some(true)` iff at least one declared var is set and non-empty.
    pub fn in_allocation(&self) -> Option<bool> {
        self.allocation_status(|name| {
            std::env::var_os(name).is_some_and(|v| !v.is_empty())
        })
    }

    /// The `in_allocation` core, taking the env lookup as a closure so it can
    /// be exercised without touching the process environment.
    fn allocation_status(&self, is_set: impl Fn(&str) -> bool) -> Option<bool> {
        let vars = &self.scheduler.allocation_env;
        if vars.is_empty() {
            return None;
        }
        Some(vars.iter().any(|name| is_set(name)))
    }

    /// The §4.2/§11.2 load-time validation. `machine` names the machine in
    /// errors; call before the Meta is used.
    pub fn validate(&self, machine: &str) -> Res<()> {
        self.validate_inner()
            .with_context(|| format!("invalid meta.toml for machine \"{machine}\""))
    }

    fn validate_inner(&self) -> Res<()> {
        if self.queues.is_empty() {
            bail!("no [queues.*] declared; every machine needs at least one queue");
        }
        let defaults: Vec<_> = self.queues.iter().filter(|(_, q)| q.default).map(|(n, _)| n).collect();
        if defaults.len() > 1 {
            bail!("more than one queue is marked default = true: {}", defaults.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "));
        }

        // Queue build-universe gates (§4.4), mirroring the script-route lists:
        // non-empty and naming known universes.
        for (name, queue) in &self.queues {
            if let Some(universes) = &queue.build_universes {
                if universes.is_empty() {
                    bail!(
                        "queue \"{name}\" declares an empty build-universes list; omit the key \
                         to be compatible with all universes"
                    );
                }
                for u in universes {
                    if !self.is_known_universe(u) {
                        bail!("queue \"{name}\" lists unknown build universe \"{u}\"");
                    }
                }
            }
        }

        // §10: `blocking-submit` is an extra *flavor* of `submit`, never a
        // replacement for it — only `build submit --block` reaches for it, and
        // every other submission on the machine still goes through `submit`.
        // Declaring one without the other is therefore always a mistake, and a
        // silent one: it would surface as a machine that submits nothing.
        if self.scheduler.blocking_submit.is_some() && self.scheduler.submit.is_none() {
            bail!(
                "[scheduler].blocking-submit is declared without [scheduler].submit; it is the \
                 blocking flavor of that command, not a substitute for it"
            );
        }

        // §10: the pattern is compiled per query with the job id put in, as
        // the other status patterns are; any id must leave it valid.
        if let Some(pattern) = &self.scheduler.unknown_job_pattern {
            let pattern = pattern.replace("@JOB_ID@", &regex::escape("12345.server"));
            if let Err(e) = regex::Regex::new(&pattern) {
                bail!("[scheduler].unknown-job-pattern is not a valid regular expression: {e}");
            }
        }

        for kind in [ScriptKind::Submit, ScriptKind::Run, ScriptKind::BuildSubmit] {
            self.validate_script_kind(kind)
                .with_context(|| format!("in [{}]", kind.table_name()))?;
        }

        for (name, universe) in &self.universes {
            match (&universe.wrapper_argv, &universe.wrapper) {
                (Some(_), Some(_)) => bail!("universe \"{name}\" defines both wrapper-argv and wrapper; pick one"), // §4.8
                // Neither wrapper = identity universe (§4.8): legal, e.g. as a
                // pure env-setup override carrier.
                (None, None) => {}
                (None, Some(t)) => {
                    if !t.contains("@COMMAND@") {
                        bail!("universe \"{name}\"'s wrapper template does not contain @COMMAND@") // §4.8
                    }
                    if let Some(pos) = command_token_in_quotes(t) {
                        bail!(
                            "universe \"{name}\"'s wrapper template puts @COMMAND@ inside a \
                             quoted string (byte {pos}); wrap() supplies @COMMAND@ already \
                             shell-quoted, so it must sit OUTSIDE the template's own quotes \
                             — e.g. `bash -lc '… && '@COMMAND@`" // §4.8
                        )
                    }
                }
                (Some(_), None) => {}
            }
        }

        // Every universe referenced by name must exist; "host" is always a
        // legal reference (§4.8), declared or implicit.
        let mut refs: Vec<(String, &Option<String>)> = vec![("[build].universe".into(), &self.build.universe)];
        for kind in [ScriptKind::Submit, ScriptKind::Run, ScriptKind::BuildSubmit] {
            for (name, entry) in &self.scripts(kind).routes {
                refs.push((format!("[{}] script \"{name}\"", kind.table_name()), &entry.universe));
            }
        }
        for (what, reference) in refs {
            if let Some(name) = reference
                && !self.is_known_universe(name)
            {
                bail!("{what} names unknown universe \"{name}\" (known: {})",
                    if self.universes.is_empty() { HOST_UNIVERSE.to_owned() }
                    else { self.universes.keys().cloned().collect::<Vec<_>>().join(", ") });
            }
        }

        Ok(())
    }

    /// A name is referenceable as a universe when declared, or when it is the
    /// always-legal implicit "host" (§4.8).
    fn is_known_universe(&self, name: &str) -> bool {
        name == HOST_UNIVERSE || self.universes.contains_key(name)
    }

    /// Per-kind §4.2 checks, applied per §11.2 partition: sane queue and
    /// universe refs, and per-universe-unambiguous queue mappings. Full queue
    /// coverage is owed only by the normal submit and run scripts, and only
    /// for the host context — those are what a plain `sim submit` reaches for
    /// on every queue. Test scripts and buildsubmitscripts may serve just some
    /// queues (tests or builds offered on some partitions only), and so may a
    /// non-host universe's scripts; there a queue without a script is a
    /// selection-time error naming the queues that have one (§4.4, §11.2).
    fn validate_script_kind(&self, kind: ScriptKind) -> Res<()> {
        let sv = self.scripts(kind);

        // buildsubmitscripts are opt-in: a machine that never hands builds to
        // a scheduler declares none. Submit and Run stay mandatory — a machine
        // that cannot run anything must fail at load rather than at the first
        // `sim run`.
        if sv.routes.is_empty() && kind == ScriptKind::BuildSubmit {
            return Ok(());
        }

        for (name, entry) in &sv.routes {
            for queue in &entry.queues {
                if !self.queues.contains_key(queue) {
                    bail!("script \"{name}\" maps unknown queue \"{queue}\"");
                }
            }
            // A build job is never a test and takes its shape from [build],
            // so these would only ever be silently ignored there.
            if kind == ScriptKind::BuildSubmit && (entry.test || entry.tasks.is_some()) {
                bail!(
                    "build-submit script \"{name}\" sets `test` or `tasks`, which a build job \
                     never reads (its shape comes from [build])"
                );
            }
            if let Some(universes) = &entry.build_universes {
                if universes.is_empty() {
                    bail!(
                        "script \"{name}\" declares an empty build-universes list; omit the key \
                         to be compatible with all universes" // §4.4
                    );
                }
                for u in universes {
                    if !self.is_known_universe(u) {
                        bail!("script \"{name}\" lists unknown build universe \"{u}\"");
                    }
                }
            }
        }

        // The universe contexts selection can run under: host + declared.
        let contexts: Vec<&str> = std::iter::once(HOST_UNIVERSE)
            .chain(self.universes.keys().map(String::as_str).filter(|u| *u != HOST_UNIVERSE))
            .collect();

        for test in [false, true] {
            let partition: Vec<_> = sv.partition_all(test).collect();
            let owes_coverage = !test && kind != ScriptKind::BuildSubmit;
            if partition.is_empty() {
                if owes_coverage && !self.queues.is_empty() {
                    bail!("no normal scripts declared");
                }
                continue;
            }
            let label = if test { "test " } else { "" };

            for universe in &contexts {
                for (queue, qdef) in &self.queues {
                    // A queue gated to other build universes cannot be reached
                    // in this context — no script coverage is owed for it.
                    if !qdef.compatible_with(universe) {
                        continue;
                    }
                    let serving: Vec<_> = partition
                        .iter()
                        .filter(|(_, e)| e.compatible_with(universe) && e.queues.iter().any(|q| q == queue))
                        .collect();
                    if serving.len() > 1 {
                        bail!(
                            "queue \"{queue}\" is mapped by more than one {label}script \
                             compatible with universe \"{universe}\": {}",
                            serving.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                        );
                    }
                    if owes_coverage && *universe == HOST_UNIVERSE && serving.is_empty() {
                        bail!("queue \"{queue}\" is served by no script"); // §4.2
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve the `[paths]` values for actual use: `@USER@` plus any
    /// `@ENV(NAME)@` reads (§4.2, §6.1). Deliberately NOT run at MDB load —
    /// an entry must load and validate on hosts that lack the machine's
    /// environment (the dev-MDB sweep, `machine show`) — so an unset or
    /// empty env var errors here, at the moment a path is actually needed.
    /// The machine's values only, every key: what one user's knobs make of
    /// them is [`Meta::paths_for`].
    pub fn resolved_paths(&self) -> Res<Paths> {
        let mut paths = self.paths.clone();
        for (key, field) in path_fields(&mut paths) {
            if let Some(value) = field {
                *value = resolve_path(key, value)?;
            }
        }
        Ok(paths)
    }

    /// One `[paths]` key as it applies to the user whose knobs `db` holds
    /// (§4.2, §5): the knob of the same name (read leniently,
    /// a value that is not an absolute path is no value), else the
    /// machine's value, resolved. A machine value a knob overrides is not
    /// resolved at all, so an `@ENV(…)@` unset here costs nothing. Login node
    /// only (D11): what a compute-node run needs is frozen before it.
    pub fn path_for(&self, db: &crate::database::Database, key: &str) -> Res<Option<String>> {
        if let Some(value) = db.knob(key).filter(|value| std::path::Path::new(value).is_absolute()) {
            return Ok(Some(value.to_owned()));
        }
        let mut paths = self.paths.clone();
        let field = path_fields(&mut paths)
            .into_iter()
            .find_map(|(name, field)| (name == key).then(|| field.take()))
            .with_context(|| format!("no [paths] key \"{key}\""))?;
        field.map(|value| resolve_path(key, &value)).transpose()
    }

    /// Every `[paths]` key by [`Meta::path_for`], but the build cache's
    /// place, which is left `None`: that one is the cache's to resolve and
    /// fall back from (`objcache::store_root`), never a reason to fail a
    /// build, a simulation or an install.
    pub fn paths_for(&self, db: &crate::database::Database) -> Res<Paths> {
        let mut paths = Paths::default();
        for (key, field) in path_fields(&mut paths) {
            if key != "build-cache-home" {
                *field = self.path_for(db, key)?;
            }
        }
        Ok(paths)
    }
}

/// Each `[paths]` key with its field, by the name of the knob that
/// overrides it for one user (§4.2, §5).
fn path_fields(paths: &mut Paths) -> [(&'static str, &mut Option<String>); 5] {
    [
        ("install-home", &mut paths.install_home),
        ("simulation-home", &mut paths.simulation_home),
        ("test-home", &mut paths.test_home),
        ("scratch-home", &mut paths.scratch_home),
        ("build-cache-home", &mut paths.build_cache_home),
    ]
}

/// What a path knob left unset comes to, said for `cactup knob`; `None`
/// for any other name.
pub fn path_knob_unset(key: &str) -> Option<String> {
    let fallback = match (key, fallback_home(key)) {
        (_, Some(home)) => home.display().to_string(),
        ("scratch-home", None) => "empty".to_owned(),
        ("build-cache-home", None) => "<install-home>/.cactup-build-cache".to_owned(),
        _ => return None,
    };
    Some(format!("unset: the machine's value, else {fallback}"))
}

/// Where a `[paths]` key points when neither a knob nor the machine sets it
/// (§4.2): a directory of `~/.cactup`, for the keys that have one.
pub fn fallback_home(key: &str) -> Option<std::path::PathBuf> {
    let dir = match key {
        "install-home" => "cacti",
        "simulation-home" => "simulations",
        "test-home" => "tests",
        _ => return None,
    };
    Some(crate::CACTUP_ROOT.join(dir))
}

/// `@USER@` and `@ENV(NAME)@` in one `[paths]` value.
fn resolve_path(key: &str, value: &str) -> Res<String> {
    let mut vars = VarSet::new();
    vars.set("USER", super::whoami());
    vars.substitute(value, Syntax::Plain).with_context(|| format!("in [paths].{key}"))
}

/// Knob validator (§5) for the path knobs, one per `[paths]` key: an absolute
/// path, since every command reads it from whatever directory it runs in.
pub fn validate_path_knob(key: &str, value: &str) -> Res<String> {
    let value = value.trim();
    if !std::path::Path::new(value).is_absolute() || value.contains(['\n', '\0']) {
        bail!("invalid {key} \"{value}\": it must be an absolute path");
    }
    Ok(value.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multi-queue, multi-script machine exercising the §4.2 features
    /// (modeled on the spec's `mike` example).
    const MIKE: &str = r#"
        [machine]
        name = "mike"
        status = "production"
        hostname = "mike.hpc.lsu.edu"

        [paths]
        simulation-home = "/work/@USER@/simulations"

        [hardware]
        max-cpus-per-node = 16
        memory = 64000

        [scheduler]
        submit = "sbatch @SCRIPTFILE@ 2>&1"
        get-status = "squeue -j @JOB_ID@"
        max-walltime = "48:00:00"

        [environment]
        env-setup = "module load gcc/11 openmpi/4"
        build-env-setup = "module load cmake/3.27"

        [queues.checkpt]
        gpu = false
        max-walltime = "72:00:00"
        default = true

        [queues.single]
        gpu = false

        [queues.gpu]
        gpu = true
        max-walltime = "24:00:00"
        # Per-queue hardware overrides (§4.2); unset keys inherit [hardware].
        max-cpus-per-node = 64
        max-gpus-per-node = 4
        threads-per-cpu = 2

        [submitscripts]
        "slurm-cpu" = { queues = ["checkpt", "single"] }
        "slurm-gpu" = ["gpu"]
        "slurm-test" = { queues = ["checkpt", "single"], test = true }

        [runscripts]
        "cpu" = { queues = ["checkpt", "single"] }
        "gpu-sing" = { queues = ["gpu"], universe = "et-sif" }
        "test-cpu" = { queues = ["checkpt", "single"], test = true, tasks = 2 }


        [universes.et-sif]
        kind = "apptainer"
        wrapper-argv = ["apptainer", "exec", "--bind", "@SOURCEDIR@", "/work/@USER@/et.sif"]
    "#;

    fn mike() -> Meta {
        let meta: Meta = toml::from_str(MIKE).unwrap();
        meta.validate("mike").unwrap();
        meta
    }

    #[test]
    fn parses_and_validates_the_spec_example() {
        let meta = mike();
        assert_eq!(meta.machine.name.as_deref(), Some("mike"));
        assert_eq!(meta.default_queue(Some(HOST_UNIVERSE)), Some("checkpt"));
        assert!(meta.queues["gpu"].gpu);
        // Array shorthand and inline-table forms normalize identically.
        let sv = &meta.submitscripts.routes;
        assert_eq!(sv["slurm-gpu"].queues, ["gpu"]);
        assert!(!sv["slurm-gpu"].test);
        assert!(sv["slurm-test"].test);
    }

    /// §4.2's schema is closed: a key cactup does not model is a parse error
    /// naming it, not a silent no-op — so `ppn` (simfactory's spelling) or a
    /// typo'd `tets` fails at load instead of quietly doing nothing.
    #[test]
    fn unknown_keys_are_rejected_by_every_table() {
        let err = |text: String| toml::from_str::<Meta>(&text).expect_err("unknown key must not parse").to_string();

        // Top level.
        let msg = err(format!("{MIKE}\n[bogus]\nx = 1\n"));
        assert!(msg.contains("unknown field `bogus`"), "{msg}");
        // A modeled table — `allocation` is a knob, never a machine key.
        let msg = err(MIKE.replace("[scheduler]", "[scheduler]\n        allocation = \"proj\""));
        assert!(msg.contains("unknown field `allocation`"), "{msg}");
        // Per-queue.
        let msg = err(MIKE.replace("[queues.single]", "[queues.single]\n        ppn = 16"));
        assert!(msg.contains("unknown field `ppn`"), "{msg}");
        // Inside a script's inline table: the message names the script too,
        // since the table itself is keyed by script name.
        let msg = err(MIKE.replace(r#""slurm-gpu" = ["gpu"]"#, r#""slurm-gpu" = { queues = ["gpu"], tets = true }"#));
        assert!(msg.contains("unknown field `tets`") && msg.contains("slurm-gpu"), "{msg}");
        // A wrong *shape* still reports as a shape problem, not a key one.
        let msg = err(MIKE.replace(r#""slurm-gpu" = ["gpu"]"#, r#""slurm-gpu" = 3"#));
        assert!(msg.contains("must be a") && !msg.contains("unknown field"), "{msg}");
        // Inside a universe, whose env keys sit inline with the wrappers.
        let msg = err(MIKE.replace(r#"kind = "apptainer""#, r#"wraper = "x""#));
        assert!(msg.contains("unknown field `wraper`"), "{msg}");
    }

    /// The two keys §4.2/§4.8 keep for the human reader alone: a closed schema
    /// means they have to be modeled to be writable at all.
    #[test]
    fn documentation_only_keys_still_parse() {
        let text = MIKE.replace("[paths]", "webpage = \"https://hpc.lsu.edu/mike\"\n\n        [paths]");
        let meta: Meta = toml::from_str(&text).unwrap();
        meta.validate("mike").unwrap();
        assert_eq!(meta.machine.webpage.as_deref(), Some("https://hpc.lsu.edu/mike"));
        // `kind` (in MIKE) parses and is deliberately dropped: nothing switches on it.
        assert!(meta.universes["et-sif"].wrapper_argv.is_some());
    }

    #[test]
    fn scheduler_queue_name_honors_override() {
        // @QUEUE@ resolves to the queue key by default…
        assert_eq!(mike().scheduler_queue_name("gpu").unwrap(), "gpu");
        // …and to the `name` override when set (e.g. several cactup queues
        // over one real partition, or an intentionally-empty scheduler name).
        let toml_text = MIKE.replace("[queues.gpu]\n        gpu = true", "[queues.gpu]\n        name = \"gpu-v100\"\n        gpu = true");
        let meta: Meta = toml::from_str(&toml_text).unwrap();
        meta.validate("mike").unwrap();
        assert_eq!(meta.scheduler_queue_name("gpu").unwrap(), "gpu-v100");
        assert!(meta.scheduler_queue_name("nope").is_err());
    }

    #[test]
    fn effective_hardware_inherits_and_overrides() {
        let meta = mike();
        // Queue with no overrides: pure inheritance from [hardware].
        let hw = meta.effective_hardware("checkpt").unwrap();
        assert_eq!((hw.max_cpus_per_node, hw.memory, hw.threads_per_cpu()), (Some(16), Some(64000), 1));
        // …and a CPU queue simply has no GPU facts to inherit.
        assert_eq!((hw.max_gpus_per_node, hw.default_gpus_per_task), (None, None));
        // Queue overrides win key-by-key; unset keys still inherit.
        let hw = meta.effective_hardware("gpu").unwrap();
        assert_eq!((hw.max_cpus_per_node, hw.memory, hw.threads_per_cpu()), (Some(64), Some(64000), 2));
        assert_eq!(hw.max_gpus_per_node, Some(4), "per-queue GPU count is what makes qbd's gpu2/gpu4 expressible");
        assert!(meta.effective_hardware("nope").is_err());
    }

    #[test]
    fn hardware_table_is_optional_when_queues_define_it() {
        // No top-level [hardware] at all: queue-level keys carry the load.
        let toml_text = MIKE
            .replace("[hardware]\n        max-cpus-per-node = 16\n        memory = 64000", "")
            .replace("[queues.checkpt]", "[queues.checkpt]\nmax-cpus-per-node = 32\nmemory = 128000");
        let meta: Meta = toml::from_str(&toml_text).unwrap();
        meta.validate("mike").unwrap();
        let hw = meta.effective_hardware("checkpt").unwrap();
        assert_eq!((hw.max_cpus_per_node, hw.memory), (Some(32), Some(128000)));
        // A queue defining nothing gets the (empty) machine-wide values.
        let hw = meta.effective_hardware("single").unwrap();
        assert_eq!((hw.max_cpus_per_node, hw.memory), (None, None));
    }

    #[test]
    fn walltime_ceiling_precedence() {
        let meta = mike();
        // Queue value wins; machine [scheduler] fallback; built-in one year.
        assert_eq!(meta.effective_max_walltime("checkpt").unwrap(), Walltime(72 * 3600));
        assert_eq!(meta.effective_max_walltime("single").unwrap(), Walltime(48 * 3600));
        let mut meta = meta;
        meta.scheduler.max_walltime = None;
        assert_eq!(meta.effective_max_walltime("single").unwrap(), Walltime(365 * 86400));
        assert!(meta.effective_max_walltime("nope").is_err());
    }

    #[test]
    fn env_setup_phases_append() {
        let env = mike().environment;
        assert_eq!(env.effective(Phase::Run), "module load gcc/11 openmpi/4");
        assert_eq!(
            env.effective(Phase::Build),
            "module load gcc/11 openmpi/4\nmodule load cmake/3.27"
        );
    }

    #[test]
    fn script_selection_follows_the_queue() {
        let meta = mike();
        let rs = meta.scripts(ScriptKind::Run);
        assert_eq!(rs.route("gpu", "host", false).unwrap().0, "gpu-sing");
        assert_eq!(rs.route("single", "host", false).unwrap().0, "cpu");
        // Test commands get the test partition; normal never does (§11.2).
        assert_eq!(rs.route("single", "host", true).unwrap().0, "test-cpu");
        // gpu-sing carries its universe association (§4.8 step 3).
        assert_eq!(rs.route("gpu", "host", false).unwrap().1.universe.as_deref(), Some("et-sif"));
        // test-cpu carries its default-tasks setting (§4.2); cpu has none.
        assert_eq!(rs.route("single", "host", true).unwrap().1.tasks, Some(2));
        assert_eq!(rs.route("single", "host", false).unwrap().1.tasks, None);
    }

    /// A test never borrows a normal script (§11.2): a testsuite needs a
    /// script written to run it, and a normal one references variables a
    /// test run does not define. A queue no test script lists is an error
    /// naming the queues that have one; with no test script at all, every
    /// queue is.
    #[test]
    fn tests_never_borrow_normal_scripts() {
        let meta = mike();
        let rs = meta.scripts(ScriptKind::Run);
        let err = format!("{:#}", rs.route("gpu", "host", true).unwrap_err());
        assert!(err.contains("no test run script serves queue \"gpu\" (served: checkpt, single)"), "{err}");

        let mut meta = mike();
        meta.runscripts.routes.shift_remove("test-cpu");
        let rs = meta.scripts(ScriptKind::Run);
        let err = format!("{:#}", rs.route("single", "host", true).unwrap_err());
        assert!(err.contains("nor any other queue"), "{err}");
    }

    #[test]
    fn validation_rejects_uncovered_queue() {
        let broken = MIKE.replace("\"slurm-cpu\" = { queues = [\"checkpt\", \"single\"] }",
                                  "\"slurm-cpu\" = [\"checkpt\"]");
        let meta: Meta = toml::from_str(&broken).unwrap();
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("\"single\""), "unexpected error: {err}");
    }

    /// Only the normal submit and run scripts owe every queue a script at
    /// load: test scripts and buildsubmitscripts may serve some partitions
    /// only, and a queue they skip is refused when it is chosen.
    #[test]
    fn only_normal_scripts_must_cover_every_queue() {
        let partial = format!("{MIKE}\n[buildsubmitscripts]\n\"b\" = [\"gpu\"]\n");
        let meta: Meta = toml::from_str(&partial).unwrap();
        meta.validate("mike").unwrap();
        let err = format!("{:#}", meta.scripts(ScriptKind::BuildSubmit).route("single", "host", false).unwrap_err());
        assert!(err.contains("no build-submit script serves queue \"single\" (served: gpu)"), "{err}");
    }

    #[test]
    fn build_submit_routes_reject_test_and_tasks() {
        for extra in ["test = true", "tasks = 2"] {
            let text = format!("{MIKE}\n[buildsubmitscripts]\n\"b\" = {{ queues = [\"gpu\"], {extra} }}\n");
            let meta: Meta = toml::from_str(&text).unwrap();
            let err = format!("{:#}", meta.validate("mike").unwrap_err());
            assert!(err.contains("never reads"), "{extra}: {err}");
        }
    }

    #[test]
    fn validation_rejects_double_mapping() {
        let double_map = MIKE.replace("\"slurm-gpu\" = [\"gpu\"]",
                                      "\"slurm-gpu\" = [\"gpu\", \"single\"]");
        let meta: Meta = toml::from_str(&double_map).unwrap();
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("more than one"), "unexpected error: {err}");
    }

    #[test]
    fn validation_rejects_unknown_queue_and_unknown_universe() {
        let bad_queue = MIKE.replace("\"slurm-gpu\" = [\"gpu\"]", "\"slurm-gpu\" = [\"nope\"]");
        let meta: Meta = toml::from_str(&bad_queue).unwrap();
        assert!(meta.validate("mike").is_err());

        let bad_universe = MIKE.replace("universe = \"et-sif\"", "universe = \"missing\"");
        let meta: Meta = toml::from_str(&bad_universe).unwrap();
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("missing"), "unexpected error: {err}");
    }

    #[test]
    fn validation_rejects_bad_universe_forms() {
        let both = format!("{MIKE}\nwrapper = \"ssh h '@COMMAND@'\"\n");
        let meta: Meta = toml::from_str(&both).unwrap();
        assert!(meta.validate("mike").is_err());

        let no_command = MIKE.replace(
            "wrapper-argv = [\"apptainer\", \"exec\", \"--bind\", \"@SOURCEDIR@\", \"/work/@USER@/et.sif\"]",
            "wrapper = \"ssh headnode bash\"",
        );
        let meta: Meta = toml::from_str(&no_command).unwrap();
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("@COMMAND@"), "unexpected error: {err}");
    }

    #[test]
    fn validation_rejects_command_token_inside_template_quotes() {
        // The Deep Bayou bug: @COMMAND@ nested inside the template's own quotes.
        // wrap() already shell-quotes it, so this collapses once the command
        // carries quotes. Swap et-sif's wrapper-argv for such a template.
        let quoted = MIKE.replace(
            "wrapper-argv = [\"apptainer\", \"exec\", \"--bind\", \"@SOURCEDIR@\", \"/work/@USER@/et.sif\"]",
            "wrapper = \"bash -lc 'module load foo && @COMMAND@'\"",
        );
        let meta: Meta = toml::from_str(&quoted).unwrap();
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("inside a quoted string"), "unexpected error: {err}");

        // The fixed spelling (token outside the quotes) validates.
        let ok = MIKE.replace(
            "wrapper-argv = [\"apptainer\", \"exec\", \"--bind\", \"@SOURCEDIR@\", \"/work/@USER@/et.sif\"]",
            "wrapper = \"bash -lc 'module load foo && '@COMMAND@\"",
        );
        let meta: Meta = toml::from_str(&ok).unwrap();
        meta.validate("mike").unwrap();
    }

    #[test]
    fn command_token_quote_scan() {
        // Outside all quotes: fine.
        assert_eq!(command_token_in_quotes("ssh h 'cd /x && '@COMMAND@"), None);
        assert_eq!(command_token_in_quotes("@COMMAND@"), None);
        // Inside single quotes: flagged (the db1 bug).
        assert!(command_token_in_quotes("bash -lc 'a && @COMMAND@'").is_some());
        // Inside double quotes: also flagged.
        assert!(command_token_in_quotes("sh -c \"@COMMAND@\"").is_some());
        // A closed quote run before the token leaves it unquoted.
        assert_eq!(command_token_in_quotes("'a''b' @COMMAND@"), None);
        // An escaped quote does not open a quote run, so the token stays free.
        assert_eq!(command_token_in_quotes("echo \\' @COMMAND@"), None);
    }

    #[test]
    fn universe_wrapping_both_forms() {
        let meta = mike();
        let mut vars = VarSet::new();
        vars.set("SOURCEDIR", "/src");
        vars.set("USER", "alice");

        let wrapped = meta.universe("et-sif").unwrap().wrap(&vars, "make -j4").unwrap();
        assert_eq!(
            wrapped,
            WrappedCommand::Argv(vec![
                "apptainer".into(), "exec".into(), "--bind".into(), "/src".into(),
                "/work/alice/et.sif".into(), "/bin/sh".into(), "-c".into(), "make -j4".into(),
            ])
        );

        let ssh = Universe {
            wrapper_argv: None,
            wrapper: Some("ssh headnode 'cd @SOURCEDIR@ && '@COMMAND@".to_owned()),
            environment: Environment::default(),
        };
        let WrappedCommand::Shell(cmd) = ssh.wrap(&vars, "echo it's here").unwrap() else {
            panic!("expected shell form");
        };
        assert_eq!(cmd, "ssh headnode 'cd /src && ''echo it'\\''s here'");
    }

    /// A script table has no reserved keys: the old `default-universe`
    /// setting (generation 1) is just a malformed script entry now, and is
    /// refused naming both accepted entry shapes rather than silently taken.
    #[test]
    fn default_universe_is_no_longer_a_setting() {
        let with_default = MIKE.replace("[runscripts]", "[runscripts]\ndefault-universe = \"et-sif\"");
        let err = toml::from_str::<Meta>(&with_default).unwrap_err().to_string();
        assert!(err.contains("script \"default-universe\" must be a"), "{err}");
    }

    /// buildsubmitscript is the only OPTIONAL script kind. Skipping validation
    /// for an empty table must not leak over to submitscript/runscript: a
    /// machine that declares no runscript can never run anything, and has to
    /// fail here at load rather than much later at the first `sim run`.
    #[test]
    fn only_buildsubmitscript_may_be_absent() {
        // mike() itself declares no [buildsubmitscripts] and validates
        // fine — that is the opt-in kind doing its job.
        mike();

        // Empty its runscript table and the machine must be rejected.
        let no_runscripts = MIKE
            .replace("\"cpu\" = { queues = [\"checkpt\", \"single\"] }", "")
            .replace("\"gpu-sing\" = { queues = [\"gpu\"], universe = \"et-sif\" }", "")
            .replace(
                "\"test-cpu\" = { queues = [\"checkpt\", \"single\"], test = true, tasks = 2 }",
                "",
            );
        let meta: Meta = toml::from_str(&no_runscripts).unwrap();
        assert!(meta.runscripts.routes.is_empty(), "fixture edit missed the runscript table");
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("no normal scripts declared"), "unexpected error: {err}");
        assert!(err.contains("in [runscripts]"), "unexpected error: {err}");
    }

    /// `build submit` groundwork (not yet wired to a command): a machine that
    /// opts into [buildsubmitscripts] and the new [build] keys parses,
    /// validates, and reports itself able to submit builds.
    #[test]
    fn build_submit_keys_parse_and_validate() {
        let with_build_submit = MIKE
            .replace(
                "[submitscripts]",
                "[buildsubmitscripts]\n\
                 \"slurm-build\" = { queues = [\"checkpt\", \"single\"] }\n\n\
                 [submitscripts]",
            )
            .replace(
                "[queues.checkpt]",
                "[build]\n\
                 default-action = \"submit\"\n\
                 queue = \"gpu2\"\n\
                 walltime = \"4:00:00\"\n\
                 nodes = 1\n\
                 tasks = 1\n\
                 cpus-per-task = 64\n\
                 gpus-per-task = 0\n\n\
                 [queues.checkpt]",
            );
        let meta: Meta = toml::from_str(&with_build_submit).unwrap();
        meta.validate("mike").unwrap();

        assert_eq!(meta.build.default_action, Some(BuildAction::Submit));
        assert_eq!(meta.build.queue.as_deref(), Some("gpu2"));
        assert_eq!(meta.build.walltime, Some(Walltime(4 * 3600)));
        assert_eq!((meta.build.nodes, meta.build.tasks), (Some(1), Some(1)));
        assert_eq!((meta.build.cpus_per_task, meta.build.gpus_per_task), (Some(64), Some(0)));
        assert!(meta.can_submit_builds());
    }

    #[test]
    fn can_submit_builds_is_false_without_a_buildsubmitscript() {
        // MIKE never declares [buildsubmitscripts]; an empty table is
        // exactly the state of all 37 bundled machines today, and must load
        // and validate cleanly (it does — see `parses_and_validates_the_spec_example`).
        assert!(!mike().can_submit_builds());
    }

    /// §10: the blocking flavor of `submit` cannot stand in for it.
    #[test]
    fn blocking_submit_without_submit_is_rejected() {
        let meta: Meta = toml::from_str(
            &MIKE.replace(
                "submit = \"sbatch @SCRIPTFILE@ 2>&1\"",
                "blocking-submit = \"sbatch --wait @SCRIPTFILE@ 2>&1\"",
            ),
        )
        .unwrap();
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("blocking-submit"), "{err}");
        assert!(err.contains("[scheduler].submit"), "{err}");

        // Both together is the normal case, and the ordinary machine (which
        // declares neither) stays valid.
        mike().validate("mike").unwrap();
    }

    /// Every machine in the shipped MDB that declares a blocking submit also
    /// declares the ordinary one, and both name @SCRIPTFILE@ — an mdb-wide
    /// guard, so a future entry cannot quietly ship a `--block` that submits
    /// a script it was never handed.
    #[test]
    fn every_blocking_submit_in_the_mdb_matches_its_submit() {
        let mdb = crate::mdb::Mdb::with_roots(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            std::path::PathBuf::from("/nonexistent-user-mdb"),
        );
        let mut found = 0;
        for (name, _layer) in mdb.machines().unwrap() {
            let machine = mdb.load(&name).unwrap_or_else(|e| panic!("machine {name} failed to load: {e:#}"));
            let Some(blocking) = machine.meta.scheduler.blocking_submit.as_deref() else { continue };
            found += 1;
            let submit = machine
                .meta
                .scheduler
                .submit
                .as_deref()
                .unwrap_or_else(|| panic!("{name}: blocking-submit without submit"));
            assert!(submit.contains("@SCRIPTFILE@"), "{name}: submit names no @SCRIPTFILE@: {submit}");
            assert!(
                blocking.contains("@SCRIPTFILE@"),
                "{name}: blocking-submit names no @SCRIPTFILE@: {blocking}"
            );
        }
        // The mdb ships plenty of them; zero would mean this guard silently
        // stopped guarding anything.
        assert!(found > 10, "only {found} machines declare blocking-submit");
    }

    #[test]
    fn can_submit_builds_is_false_without_a_scheduler_submit_command() {
        let with_build_submit = MIKE
            .replace(
                "[submitscripts]",
                "[buildsubmitscripts]\n\
                 \"slurm-build\" = { queues = [\"checkpt\", \"single\"] }\n\n\
                 [submitscripts]",
            )
            // Drop [scheduler].submit: a buildsubmitscript with nothing
            // to run it is not actually submit-capable.
            .replace("submit = \"sbatch @SCRIPTFILE@ 2>&1\"\n        ", "");
        let meta: Meta = toml::from_str(&with_build_submit).unwrap();
        meta.validate("mike").unwrap();
        assert!(!meta.buildsubmitscripts.routes.is_empty());
        assert!(meta.scheduler.submit.is_none());
        assert!(!meta.can_submit_builds());
    }

    #[test]
    fn a_path_knob_overrides_the_machine() {
        let user = super::super::whoami();
        let meta: Meta = toml::from_str(&MIKE.replacen(
            "simulation-home",
            "scratch-home = \"@ENV(CACTUP_SURELY_UNSET_VARIABLE)@/scratch\"\nsimulation-home",
            1,
        ))
        .unwrap();
        let mut db = crate::database::Database::new();
        // Without knobs, the machine's values, and its unresolvable one
        // fails the lot.
        assert!(meta.paths_for(&db).is_err());
        assert_eq!(meta.path_for(&db, "install-home").unwrap(), None);
        // A knob stands in for the machine's value, which is then never
        // resolved; the other keys keep the machine's.
        db.set_knob("scratch-home", "/fast/mine".to_owned());
        db.set_knob("install-home", "/work/mine/cacti".to_owned());
        let paths = meta.paths_for(&db).unwrap();
        assert_eq!(paths.scratch_home.as_deref(), Some("/fast/mine"));
        assert_eq!(paths.install_home.as_deref(), Some("/work/mine/cacti"));
        assert_eq!(paths.simulation_home, Some(format!("/work/{user}/simulations")));
        // A value that is no absolute path is no value.
        db.set_knob("simulation-home", "relative/sims".to_owned());
        assert_eq!(meta.paths_for(&db).unwrap().simulation_home, Some(format!("/work/{user}/simulations")));
        // Every path knob is a standard knob, and only takes absolute paths.
        for (key, _) in path_fields(&mut Paths::default()) {
            assert!(crate::database::knob_spec(key).is_some(), "{key} is a standard knob");
            assert!(crate::database::knob_stored_form(key, "relative").is_err());
            assert_eq!(crate::database::knob_stored_form(key, " /abs/x ").unwrap(), "/abs/x");
            assert!(path_knob_unset(key).is_some_and(|note| note.contains("machine's value")), "{key}");
        }
        assert_eq!(path_knob_unset("queue"), None);
    }

    #[test]
    fn paths_resolution_only_touches_paths() {
        let meta = mike();
        let user = super::super::whoami();
        let paths = meta.resolved_paths().unwrap();
        assert_eq!(
            paths.simulation_home.as_deref(),
            Some(format!("/work/{user}/simulations").as_str())
        );
        // The stored meta keeps its tokens (resolution is on-demand)…
        assert_eq!(meta.paths.simulation_home.as_deref(), Some("/work/@USER@/simulations"));
        // … the build cache's place among them.
        let with_cache: Meta = toml::from_str(&MIKE.replacen(
            "simulation-home",
            "build-cache-home = \"/work/@USER@/cactup-cache\"\nsimulation-home",
            1,
        ))
        .unwrap();
        let db = crate::database::Database::new();
        assert_eq!(
            with_cache.path_for(&db, "build-cache-home").unwrap().as_deref(),
            Some(format!("/work/{user}/cactup-cache").as_str())
        );
        // One that cannot be resolved here fails the cache's question, and
        // nothing else.
        let unresolvable: Meta = toml::from_str(&MIKE.replacen(
            "simulation-home",
            "build-cache-home = \"@ENV(CACTUP_SURELY_UNSET_VARIABLE)@/cactup-cache\"\nsimulation-home",
            1,
        ))
        .unwrap();
        assert!(unresolvable.paths_for(&db).is_ok());
        assert!(unresolvable.path_for(&db, "build-cache-home").is_err());
        // …and scheduler templates keep theirs for use-time substitution.
        assert_eq!(meta.scheduler.submit.as_deref(), Some("sbatch @SCRIPTFILE@ 2>&1"));
    }

    /// A single-queue machine routing configs to scripts by build universe
    /// via `build-universes` lists (§4.4; modeled on the reworked db1 entry).
    const DB: &str = r#"
        [machine]
        name = "db"

        [environment]
        env-setup = "export BASE=1"
        build-env-setup = "module load base-build"

        [queues.gpu]
        gpu = true

        [submitscripts]
        "default"   = { queues = ["gpu"], build-universes = ["host"] }
        "sing"      = { queues = ["gpu"], build-universes = ["et-sing", "et-sing-cpu"] }
        "test"      = { queues = ["gpu"], build-universes = ["host"], test = true }
        "sing-test" = { queues = ["gpu"], build-universes = ["et-sing", "et-sing-cpu"], test = true }

        [runscripts]
        "default"   = { queues = ["gpu"], build-universes = ["host"] }
        "sing"      = { queues = ["gpu"], build-universes = ["et-sing", "et-sing-cpu"] }

        # Identity universe (§4.8): no wrapper, just a build-env override.
        [universes.host]
        build-env-setup = "module load gcc/9"

        [universes.et-sing]
        wrapper-argv = ["apptainer", "exec", "--nv", "et.sif"]

        [universes.et-sing-cpu]
        wrapper-argv = ["apptainer", "exec", "et.sif"]
    "#;

    fn db() -> Meta {
        let meta: Meta = toml::from_str(DB).unwrap();
        meta.validate("db").unwrap();
        meta
    }

    #[test]
    fn universes_lists_filter_selection() {
        let meta = db();
        let sv = meta.scripts(ScriptKind::Submit);
        // One queue, four scripts: the build universe picks the script (§4.4).
        assert_eq!(sv.route("gpu", "host", false).unwrap().0, "default");
        assert_eq!(sv.route("gpu", "et-sing", false).unwrap().0, "sing");
        assert_eq!(sv.route("gpu", "et-sing-cpu", false).unwrap().0, "sing");
        assert_eq!(sv.route("gpu", "host", true).unwrap().0, "test");
        assert_eq!(sv.route("gpu", "et-sing", true).unwrap().0, "sing-test");
        // These runscripts have no test partition, and a test never borrows
        // a normal script (§11.2).
        let rs = meta.scripts(ScriptKind::Run);
        assert!(rs.route("gpu", "et-sing-cpu", true).is_err());
    }

    #[test]
    fn universe_miss_is_a_selection_time_error() {
        // A declared universe no script lists: loads fine (coverage is a
        // load-time obligation only for host — §4.4)…
        let extra = format!("{DB}\n[universes.other]\nwrapper-argv = [\"env\"]\n");
        let meta: Meta = toml::from_str(&extra).unwrap();
        meta.validate("db").unwrap();
        // …but selecting under it fails, naming universe and queue.
        let err = format!(
            "{:#}",
            meta.scripts(ScriptKind::Run).route("gpu", "other", false).unwrap_err()
        );
        assert!(err.contains("\"other\"") && err.contains("\"gpu\""), "{err}");

        // Missing HOST coverage is still a load-time error.
        let no_host = DB
            .replace("\"default\"   = { queues = [\"gpu\"], build-universes = [\"host\"] }\n        \"sing\"      = { queues = [\"gpu\"], build-universes = [\"et-sing\", \"et-sing-cpu\"] }\n\n        # Identity",
                     "\"sing\"      = { queues = [\"gpu\"], build-universes = [\"et-sing\", \"et-sing-cpu\"] }\n\n        # Identity");
        let meta: Meta = toml::from_str(&no_host).unwrap();
        let err = format!("{:#}", meta.validate("db").unwrap_err());
        assert!(err.contains("served by no"), "{err}");
    }

    #[test]
    fn per_universe_ambiguity_is_rejected() {
        // Making the runscript "default" compatible with ALL universes (list
        // omitted) makes queue gpu doubly served in the et-sing context.
        let ambiguous = DB.replace(
            "[runscripts]\n        \"default\"   = { queues = [\"gpu\"], build-universes = [\"host\"] }",
            "[runscripts]\n        \"default\"   = { queues = [\"gpu\"] }",
        );
        let meta: Meta = toml::from_str(&ambiguous).unwrap();
        let err = format!("{:#}", meta.validate("db").unwrap_err());
        assert!(err.contains("more than one") && err.contains("et-sing"), "{err}");
    }

    #[test]
    fn universes_list_names_are_validated() {
        let empty = DB.replace("build-universes = [\"et-sing\", \"et-sing-cpu\"] }\n        \"test\"",
                               "build-universes = [] }\n        \"test\"");
        let meta: Meta = toml::from_str(&empty).unwrap();
        let err = format!("{:#}", meta.validate("db").unwrap_err());
        assert!(err.contains("empty build-universes list"), "{err}");

        let unknown = DB.replace("build-universes = [\"et-sing\", \"et-sing-cpu\"] }\n        \"test\"",
                                 "build-universes = [\"ghost\"] }\n        \"test\"");
        let meta: Meta = toml::from_str(&unknown).unwrap();
        let err = format!("{:#}", meta.validate("db").unwrap_err());
        assert!(err.contains("\"ghost\""), "{err}");
    }

    /// A two-queue machine whose queues are gated by build universe the same
    /// way scripts are (§4.4): `host-only` serves host builds, `sing` serves
    /// the Singularity flavors.
    const GATED_QUEUES: &str = r#"
        [machine]
        name = "gq"

        [queues.host-only]
        build-universes = ["host"]
        default = true

        [queues.sing]
        gpu = true
        build-universes = ["et-sing"]

        [submitscripts]
        "s" = { queues = ["host-only", "sing"] }

        [runscripts]
        "r" = { queues = ["host-only", "sing"] }


        [universes.et-sing]
        wrapper-argv = ["apptainer", "exec", "et.sif"]
    "#;

    #[test]
    fn queue_build_universes_gate_selection() {
        let meta: Meta = toml::from_str(GATED_QUEUES).unwrap();
        meta.validate("gq").unwrap();

        let host = &meta.queues["host-only"];
        let sing = &meta.queues["sing"];
        assert!(host.compatible_with("host") && !host.compatible_with("et-sing"));
        assert!(sing.compatible_with("et-sing") && !sing.compatible_with("host"));

        // The default queue is chosen among the universe-compatible queues:
        // "host-only" for host builds, and (as the sole compatible one) "sing"
        // for et-sing builds even though it is not marked default.
        assert_eq!(meta.default_queue(Some("host")), Some("host-only"));
        assert_eq!(meta.default_queue(Some("et-sing")), Some("sing"));
    }

    #[test]
    fn queue_build_universes_lists_are_validated() {
        let empty = GATED_QUEUES.replace("build-universes = [\"host\"]", "build-universes = []");
        let err = format!("{:#}", toml::from_str::<Meta>(&empty).unwrap().validate("gq").unwrap_err());
        assert!(err.contains("empty build-universes list"), "{err}");

        let unknown = GATED_QUEUES.replace("build-universes = [\"host\"]", "build-universes = [\"ghost\"]");
        let err = format!("{:#}", toml::from_str::<Meta>(&unknown).unwrap().validate("gq").unwrap_err());
        assert!(err.contains("\"ghost\"") && err.contains("host-only"), "{err}");
    }

    #[test]
    fn identity_universe_wraps_as_bare_shell() {
        let meta = db();
        // Declared host has no wrapper: identity wrapping (§4.8).
        let host = meta.universe("host").unwrap();
        assert!(host.wrapper.is_none() && host.wrapper_argv.is_none());
        let vars = VarSet::new();
        assert_eq!(
            host.wrap(&vars, "make -j4").unwrap(),
            WrappedCommand::Shell("make -j4".to_owned())
        );
    }

    #[test]
    fn host_universe_is_always_resolvable() {
        // mike declares no host: `universe("host")` yields the implicit
        // identity universe, and declared_host stays None (§4.8).
        let meta = mike();
        assert!(meta.declared_host().is_none());
        let host = meta.universe("host").unwrap();
        assert_eq!(
            host.wrap(&VarSet::new(), "true").unwrap(),
            WrappedCommand::Shell("true".to_owned())
        );
        // Unknown names still error, with host in the known list.
        let err = format!("{:#}", meta.universe("ghost").unwrap_err());
        assert!(err.contains("host") && err.contains("et-sif"), "{err}");
        // "host" is a legal reference even when undeclared.
        let with_ref = MIKE.replace(
            "\"cpu\" = { queues = [\"checkpt\", \"single\"] }",
            "\"cpu\" = { queues = [\"checkpt\", \"single\"], universe = \"host\" }",
        );
        assert_ne!(with_ref, MIKE, "fixture edit missed the cpu runscript");
        let meta: Meta = toml::from_str(&with_ref).unwrap();
        meta.validate("mike").unwrap();
        // A declared host is returned as-is.
        assert!(db().declared_host().is_some());
    }

    #[test]
    fn effective_env_overrides_key_by_key() {
        let meta = db();
        // No universe: the machine [environment] as before.
        assert_eq!(meta.effective_env(None, Phase::Build), "export BASE=1\nmodule load base-build");
        // host overrides build-env-setup only; env-setup is inherited (§6.1).
        assert_eq!(meta.effective_env(Some("host"), Phase::Build), "export BASE=1\nmodule load gcc/9");
        // Unset phase keys inherit, concatenation semantics unchanged.
        assert_eq!(meta.effective_env(Some("host"), Phase::Run), "export BASE=1");
        // A universe with no env keys inherits everything.
        assert_eq!(meta.effective_env(Some("et-sing"), Phase::Build), "export BASE=1\nmodule load base-build");
        // Unknown names fall back to the machine env (resolution errors first).
        assert_eq!(meta.effective_env(Some("nope"), Phase::Build), "export BASE=1\nmodule load base-build");
    }

    /// The provisioning keys (§6.1): `provision` in every phase, then the
    /// phase's own, each in a subshell after the setup, overridden by a
    /// universe key by key like the setups; never part of the setup itself.
    #[test]
    fn provisioning_follows_the_setup_key_by_key() {
        let text = DB
            .replace(
                "build-env-setup = \"module load base-build\"",
                "build-env-setup = \"module load base-build\"\n        provision = \"mkdir -p /tmp/all\"\n        run-provision = \"touch /tmp/run\"",
            )
            .replace(
                "build-env-setup = \"module load gcc/9\"",
                "build-env-setup = \"module load gcc/9\"\n        build-provision = \"fetch-it\"",
            );
        let meta: Meta = toml::from_str(&text).unwrap();
        meta.validate("db").unwrap();
        // The setup is the setup alone.
        assert_eq!(meta.effective_env(Some("host"), Phase::Build), "export BASE=1\nmodule load gcc/9");
        // The machine's: `provision` only for a build, then `run-provision`
        // for a run, each a subshell of its own.
        let build = meta.effective_provisioning(None, Phase::Build);
        assert_eq!(build.matches("\n(\nset -e\n").count(), 1, "{build}");
        assert!(build.starts_with("# provision, in a subshell") && build.contains("mkdir -p /tmp/all\n:\n)\ncase $? in"), "{build}");
        let run = meta.effective_provisioning(None, Phase::Run);
        let (all, own) = (run.find("mkdir -p /tmp/all").unwrap(), run.find("touch /tmp/run").unwrap());
        assert!(all < own && run.contains("# run-provision, in a subshell"), "{run}");
        assert_eq!(meta.effective_provisioning(None, Phase::Submit), meta.effective_provisioning(None, Phase::Build));
        // The host universe adds a build-provision and inherits `provision`.
        let host = meta.effective_provisioning(Some("host"), Phase::Build);
        assert!(host.find("mkdir -p /tmp/all").unwrap() < host.find("fetch-it").unwrap(), "{host}");
        // What a script gets: the setup, then the steps.
        let phase = meta.phase_env(Some("host"), Phase::Build);
        assert!(phase.starts_with("export BASE=1\nmodule load gcc/9\n# provision,") && phase.ends_with("esac"), "{phase}");
        assert_eq!(meta.phase_env(Some("et-sing"), Phase::Submit).lines().next(), Some("export BASE=1"));
        // None at all: nothing is added.
        assert_eq!(db().phase_env(None, Phase::Run), "export BASE=1");
        assert_eq!(db().effective_provisioning(None, Phase::Run), "");
    }

    /// A universe inherits the machine's provisioning keys unless it sets
    /// its own (§6.1), and an empty one turns the machine's off there: what
    /// a generation-2 universe with its own setup never ran (the creation
    /// lines were in the machine's setup it replaced) stays off with
    /// `provision = ""`.
    #[test]
    fn an_empty_universe_provisioning_key_turns_the_machine_s_off() {
        let text = DB
            .replace("env-setup = \"export BASE=1\"", "env-setup = \"export BASE=1\"\n        provision = \"python3 -m venv ~/venv\"\n        build-provision = \"fetch-it\"")
            .replace("wrapper-argv = [\"apptainer\", \"exec\", \"et.sif\"]", "wrapper-argv = [\"apptainer\", \"exec\", \"et.sif\"]\n        env-setup = \"export SING=1\"\n        provision = \"\"");
        let meta: Meta = toml::from_str(&text).unwrap();
        meta.validate("db").unwrap();
        // et-sing sets no provisioning key: it inherits the machine's.
        for phase in [Phase::Build, Phase::Submit, Phase::Run] {
            assert!(meta.effective_provisioning(Some("et-sing"), phase).contains("python3 -m venv"), "{phase:?}");
        }
        // et-sing-cpu's empty `provision` turns the machine's off, and only
        // that key: the machine's `build-provision` still runs there.
        for phase in [Phase::Submit, Phase::Run] {
            assert_eq!(meta.effective_provisioning(Some("et-sing-cpu"), phase), "", "{phase:?}");
            assert_eq!(meta.phase_env(Some("et-sing-cpu"), phase), "export SING=1");
        }
        let build = meta.effective_provisioning(Some("et-sing-cpu"), Phase::Build);
        assert!(!build.contains("venv") && build.contains("fetch-it"), "{build}");
    }

    /// A provisioning step runs under `set -e` in its subshell, keeps what
    /// it sets there, and stops the script with its status when it fails,
    /// in a script with `set -e` or without (§6.1).
    #[test]
    fn a_provisioning_step_is_contained_and_stops_on_failure() {
        let env = Environment {
            env_setup: Some("export SEEN=setup".to_owned()),
            provision: Some("export SEEN=provision\nfalse\necho not reached\n".to_owned()),
            run_provision: Some("echo run".to_owned()),
            ..Environment::default()
        };
        let run = |script: &str| {
            let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).output().unwrap();
            (out.status.code(), String::from_utf8_lossy(&out.stdout).into_owned())
        };
        for errexit in ["", "set -e\n"] {
            let script = format!("{errexit}{}\n{}echo after", env.effective(Phase::Run), env.provisioning(Phase::Run));
            assert_eq!(run(&script), (Some(1), String::new()), "{script}");
        }
        let env = Environment { provision: Some("export SEEN=provision\nexit 0\n".to_owned()), ..env };
        let script = format!("{}\n{}echo \"$SEEN\"", env.effective(Phase::Run), env.provisioning(Phase::Run));
        assert_eq!(run(&script), (Some(0), "run\nsetup\n".to_owned()), "{script}");
        let env = Environment { run_provision: Some("exit 7".to_owned()), ..env };
        assert_eq!(run(&format!("{}echo after", env.provisioning(Phase::Run))).0, Some(7));
        // As `set -e` has it: a false `&&` at the end passes, a failure
        // mid-step stops it, an explicit exit gives its status.
        for (text, expected) in [
            ("[ -e /nonexistent ] && echo made", (Some(0), "after\n")),
            ("false\necho not reached", (Some(1), "")),
            ("exit 3", (Some(3), "")),
        ] {
            let env = Environment { provision: Some(text.to_owned()), ..Environment::default() };
            for errexit in ["", "set -e\n"] {
                let script = format!("{errexit}{}echo after", env.provisioning(Phase::Build));
                assert_eq!(run(&script), (expected.0, expected.1.to_owned()), "{script}");
            }
        }
    }

    /// The generation-2 names of the phase setups are gone (§17.5): the
    /// generation is the only compatibility, so they fail as any unknown
    /// key does, in `[environment]` and in a universe.
    #[test]
    fn the_old_phase_setup_names_are_unknown_keys() {
        for old in ["env-build-setup", "env-submit-setup", "env-run-setup"] {
            let msg = toml::from_str::<Meta>(&MIKE.replace("build-env-setup", old)).unwrap_err().to_string();
            assert!(msg.contains(&format!("unknown field `{old}`")), "{msg}");
            let msg = toml::from_str::<Meta>(&DB.replace("build-env-setup = \"module load gcc/9\"", &format!("{old} = \"x\""))).unwrap_err().to_string();
            assert!(msg.contains(&format!("unknown field `{old}`")), "{msg}");
        }
    }

    /// `[scheduler].job-ids` and `unknown-job-pattern` (§10): parsed and
    /// checked; a value or a pattern cactup cannot use fails the load.
    #[test]
    fn the_job_id_keys_parse_and_validate() {
        assert_eq!(mike().scheduler.job_ids, JobIds::Scheduler);
        assert!(mike().scheduler.unknown_job_pattern.is_none());
        let with = |keys: &str| MIKE.replace("[scheduler]", &format!("[scheduler]\n        {keys}"));
        let meta: Meta = toml::from_str(&with("job-ids = \"pids\"\n        unknown-job-pattern = 'Unknown Job Id @JOB_ID@'")).unwrap();
        meta.validate("mike").unwrap();
        assert_eq!(meta.scheduler.job_ids, JobIds::Pids);
        assert_eq!(meta.scheduler.unknown_job_pattern.as_deref(), Some("Unknown Job Id @JOB_ID@"));
        let msg = toml::from_str::<Meta>(&with("job-ids = \"pid\"")).unwrap_err().to_string();
        assert!(msg.contains("unknown variant `pid`") && msg.contains("pids"), "{msg}");
        let meta: Meta = toml::from_str(&with("unknown-job-pattern = 'Unknown (Job'")).unwrap();
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("unknown-job-pattern is not a valid regular expression"), "{err}");
    }

    #[test]
    fn allocation_env_parses_and_reports_status() {
        // Undeclared: unknowable, no check applies.
        assert_eq!(mike().in_allocation(), None);
        assert!(mike().scheduler.allocation_env.is_empty());

        // String form → single-element list.
        let one = MIKE.replace(
            "max-walltime = \"48:00:00\"",
            "max-walltime = \"48:00:00\"\n        allocation-env = \"SLURM_JOB_ID\"",
        );
        let meta: Meta = toml::from_str(&one).unwrap();
        meta.validate("mike").unwrap();
        assert_eq!(meta.scheduler.allocation_env, ["SLURM_JOB_ID"]);
        // Outside an allocation (no listed var set) → Some(false); inside → Some(true).
        assert_eq!(meta.allocation_status(|_| false), Some(false));
        assert_eq!(meta.allocation_status(|n| n == "SLURM_JOB_ID"), Some(true));

        // List form → any-of semantics.
        let many = MIKE.replace(
            "max-walltime = \"48:00:00\"",
            "max-walltime = \"48:00:00\"\n        allocation-env = [\"SLURM_JOB_ID\", \"PBS_JOBID\"]",
        );
        let meta: Meta = toml::from_str(&many).unwrap();
        assert_eq!(meta.scheduler.allocation_env, ["SLURM_JOB_ID", "PBS_JOBID"]);
        assert_eq!(meta.allocation_status(|n| n == "PBS_JOBID"), Some(true));
        assert_eq!(meta.allocation_status(|_| false), Some(false));
    }

    #[test]
    fn env_token_in_paths_errors_when_unset() {
        let mut meta = mike();
        meta.paths.test_home = Some("@ENV(CACTUP_TEST_SURELY_UNSET)@/tests".into());
        let err = format!("{:#}", meta.resolved_paths().unwrap_err());
        assert!(
            err.contains("test-home") && err.contains("CACTUP_TEST_SURELY_UNSET"),
            "key and env var named: {err}"
        );
    }
}
