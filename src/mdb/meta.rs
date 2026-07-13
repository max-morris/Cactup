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
/// scripts: `max-tasks-per-node` (simfactory's `ppn`; §8.5 topology +
/// @MAX_TASKS_PER_NODE@), `memory` (@MEMORY@), and `threads-per-cpu`
/// (simfactory's `num-smt`; @THREADS_PER_CPU@). Simfactory's other capacity
/// keys (`min-ppn`, `spn`, `mpn`, `nodes`, `num-threads`, `max-*`,
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
    pub max_tasks_per_node: Option<u32>,
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

#[derive(Debug, Default, Deserialize)]
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
    /// Per-queue hardware overrides (§4.2): each falls back to the top-level
    /// `[hardware]` value when unset (`Meta::effective_hardware`).
    pub max_tasks_per_node: Option<u32>,
    pub threads_per_cpu: Option<u32>,
    /// MB per node.
    pub memory: Option<u64>,
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
    pub test: bool,
    pub default: bool,
    /// Default total task count for runs launched through this script when no
    /// process-layout flag (-n/-T/-t) is given; overrides the fill-the-node
    /// default of §8.5.
    pub tasks: Option<u32>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum VariantEntryRaw {
    Queues(Vec<String>),
    Full {
        queues: Vec<String>,
        universe: Option<String>,
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
                test: false,
                default: false,
                tasks: None,
            },
            VariantEntryRaw::Full { queues, universe, test, default, tasks } => {
                VariantEntry { queues, universe, test, default, tasks }
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
                         {{ queues = […], universe = \"…\", test = …, default = …, tasks = … }} table: {e}"
                    ))
                })?;
                out.variants.insert(key, entry.into());
            }
        }
        Ok(out)
    }
}

