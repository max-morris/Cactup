//! `<machine>/meta.toml` model, load-time validation, and variant/queue/
//! universe resolution (spec §4.2, §4.4, §4.8, §11.2).
//!
//! serde is deliberately tolerant of unmodelled keys (simfactory carried many
//! informational fields); everything cactup *acts on* is modelled below.

use crate::template::VarSet;
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
    kind: None,
    wrapper_argv: None,
    wrapper: None,
    environment: Environment {
        env_setup: None,
        env_build_setup: None,
        env_submit_setup: None,
        env_run_setup: None,
    },
};

/// The three execution phases an `env-<phase>-setup` can target (§4.2, §6.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Build,
    Submit,
    Run,
}

/// The two per-queue script kinds (§4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptKind {
    Submit,
    Run,
}

impl ScriptKind {
    /// The `[variants.<kind>]` table name / script directory stem.
    pub fn dir_name(&self) -> &'static str {
        match self {
            ScriptKind::Submit => "submitscripts",
            ScriptKind::Run => "runscripts",
        }
    }

    fn table_name(&self) -> &'static str {
        match self {
            ScriptKind::Submit => "variants.submitscript",
            ScriptKind::Run => "variants.runscript",
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
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
    pub variants: Variants,
    #[serde(default)]
    pub universes: IndexMap<String, Universe>,
    /// cactup-written metadata; today only the `machine create --from-existing`
    /// provenance record (§4.7).
    #[serde(default)]
    pub cactup: CactupMeta,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct MachineInfo {
    pub name: Option<String>,
    pub nickname: Option<String>,
    /// personal | experimental | production | storage | outdated.
    pub status: Option<String>,
    pub hostname: Option<String>,
    pub location: Option<String>,
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Paths {
    /// Default install prefix; fallback `~/.cactup/cacti` (§4.2).
    pub install_home: Option<String>,
    /// Simulation-output root; fallback `~/.cactup/simulations` (§4.2, §8.1).
    pub simulation_home: Option<String>,
    /// Testsuite-output root; fallback `~/.cactup/tests` (§4.2, §11.5).
    pub test_home: Option<String>,
    /// Exposed to scripts as @SCRATCH_HOME@ (empty when unset); otherwise
    /// unused by cactup itself.
    pub scratch_home: Option<String>,
}

/// §4.2 carries only the hardware keys cactup actually feeds to submit/run
/// scripts. These split along the **availability vs. request** axis (§8.5): the
/// `max-` keys are *availability* facts (what a node/queue physically has),
/// while the process-layout flags (`--tasks`, `--cpus`, …) are the *request*.
///
/// - `max-cpus-per-node` (simfactory's `ppn`; the CPUs/cores available per node
///   — an availability fact, **not** MPI ranks; §8.5 topology +
///   @MAX_CPUS_PER_NODE@),
/// - `default-cpus-per-task` (simfactory's `num-threads`; the request-side
///   default for `CPUS_PER_TASK` when `--cpus` is omitted — §8.5),
/// - `memory` (@MEMORY@), and `threads-per-cpu` (simfactory's `num-smt`;
///   @THREADS_PER_CPU@).
///
/// Simfactory's other capacity keys (`min-ppn`, `spn`, `mpn`, `nodes`, `max-*`,
/// `cpu-freq`, `flop-per-cycle`, …) were dropped — nothing consumed them.
///
/// The whole table is optional: each key may instead (or additionally) be set
/// per-queue in `[queues.<name>]`; queue values override these machine-wide
/// ones (`Meta::effective_hardware`).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Hardware {
    /// Fill missing core/memory values from the OS at load time (§4.6).
    #[serde(default)]
    pub autodetect: bool,
    /// CPUs/cores available per node — the availability fact the fill-the-node
    /// rule divides by `CPUS_PER_TASK` to get `TASKS_PER_NODE` (§8.5).
    pub max_cpus_per_node: Option<u32>,
    /// Request-side default for `CPUS_PER_TASK` when `--cpus` is omitted (§8.5).
    pub default_cpus_per_task: Option<u32>,
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
#[serde(rename_all = "kebab-case")]
pub struct Build {
    pub make: Option<String>,
    pub make_jobs: Option<u32>,
    #[serde(default)]
    pub enabled_thorns: Vec<String>,
    #[serde(default)]
    pub disabled_thorns: Vec<String>,
    /// Machine-level build-phase default universe (§4.8 precedence step 4).
    pub universe: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Environment {
    pub env_setup: Option<String>,
    pub env_build_setup: Option<String>,
    pub env_submit_setup: Option<String>,
    pub env_run_setup: Option<String>,
}

impl Environment {
    /// The effective env block for a phase: `env-setup` followed by the
    /// matching `env-<phase>-setup`, each empty when unset (§4.2).
    pub fn effective(&self, phase: Phase) -> String {
        let extra = match phase {
            Phase::Build => &self.env_build_setup,
            Phase::Submit => &self.env_submit_setup,
            Phase::Run => &self.env_run_setup,
        };
        let mut out = String::new();
        for block in [&self.env_setup, extra].into_iter().flatten() {
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            out.push_str(block);
        }
        out
    }
}

/// §10 keeps simfactory's scheduler keys verbatim; the `allow(dead_code)`
/// ones are carried for MDB fidelity, not consumed: `interactive` was dropped
/// (§3.1), stdout/stderr filenames are template-owned via @STDOUT_FILE@/
/// @STDERR_FILE@ (§8.3.1), and chain sizing is walltime-only (§8.8).
#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Scheduler {
    pub submit: Option<String>,
    #[allow(dead_code)]
    pub interactive_cmd: Option<String>,
    pub get_status: Option<String>,
    /// Optional one-call form of `get-status` (§10, a cactup addition): lists
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
    #[allow(dead_code)]
    pub stdout: Option<String>,
    #[allow(dead_code)]
    pub stderr: Option<String>,
    #[allow(dead_code)]
    pub stdout_follow: Option<String>,
    #[allow(dead_code)]
    pub max_queue_slots: Option<u32>,
    /// Machine-level fallback ceiling for queues that omit `max-walltime` (§4.2).
    pub max_walltime: Option<Walltime>,
    /// Env var(s) the scheduler sets inside a job allocation — e.g.
    /// `SLURM_JOB_ID` (both `salloc` and `sbatch` set it), `PBS_JOBID`. When
    /// declared, `test run`'s foreground path (§11.6) refuses to launch unless
    /// one is set: the flesh testsuite harness srun/mpirun's each test, which
    /// needs an allocation, so running on a login node just produces no output.
    /// Absent = no check (non-batch machines, or ones whose launcher
    /// self-allocates from the head node). Accepts a string or a list.
    #[serde(default, deserialize_with = "string_or_seq")]
    pub allocation_env: Vec<String>,
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
#[serde(rename_all = "kebab-case")]
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
    /// Build-universe compatibility list (§4.4): the queue only serves configs
    /// whose build universe is listed. `None` = compatible with ALL universes;
    /// an explicitly empty list is a validation error. Gates purely on the
    /// config's *build* universe, exactly like the same-named script-variant
    /// key — hence the `build-` prefix.
    pub build_universes: Option<Vec<String>>,
    /// Per-queue hardware overrides (§4.2): each falls back to the top-level
    /// `[hardware]` value when unset (`Meta::effective_hardware`).
    pub max_cpus_per_node: Option<u32>,
    pub default_cpus_per_task: Option<u32>,
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

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Variants {
    #[serde(default)]
    pub submitscript: ScriptVariants,
    #[serde(default)]
    pub runscript: ScriptVariants,
    #[serde(default)]
    pub optionlist: OptionlistVariants,
}

#[derive(Debug, Default, Deserialize)]
pub struct OptionlistVariants {
    /// Names of `optionlists/<variant>.toml` files; selected at build time
    /// via --variant (§4.4). Their gpu/queue/test flags live in each file's
    /// `[cactup]` header (§7.8), not here.
    #[serde(default)]
    pub variants: Vec<String>,
}

/// One `[variants.submitscript]` / `[variants.runscript]` table (§4.2, §4.4).
///
/// Every key names a variant — except the single reserved *setting*
/// `default-universe` (§4.8 step 4), distinguishable because a variant entry
/// is an array or table while the setting is a string.
#[derive(Debug, Default)]
pub struct ScriptVariants {
    pub variants: IndexMap<String, VariantEntry>,
    /// Universe for variants that don't name their own (§4.8 step 4).
    pub default_universe: Option<String>,
}

/// A variant entry, normalized from the array shorthand (queues only) or the
/// inline-table form (§4.2).
#[derive(Debug, Clone)]
pub struct VariantEntry {
    pub queues: Vec<String>,
    pub universe: Option<String>,
    /// Build-universe compatibility list (§4.4): the variant only serves configs
    /// whose build universe is listed. `None` = compatible with ALL universes;
    /// an explicitly empty list is a validation error. Gates purely on the
    /// config's *build* universe (never the run/submit universe) — hence the
    /// `build-` prefix on the TOML key.
    pub build_universes: Option<Vec<String>>,
    pub test: bool,
    pub default: bool,
    /// Default total task count for runs launched through this script when no
    /// process-layout flag (-n/-T/-t) is given; overrides the fill-the-node
    /// default of §8.5.
    pub tasks: Option<u32>,
}

impl VariantEntry {
    /// Whether this variant may serve a config built in `universe` (§4.4).
    pub fn compatible_with(&self, universe: &str) -> bool {
        self.build_universes
            .as_ref()
            .map(|list| list.iter().any(|u| u == universe))
            .unwrap_or(true)
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum VariantEntryRaw {
    Queues(Vec<String>),
    Full {
        queues: Vec<String>,
        universe: Option<String>,
        #[serde(rename = "build-universes")]
        build_universes: Option<Vec<String>>,
        #[serde(default)]
        test: bool,
        #[serde(default)]
        default: bool,
        tasks: Option<u32>,
    },
}

impl From<VariantEntryRaw> for VariantEntry {
    fn from(raw: VariantEntryRaw) -> Self {
        match raw {
            VariantEntryRaw::Queues(queues) => VariantEntry {
                queues,
                universe: None,
                build_universes: None,
                test: false,
                default: false,
                tasks: None,
            },
            VariantEntryRaw::Full { queues, universe, build_universes, test, default, tasks } => {
                VariantEntry { queues, universe, build_universes, test, default, tasks }
            }
        }
    }
}

impl<'de> Deserialize<'de> for ScriptVariants {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::Error;
        let raw = IndexMap::<String, toml::Value>::deserialize(deserializer)?;
        let mut out = ScriptVariants::default();
        for (key, value) in raw {
            if key == "default-universe" {
                let name = value
                    .as_str()
                    .ok_or_else(|| D::Error::custom("default-universe must be a string"))?;
                out.default_universe = Some(name.to_owned());
            } else {
                let entry: VariantEntryRaw = value.try_into().map_err(|e| {
                    D::Error::custom(format!(
                        "variant \"{key}\" must be a [\"queue\", …] array or a \
                         {{ queues = […], universe = \"…\", build-universes = […], test = …, default = …, tasks = … }} table: {e}"
                    ))
                })?;
                out.variants.insert(key, entry.into());
            }
        }
        Ok(out)
    }
}

/// A universe declaration (§4.8): at most one of the two wrapper forms.
/// Neither wrapper = an identity universe (bare `sh -c` execution) — useful
/// purely as a carrier for env-setup overrides (§4.8/§6.1).
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Universe {
    /// Documentation only; cactup does not switch on it.
    #[allow(dead_code)]
    pub kind: Option<String>,
    /// Prefix form: cactup runs `<wrapper-argv…> /bin/sh -c <inner>`.
    pub wrapper_argv: Option<Vec<String>>,
    /// Template form: one shell command containing exactly one `@COMMAND@`.
    pub wrapper: Option<String>,
    /// Per-universe env-setup overrides (§6.1): each set key replaces the
    /// machine `[environment]` key of the same name (`Meta::effective_env`).
    #[serde(flatten)]
    pub environment: Environment,
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
                    .map(|arg| vars.substitute(arg))
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
                    vars.substitute(template)
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
#[serde(rename_all = "kebab-case")]
pub struct CactupMeta {
    pub origin: Option<Origin>,
}

/// `--from-existing` provenance (§4.7).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Origin {
    pub from: String,
    pub hash: String,
}

impl ScriptVariants {
    /// The variants of one partition — test-marked or normal (§11.2) —
    /// regardless of universe compatibility (validation-side view).
    fn partition_all(&self, test: bool) -> impl Iterator<Item = (&str, &VariantEntry)> {
        self.variants
            .iter()
            .filter(move |(_, e)| e.test == test)
            .map(|(n, e)| (n.as_str(), e))
    }

    /// The variants of one partition that are compatible with `universe`
    /// (§4.4/§11.2) — every selection candidate set is filtered this way.
    pub fn partition<'s>(
        &'s self,
        test: bool,
        universe: &str,
    ) -> impl Iterator<Item = (&'s str, &'s VariantEntry)> {
        self.partition_all(test).filter(move |(_, e)| e.compatible_with(universe))
    }

    /// The default variant of a (universe-filtered) partition: the
    /// `default = true` entry, or the sole variant of the partition (§4.2).
    pub fn partition_default<'s>(&'s self, test: bool, universe: &str) -> Option<(&'s str, &'s VariantEntry)> {
        let mut iter = self.partition(test, universe);
        let first = iter.next()?;
        if iter.next().is_none() {
            return Some(first);
        }
        self.partition(test, universe).find(|(_, e)| e.default)
    }

    /// Select the variant serving `queue` for a config built in `universe`
    /// (§4.4) — pass `HOST_UNIVERSE` when the config records none — honoring
    /// the §11.2 test partitioning (`prefer_test` = this is a `test …`
    /// command) and the `--variant` escape hatch.
    pub fn select(&self, queue: &str, universe: &str, prefer_test: bool, explicit: Option<&str>) -> Res<(&str, &VariantEntry)> {
        let test_set_nonempty = self.partition(true, universe).next().is_some();
        let use_test = prefer_test && test_set_nonempty;

        if let Some(name) = explicit {
            let (name, entry) = self
                .variants
                .get_key_value(name)
                .ok_or_else(|| {
                    anyhow!(
                        "no variant named \"{name}\" (known: {})",
                        self.variants.keys().cloned().collect::<Vec<_>>().join(", ")
                    )
                })?;
            if use_test && !entry.test {
                bail!("variant \"{name}\" is not test-marked, but this machine has test variants (§11.2)");
            }
            if !prefer_test && entry.test {
                bail!("variant \"{name}\" is test-only (test = true) and cannot serve a normal run (§11.2)");
            }
            if !entry.compatible_with(universe) {
                bail!(
                    "variant \"{name}\" is not compatible with build universe \"{universe}\" \
                     (build-universes = [{}]) (§4.4)",
                    entry.build_universes.as_deref().unwrap_or_default().join(", ")
                );
            }
            return Ok((name.as_str(), entry));
        }

        if let Some(found) = self
            .partition(use_test, universe)
            .find(|(_, e)| e.queues.iter().any(|q| q == queue))
        {
            return Ok(found);
        }
        self.partition_default(use_test, universe).ok_or_else(|| {
            anyhow!(
                "no {}variant compatible with universe \"{universe}\" serves queue \"{queue}\" \
                 and none is a compatible default",
                if use_test { "test " } else { "" }
            )
        })
    }
}

impl Meta {
    pub fn script_variants(&self, kind: ScriptKind) -> &ScriptVariants {
        match kind {
            ScriptKind::Submit => &self.variants.submitscript,
            ScriptKind::Run => &self.variants.runscript,
        }
    }

    /// The queue used when -q is omitted, restricted to queues compatible with
    /// the config's build `universe` (§4.4): the `default = true` queue among
    /// them, or the sole compatible queue.
    pub fn default_queue(&self, universe: &str) -> Option<&str> {
        let mut compatible = self.queues.iter().filter(|(_, q)| q.compatible_with(universe));
        let first = compatible.next()?;
        if compatible.next().is_none() {
            return Some(first.0.as_str());
        }
        self.queues
            .iter()
            .find(|(_, q)| q.default && q.compatible_with(universe))
            .map(|(n, _)| n.as_str())
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
    /// `max-cpus-per-node`/`default-cpus-per-task`/`threads-per-cpu`/`memory`
    /// where set, inheriting anything else from the top-level `[hardware]`
    /// table (which is itself optional).
    pub fn effective_hardware(&self, queue: &str) -> Res<Hardware> {
        let q = self.queue(queue)?;
        Ok(Hardware {
            autodetect: self.hardware.autodetect,
            max_cpus_per_node: q.max_cpus_per_node.or(self.hardware.max_cpus_per_node),
            default_cpus_per_task: q.default_cpus_per_task.or(self.hardware.default_cpus_per_task),
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

    /// Effective env block for `phase` when executing in `universe` (§6.1):
    /// the universe's set env keys replace the machine `[environment]` ones
    /// KEY-BY-KEY; unset keys inherit. `None` (bare / --no-universe /
    /// implicit host) and unknown names fall back to the machine env.
    pub fn effective_env(&self, universe: Option<&str>, phase: Phase) -> String {
        let base = &self.environment;
        match universe.and_then(|u| self.universes.get(u)) {
            None => base.effective(phase),
            Some(u) => {
                let over = &u.environment;
                let pick = |o: &Option<String>, b: &Option<String>| o.clone().or_else(|| b.clone());
                Environment {
                    env_setup: pick(&over.env_setup, &base.env_setup),
                    env_build_setup: pick(&over.env_build_setup, &base.env_build_setup),
                    env_submit_setup: pick(&over.env_submit_setup, &base.env_submit_setup),
                    env_run_setup: pick(&over.env_run_setup, &base.env_run_setup),
                }
                .effective(phase)
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

        // Queue build-universe gates (§4.4), mirroring the script-variant lists:
        // non-empty and naming known universes.
        for (name, queue) in &self.queues {
            if let Some(universes) = &queue.build_universes {
                if universes.is_empty() {
                    bail!(
                        "queue \"{name}\" declares an empty build-universes list; omit the key \
                         to be compatible with all universes (§4.4)"
                    );
                }
                for u in universes {
                    if !self.is_known_universe(u) {
                        bail!("queue \"{name}\" lists unknown build universe \"{u}\"");
                    }
                }
            }
        }

        if self.variants.optionlist.variants.is_empty() {
            bail!("[variants.optionlist] lists no variants; at least one optionlist is required");
        }

        for kind in [ScriptKind::Submit, ScriptKind::Run] {
            self.validate_script_kind(kind)
                .with_context(|| format!("in [{}]", kind.table_name()))?;
        }

        for (name, universe) in &self.universes {
            match (&universe.wrapper_argv, &universe.wrapper) {
                (Some(_), Some(_)) => bail!("universe \"{name}\" defines both wrapper-argv and wrapper; pick one (§4.8)"),
                // Neither wrapper = identity universe (§4.8): legal, e.g. as a
                // pure env-setup override carrier.
                (None, None) => {}
                (None, Some(t)) => {
                    if !t.contains("@COMMAND@") {
                        bail!("universe \"{name}\"'s wrapper template does not contain @COMMAND@ (§4.8)")
                    }
                    if let Some(pos) = command_token_in_quotes(t) {
                        bail!(
                            "universe \"{name}\"'s wrapper template puts @COMMAND@ inside a \
                             quoted string (byte {pos}); wrap() supplies @COMMAND@ already \
                             shell-quoted, so it must sit OUTSIDE the template's own quotes \
                             — e.g. `bash -lc '… && '@COMMAND@` (§4.8)"
                        )
                    }
                }
                (Some(_), None) => {}
            }
        }

        // Every universe referenced by name must exist; "host" is always a
        // legal reference (§4.8), declared or implicit.
        let mut refs: Vec<(String, &Option<String>)> = vec![("[build].universe".into(), &self.build.universe)];
        for kind in [ScriptKind::Submit, ScriptKind::Run] {
            let sv = self.script_variants(kind);
            refs.push((format!("[{}].default-universe", kind.table_name()), &sv.default_universe));
            for (vname, entry) in &sv.variants {
                refs.push((format!("[{}] variant \"{vname}\"", kind.table_name()), &entry.universe));
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
    /// universe refs, at most one default (GLOBAL per partition, not
    /// per-universe), per-universe-unambiguous queue mappings, and full queue
    /// coverage for the host context only — for other universes a miss is a
    /// selection-time error (§4.4). The test partition is only checked when
    /// it is non-empty.
    fn validate_script_kind(&self, kind: ScriptKind) -> Res<()> {
        let sv = self.script_variants(kind);

        for (name, entry) in &sv.variants {
            for queue in &entry.queues {
                if !self.queues.contains_key(queue) {
                    bail!("variant \"{name}\" maps unknown queue \"{queue}\"");
                }
            }
            if let Some(universes) = &entry.build_universes {
                if universes.is_empty() {
                    bail!(
                        "variant \"{name}\" declares an empty build-universes list; omit the key \
                         to be compatible with all universes (§4.4)"
                    );
                }
                for u in universes {
                    if !self.is_known_universe(u) {
                        bail!("variant \"{name}\" lists unknown build universe \"{u}\"");
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
            if partition.is_empty() {
                if !test && !self.queues.is_empty() {
                    bail!("no {} variants declared", if test { "test" } else { "normal" });
                }
                continue; // empty test partition: tests borrow the normal one (§11.2)
            }
            let label = if test { "test-partition " } else { "" };

            let defaults: Vec<_> = partition.iter().filter(|(_, e)| e.default).collect();
            if defaults.len() > 1 {
                bail!(
                    "more than one {label}variant is marked default = true: {}",
                    defaults.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                );
            }

            for universe in &contexts {
                for (queue, qdef) in &self.queues {
                    // A queue gated to other build universes cannot be reached
                    // in this context — no variant coverage is owed for it.
                    if !qdef.compatible_with(universe) {
                        continue;
                    }
                    let serving: Vec<_> = partition
                        .iter()
                        .filter(|(_, e)| e.compatible_with(universe) && e.queues.iter().any(|q| q == queue))
                        .collect();
                    if serving.len() > 1 {
                        bail!(
                            "queue \"{queue}\" is mapped by more than one {label}variant \
                             compatible with universe \"{universe}\": {}",
                            serving.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                        );
                    }
                    // Coverage is a load-time obligation only for the host
                    // context (§4.4).
                    if *universe == HOST_UNIVERSE
                        && serving.is_empty()
                        && sv.partition_default(test, universe).is_none()
                    {
                        bail!(
                            "queue \"{queue}\" is served by no {label}variant and none is marked default = true (§4.2)"
                        );
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
    pub fn resolved_paths(&self) -> Res<Paths> {
        let mut vars = VarSet::new();
        vars.set("USER", super::whoami());
        let mut paths = self.paths.clone();
        for (key, path) in [
            ("install-home", &mut paths.install_home),
            ("simulation-home", &mut paths.simulation_home),
            ("test-home", &mut paths.test_home),
            ("scratch-home", &mut paths.scratch_home),
        ] {
            if let Some(value) = path {
                *value = vars
                    .substitute(value)
                    .with_context(|| format!("in [paths].{key}"))?;
            }
        }
        Ok(paths)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A multi-queue, multi-variant machine exercising the §4.2 features
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
        env-build-setup = "module load cmake/3.27"

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
        threads-per-cpu = 2

        [variants.submitscript]
        "slurm-cpu" = { queues = ["checkpt", "single"], default = true }
        "slurm-gpu" = ["gpu"]
        "slurm-test" = { queues = ["checkpt", "single"], test = true, default = true }

        [variants.runscript]
        "cpu" = { queues = ["checkpt", "single"], default = true }
        "gpu-sing" = { queues = ["gpu"], universe = "et-sif" }
        "test-cpu" = { queues = ["checkpt", "single"], test = true, default = true, tasks = 2 }

        [variants.optionlist]
        variants = ["cpu", "gpu", "test-cpu"]

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
        assert_eq!(meta.default_queue(HOST_UNIVERSE), Some("checkpt"));
        assert!(meta.queues["gpu"].gpu);
        assert_eq!(meta.variants.optionlist.variants, ["cpu", "gpu", "test-cpu"]);
        // Array shorthand and inline-table forms normalize identically.
        let sv = &meta.variants.submitscript.variants;
        assert_eq!(sv["slurm-gpu"].queues, ["gpu"]);
        assert!(!sv["slurm-gpu"].test && !sv["slurm-gpu"].default);
        assert!(sv["slurm-test"].test && sv["slurm-test"].default);
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
        // Queue overrides win key-by-key; unset keys still inherit.
        let hw = meta.effective_hardware("gpu").unwrap();
        assert_eq!((hw.max_cpus_per_node, hw.memory, hw.threads_per_cpu()), (Some(64), Some(64000), 2));
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
    fn script_selection_follows_queue_then_default() {
        let meta = mike();
        let rs = meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("gpu", "host", false, None).unwrap().0, "gpu-sing");
        assert_eq!(rs.select("single", "host", false, None).unwrap().0, "cpu");
        // Test commands get the test partition; normal never does (§11.2).
        assert_eq!(rs.select("single", "host", true, None).unwrap().0, "test-cpu");
        assert!(rs.select("single", "host", false, Some("test-cpu")).is_err());
        // With a non-empty test set, --variant must name a test variant.
        assert!(rs.select("single", "host", true, Some("cpu")).is_err());
        assert!(rs.select("single", "host", false, Some("nope")).is_err());
        // gpu-sing carries its universe association (§4.8 step 3).
        assert_eq!(rs.select("gpu", "host", false, None).unwrap().1.universe.as_deref(), Some("et-sif"));
        // test-cpu carries its default-tasks setting (§4.2); cpu has none.
        assert_eq!(rs.select("single", "host", true, None).unwrap().1.tasks, Some(2));
        assert_eq!(rs.select("single", "host", false, None).unwrap().1.tasks, None);
    }

    #[test]
    fn test_partition_falls_back_to_normal_when_empty() {
        let mut meta = mike();
        meta.variants.runscript.variants.shift_remove("test-cpu");
        let rs = meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("single", "host", true, None).unwrap().0, "cpu");
        // And --variant may then name a normal variant.
        assert_eq!(rs.select("gpu", "host", true, Some("gpu-sing")).unwrap().0, "gpu-sing");
    }

    #[test]
    fn validation_rejects_uncovered_queue() {
        let broken = MIKE.replace("\"slurm-cpu\" = { queues = [\"checkpt\", \"single\"], default = true }",
                                  "\"slurm-cpu\" = [\"checkpt\"]");
        let meta: Meta = toml::from_str(&broken).unwrap();
        let err = format!("{:#}", meta.validate("mike").unwrap_err());
        assert!(err.contains("\"single\""), "unexpected error: {err}");
    }

    #[test]
    fn validation_rejects_double_default_and_double_mapping() {
        let double_default = MIKE.replace("\"slurm-gpu\" = [\"gpu\"]",
                                          "\"slurm-gpu\" = { queues = [\"gpu\"], default = true }");
        let meta: Meta = toml::from_str(&double_default).unwrap();
        assert!(meta.validate("mike").is_err());

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
            kind: None,
            wrapper_argv: None,
            wrapper: Some("ssh headnode 'cd @SOURCEDIR@ && '@COMMAND@".to_owned()),
            environment: Environment::default(),
        };
        let WrappedCommand::Shell(cmd) = ssh.wrap(&vars, "echo it's here").unwrap() else {
            panic!("expected shell form");
        };
        assert_eq!(cmd, "ssh headnode 'cd /src && ''echo it'\\''s here'");
    }

    #[test]
    fn default_universe_key_is_a_setting_not_a_variant() {
        let with_default = MIKE.replace(
            "[variants.runscript]",
            "[variants.runscript]\ndefault-universe = \"et-sif\"",
        );
        let meta: Meta = toml::from_str(&with_default).unwrap();
        meta.validate("mike").unwrap();
        let rs = meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.default_universe.as_deref(), Some("et-sif"));
        assert!(!rs.variants.contains_key("default-universe"));
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
        env-build-setup = "module load base-build"

        [queues.gpu]
        gpu = true

        [variants.submitscript]
        "default"   = { queues = ["gpu"], build-universes = ["host"] }
        "sing"      = { queues = ["gpu"], build-universes = ["et-sing", "et-sing-cpu"] }
        "test"      = { queues = ["gpu"], build-universes = ["host"], test = true }
        "sing-test" = { queues = ["gpu"], build-universes = ["et-sing", "et-sing-cpu"], test = true }

        [variants.runscript]
        "default"   = { queues = ["gpu"], build-universes = ["host"] }
        "sing"      = { queues = ["gpu"], build-universes = ["et-sing", "et-sing-cpu"] }

        [variants.optionlist]
        variants = ["native"]

        # Identity universe (§4.8): no wrapper, just a build-env override.
        [universes.host]
        env-build-setup = "module load gcc/9"

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
        let sv = meta.script_variants(ScriptKind::Submit);
        // One queue, four variants: the build universe picks the script (§4.4).
        assert_eq!(sv.select("gpu", "host", false, None).unwrap().0, "default");
        assert_eq!(sv.select("gpu", "et-sing", false, None).unwrap().0, "sing");
        assert_eq!(sv.select("gpu", "et-sing-cpu", false, None).unwrap().0, "sing");
        assert_eq!(sv.select("gpu", "host", true, None).unwrap().0, "test");
        assert_eq!(sv.select("gpu", "et-sing", true, None).unwrap().0, "sing-test");
        // Runscripts have no test partition: tests borrow normal (§11.2),
        // still universe-filtered.
        let rs = meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("gpu", "et-sing-cpu", true, None).unwrap().0, "sing");
    }

    #[test]
    fn universe_miss_is_a_selection_time_error() {
        // A declared universe no variant lists: loads fine (coverage is a
        // load-time obligation only for host — §4.4)…
        let extra = format!("{DB}\n[universes.other]\nwrapper-argv = [\"env\"]\n");
        let meta: Meta = toml::from_str(&extra).unwrap();
        meta.validate("db").unwrap();
        // …but selecting under it fails, naming universe and queue.
        let err = format!(
            "{:#}",
            meta.script_variants(ScriptKind::Run).select("gpu", "other", false, None).unwrap_err()
        );
        assert!(err.contains("\"other\"") && err.contains("\"gpu\""), "{err}");

        // Missing HOST coverage is still a load-time error.
        let no_host = DB
            .replace("\"default\"   = { queues = [\"gpu\"], build-universes = [\"host\"] }\n        \"sing\"      = { queues = [\"gpu\"], build-universes = [\"et-sing\", \"et-sing-cpu\"] }\n\n        [variants.optionlist]",
                     "\"sing\"      = { queues = [\"gpu\"], build-universes = [\"et-sing\", \"et-sing-cpu\"] }\n\n        [variants.optionlist]");
        let meta: Meta = toml::from_str(&no_host).unwrap();
        let err = format!("{:#}", meta.validate("db").unwrap_err());
        assert!(err.contains("served by no"), "{err}");
    }

    #[test]
    fn explicit_variant_incompatible_with_universe_errors() {
        let meta = db();
        let sv = meta.script_variants(ScriptKind::Submit);
        // --variant may pick any compatible variant…
        assert_eq!(sv.select("gpu", "et-sing", false, Some("sing")).unwrap().0, "sing");
        // …but naming an incompatible one is a hard error naming the universe.
        let err = format!("{:#}", sv.select("gpu", "host", false, Some("sing")).unwrap_err());
        assert!(err.contains("\"host\"") && err.contains("et-sing"), "{err}");
    }

    #[test]
    fn per_universe_ambiguity_is_rejected() {
        // Making the runscript "default" compatible with ALL universes (list
        // omitted) makes queue gpu doubly served in the et-sing context.
        let ambiguous = DB.replace(
            "[variants.runscript]\n        \"default\"   = { queues = [\"gpu\"], build-universes = [\"host\"] }",
            "[variants.runscript]\n        \"default\"   = { queues = [\"gpu\"] }",
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

        [variants.submitscript]
        "s" = { queues = ["host-only", "sing"] }

        [variants.runscript]
        "r" = { queues = ["host-only", "sing"] }

        [variants.optionlist]
        variants = ["native"]

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
        assert_eq!(meta.default_queue("host"), Some("host-only"));
        assert_eq!(meta.default_queue("et-sing"), Some("sing"));
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
        let with_ref = MIKE.replace("[variants.runscript]", "[variants.runscript]\ndefault-universe = \"host\"");
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
        // host overrides env-build-setup only; env-setup is inherited (§6.1).
        assert_eq!(meta.effective_env(Some("host"), Phase::Build), "export BASE=1\nmodule load gcc/9");
        // Unset phase keys inherit, concatenation semantics unchanged.
        assert_eq!(meta.effective_env(Some("host"), Phase::Run), "export BASE=1");
        // A universe with no env keys inherits everything.
        assert_eq!(meta.effective_env(Some("et-sing"), Phase::Build), "export BASE=1\nmodule load base-build");
        // Unknown names fall back to the machine env (resolution errors first).
        assert_eq!(meta.effective_env(Some("nope"), Phase::Build), "export BASE=1\nmodule load base-build");
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