/// A universe declaration (§4.8): exactly one of the two wrapper forms.
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
            // Both remaining shapes are rejected by Meta::validate.
            _ => bail!("universe declares neither or both of wrapper-argv/wrapper"),
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
    /// The variants of one partition: test-marked or normal (§11.2).
    pub fn partition(&self, test: bool) -> impl Iterator<Item = (&str, &VariantEntry)> {
        self.variants
            .iter()
            .filter(move |(_, e)| e.test == test)
            .map(|(n, e)| (n.as_str(), e))
    }

    /// The default variant of a partition: the `default = true` entry, or the
    /// sole variant of the partition (§4.2).
    pub fn partition_default(&self, test: bool) -> Option<(&str, &VariantEntry)> {
        let mut iter = self.partition(test);
        let first = iter.next()?;
        if iter.next().is_none() {
            return Some(first);
        }
        self.partition(test).find(|(_, e)| e.default)
    }

    /// Select the variant serving `queue` (§4.4), honoring the §11.2 test
    /// partitioning (`prefer_test` = this is a `test …` command) and the
    /// `--variant` escape hatch.
    pub fn select(&self, queue: &str, prefer_test: bool, explicit: Option<&str>) -> Res<(&str, &VariantEntry)> {
        let test_set_nonempty = self.partition(true).next().is_some();
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
            return Ok((name.as_str(), entry));
        }

        if let Some(found) = self.partition(use_test).find(|(_, e)| e.queues.iter().any(|q| q == queue)) {
            return Ok(found);
        }
        self.partition_default(use_test).ok_or_else(|| {
            anyhow!(
                "no {}variant serves queue \"{queue}\" and none is marked default",
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

    /// The queue used when -q is omitted: the `default = true` queue, or the
    /// sole queue.
    pub fn default_queue(&self) -> Option<&str> {
        if self.queues.len() == 1 {
            return self.queues.keys().next().map(String::as_str);
        }
        self.queues
            .iter()
            .find(|(_, q)| q.default)
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
    /// `max-tasks-per-node`/`threads-per-cpu`/`memory` where set, inheriting anything else from the
    /// top-level `[hardware]` table (which is itself optional).
    pub fn effective_hardware(&self, queue: &str) -> Res<Hardware> {
        let q = self.queue(queue)?;
        Ok(Hardware {
            autodetect: self.hardware.autodetect,
            max_tasks_per_node: q.max_tasks_per_node.or(self.hardware.max_tasks_per_node),
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

    pub fn universe(&self, name: &str) -> Res<&Universe> {
        self.universes.get(name).ok_or_else(|| {
            anyhow!(
                "no universe named \"{name}\" on this machine (known: {})",
                if self.universes.is_empty() {
                    "none".to_owned()
                } else {
                    self.universes.keys().cloned().collect::<Vec<_>>().join(", ")
                }
            )
        })
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
                (None, None) => bail!("universe \"{name}\" defines neither wrapper-argv nor wrapper (§4.8)"),
                (None, Some(t)) if !t.contains("@COMMAND@") => {
                    bail!("universe \"{name}\"'s wrapper template does not contain @COMMAND@ (§4.8)")
                }
                _ => {}
            }
        }

        // Every universe referenced by name must exist.
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
                && !self.universes.contains_key(name)
            {
                bail!("{what} names unknown universe \"{name}\" (known: {})",
                    if self.universes.is_empty() { "none".to_owned() }
                    else { self.universes.keys().cloned().collect::<Vec<_>>().join(", ") });
            }
        }

        Ok(())
    }

    /// Per-kind §4.2 checks, applied per §11.2 partition: sane queue refs, at
    /// most one default, unambiguous queue mappings, and full queue coverage
    /// (the test partition is only checked when it is non-empty).
    fn validate_script_kind(&self, kind: ScriptKind) -> Res<()> {
        let sv = self.script_variants(kind);

        for (name, entry) in &sv.variants {
            for queue in &entry.queues {
                if !self.queues.contains_key(queue) {
                    bail!("variant \"{name}\" maps unknown queue \"{queue}\"");
                }
            }
        }

        for test in [false, true] {
            let partition: Vec<_> = sv.partition(test).collect();
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

            for (queue, _) in &self.queues {
                let serving: Vec<_> = partition
                    .iter()
                    .filter(|(_, e)| e.queues.iter().any(|q| q == queue))
                    .collect();
                if serving.len() > 1 {
                    bail!(
                        "queue \"{queue}\" is mapped by more than one {label}variant: {}",
                        serving.iter().map(|(n, _)| *n).collect::<Vec<_>>().join(", ")
                    );
                }
                if serving.is_empty() && sv.partition_default(test).is_none() {
                    bail!(
                        "queue \"{queue}\" is served by no {label}variant and none is marked default = true (§4.2)"
                    );
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
        max-tasks-per-node = 16
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
        max-tasks-per-node = 64
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
        assert_eq!(meta.default_queue(), Some("checkpt"));
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
        assert_eq!((hw.max_tasks_per_node, hw.memory, hw.threads_per_cpu()), (Some(16), Some(64000), 1));
        // Queue overrides win key-by-key; unset keys still inherit.
        let hw = meta.effective_hardware("gpu").unwrap();
        assert_eq!((hw.max_tasks_per_node, hw.memory, hw.threads_per_cpu()), (Some(64), Some(64000), 2));
        assert!(meta.effective_hardware("nope").is_err());
    }

    #[test]
    fn hardware_table_is_optional_when_queues_define_it() {
        // No top-level [hardware] at all: queue-level keys carry the load.
        let toml_text = MIKE
            .replace("[hardware]\n        max-tasks-per-node = 16\n        memory = 64000", "")
            .replace("[queues.checkpt]", "[queues.checkpt]\nmax-tasks-per-node = 32\nmemory = 128000");
        let meta: Meta = toml::from_str(&toml_text).unwrap();
        meta.validate("mike").unwrap();
        let hw = meta.effective_hardware("checkpt").unwrap();
        assert_eq!((hw.max_tasks_per_node, hw.memory), (Some(32), Some(128000)));
        // A queue defining nothing gets the (empty) machine-wide values.
        let hw = meta.effective_hardware("single").unwrap();
        assert_eq!((hw.max_tasks_per_node, hw.memory), (None, None));
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
        assert_eq!(rs.select("gpu", false, None).unwrap().0, "gpu-sing");
        assert_eq!(rs.select("single", false, None).unwrap().0, "cpu");
        // Test commands get the test partition; normal never does (§11.2).
        assert_eq!(rs.select("single", true, None).unwrap().0, "test-cpu");
        assert!(rs.select("single", false, Some("test-cpu")).is_err());
        // With a non-empty test set, --variant must name a test variant.
        assert!(rs.select("single", true, Some("cpu")).is_err());
        assert!(rs.select("single", false, Some("nope")).is_err());
        // gpu-sing carries its universe association (§4.8 step 3).
        assert_eq!(rs.select("gpu", false, None).unwrap().1.universe.as_deref(), Some("et-sif"));
        // test-cpu carries its default-tasks setting (§4.2); cpu has none.
        assert_eq!(rs.select("single", true, None).unwrap().1.tasks, Some(2));
        assert_eq!(rs.select("single", false, None).unwrap().1.tasks, None);
    }

    #[test]
    fn test_partition_falls_back_to_normal_when_empty() {
        let mut meta = mike();
        meta.variants.runscript.variants.shift_remove("test-cpu");
        let rs = meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("single", true, None).unwrap().0, "cpu");
        // And --variant may then name a normal variant.
        assert_eq!(rs.select("gpu", true, Some("gpu-sing")).unwrap().0, "gpu-sing");
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
