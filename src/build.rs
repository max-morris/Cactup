//! The build engine behind `cactup config build` (spec §7):
//! optionlist selection + render + flag injection (§7.8), thornlist toggles
//! (§7.5, D8), env-setup'd `make` driving (§7.2, §6.1), build universes
//! (§4.8), the rebuild-decision snapshot diff (§7.8), the per-config build
//! lock (§2.3 item 4), and `cactup-config.toml` metadata (§7.4).

use crate::args::{BuildOpts, MakeJobs};
use crate::database::SCHEMA;
use crate::installation::Installation;
use crate::lock::LinkLock;
use crate::mdb::meta::Phase;
use crate::mdb::{Machine, Optionlist};
use crate::template::{VarSet, VarValue};
use crate::Res;
use anyhow::{bail, Context};
use chrono::{DateTime, Utc};
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

/// Make command used when a machine's `meta.toml` omits `[build].make`. It
/// templates `@MAKEJOBS@` so `[build].make-jobs` (§7.6) is honored as the
/// default `-j` without every machine having to hand-write the token.
const DEFAULT_MAKE: &str = "make -j@MAKEJOBS@";

/// The `-j max` expansion: count the CPUs available to the build shell at
/// runtime. `nproc` honors the process's cpuset/affinity, so inside an
/// srun/singularity wrapper it reports that allocation, not the login node.
/// The `|| echo 1` fallback matters for safety: if `nproc` were missing, a bare
/// `make -j` (empty count) means *unbounded* parallelism, so we degrade to 1.
const MAX_MAKEJOBS: &str = "$(nproc 2>/dev/null || echo 1)";

/// Resolve the `@MAKEJOBS@` build variable (§7.6): `--make-jobs` > machine
/// `make-jobs` > 1. `-j max` becomes `MAX_MAKEJOBS`, a shell expression the
/// build shell itself evaluates — inside the universe wrapper when there is one
/// — so it counts the CPUs actually available in that context rather than on
/// the login node cactup is invoked on. Any explicit count is a plain integer.
fn make_jobs_var(cli: Option<MakeJobs>, machine_default: Option<u32>) -> VarValue {
    match cli {
        Some(MakeJobs::Max) => VarValue::Str(MAX_MAKEJOBS.to_owned()),
        Some(MakeJobs::Count(n)) => VarValue::Int(n as i64),
        None => VarValue::Int(machine_default.unwrap_or(1) as i64),
    }
}

fn default_schema() -> u32 {
    SCHEMA
}
fn default_true() -> bool {
    true
}

/// `configs/<name>/cactup-config.toml` (§7.4). `built` is a cactup extension
/// used by `config list`/`show` and the §7.1 most-recently-built repoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct ConfigMeta {
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub name: String,
    /// Optionlist variant used.
    pub variant: String,
    /// Snapshotted from the optionlist `[cactup]` header at build time (D12).
    #[serde(default)]
    pub gpu: bool,
    #[serde(default)]
    pub compatible_queues: Vec<String>,
    pub thornlist: String,
    /// A build is not portable across machines.
    pub machine: String,
    /// Resolved build universe; omitted for the host context (§4.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub universe: Option<String>,
    #[serde(default = "default_true")]
    pub coerce_run_universe: bool,
    pub config_id: String,
    pub build_id: String,
    #[serde(default)]
    pub built: Option<DateTime<Utc>>,
    #[serde(default)]
    pub flags: BuildFlags,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct BuildFlags {
    #[serde(default)]
    pub debug: bool,
    #[serde(default = "default_true")]
    pub optimize: bool,
    #[serde(rename = "unsafe", default)]
    pub unsafe_build: bool,
    #[serde(default)]
    pub profile: bool,
}

/// Optimized is the one flag that defaults ON (§7.6) — must match the serde
/// default above so a missing `[flags]` table and `BuildFlags::default()`
/// agree.
impl Default for BuildFlags {
    fn default() -> Self {
        BuildFlags { debug: false, optimize: true, unsafe_build: false, profile: false }
    }
}

impl ConfigMeta {
    pub fn path_for(cactus_root: &Path, name: &str) -> PathBuf {
        cactus_root.join("configs").join(name).join("cactup-config.toml")
    }

    pub fn load(cactus_root: &Path, name: &str) -> Res<Option<ConfigMeta>> {
        let path = Self::path_for(cactus_root, name);
        let text = match fs::read_to_string(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            other => other.with_context(|| format!("Failed to read {}", path.display()))?,
        };
        let meta: ConfigMeta =
            toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;
        if meta.schema > SCHEMA {
            bail!(
                "{} has schema {} (> {SCHEMA}); please upgrade cactup",
                path.display(),
                meta.schema
            );
        }
        Ok(Some(meta))
    }

    fn store(&self, cactus_root: &Path) -> Res<()> {
        let path = Self::path_for(cactus_root, &self.name);
        fs::write(&path, toml::to_string_pretty(self)?)
            .with_context(|| format!("Failed to write {}", path.display()))
    }
}

/// Apply the §7.5 (D8) machine thorn toggles to a thornlist's contents:
/// `disabled-thorns` entries get a `#DISABLED ` prefix, `enabled-thorns`
/// entries get it removed. Entries match a thorn line's `arrangement/Thorn`
/// (or bare thorn name after `/`).
pub fn apply_thorn_toggles(thornlist: &str, enabled: &[String], disabled: &[String]) -> String {
    let matches = |spec: &str, thorn: &str| -> bool {
        thorn == spec || thorn.rsplit('/').next() == Some(spec)
    };
    thornlist
        .lines()
        .map(|line| {
            let bare = line.strip_prefix("#DISABLED ").unwrap_or(line);
            let thorn = bare.trim();
            if thorn.is_empty() || thorn.starts_with('#') || thorn.starts_with('!') {
                return line.to_owned();
            }
            if disabled.iter().any(|d| matches(d, thorn)) {
                format!("#DISABLED {bare}")
            } else if line.starts_with("#DISABLED ") && enabled.iter().any(|e| matches(e, thorn)) {
                bare.to_owned()
            } else {
                line.to_owned()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
        + "\n"
}

/// §7.8 rule 5: inject the effective build flags into the rendered native
/// optionlist — replace an existing `KEY = …` line or append. Note the
/// deliberate American→British mapping: `optimize` drives Cactus's
/// `OPTIMISE` key (external identifier, emitted verbatim).
pub fn inject_build_flags(rendered: &str, flags: BuildFlags) -> String {
    let mut out: Vec<String> = rendered.lines().map(str::to_owned).collect();
    for (key, on) in [
        ("DEBUG", flags.debug),
        ("OPTIMISE", flags.optimize),
        ("UNSAFE", flags.unsafe_build),
        ("PROFILE", flags.profile),
    ] {
        let value = format!("{key} = {}", if on { "yes" } else { "no" });
        match out.iter_mut().find(|l| {
            l.split_once('=')
                .is_some_and(|(k, _)| k.trim() == key)
        }) {
            Some(line) => *line = value,
            None => out.push(value),
        }
    }
    out.join("\n") + "\n"
}

/// The effective flag set: CLI > stored config metadata > default (§7.6).
/// CLI booleans can only turn a flag ON; turning one off means editing the
/// optionlist or rebuilding fresh.
pub fn effective_flags(opts: &BuildOpts, stored: Option<BuildFlags>) -> BuildFlags {
    let base = stored.unwrap_or_default();
    BuildFlags {
        debug: opts.debug || base.debug,
        optimize: opts.optimize || base.optimize,
        unsafe_build: opts.unsafe_build || base.unsafe_build,
        profile: opts.profile || base.profile,
    }
}

/// Why (or whether) a rebuild must run from scratch (§7.8).
#[derive(Debug, PartialEq)]
pub enum RebuildDecision {
    /// No config on disk yet.
    Fresh,
    /// Optionlist TOML or universe unchanged: plain incremental `make`.
    Incremental,
    /// Any optionlist diff or universe change: realclean + reconfigure + build.
    Full(&'static str),
}

pub fn rebuild_decision(
    stored_optionlist: Option<&str>,
    fresh_optionlist: &str,
    stored_universe: Option<&str>,
    resolved_universe: Option<&str>,
) -> RebuildDecision {
    match stored_optionlist {
        None => RebuildDecision::Fresh,
        Some(stored) if stored != fresh_optionlist => {
            RebuildDecision::Full("the optionlist changed")
        }
        Some(_) if stored_universe != resolved_universe => {
            RebuildDecision::Full("the build universe changed")
        }
        Some(_) => RebuildDecision::Incremental,
    }
}

/// Resolve the BUILD universe name per §4.8 precedence (steps 1, 3, 4):
/// CLI → optionlist → [build].universe → declared host → None.
/// `--no-universe` stays the true bare escape hatch, bypassing even a
/// declared host; `host_declared` only matters as the final fallback, so
/// machines without `[universes.host]` keep resolving to None (implicit host
/// ≡ identity ≡ bare execution).
pub fn resolve_build_universe<'a>(
    opts: &'a BuildOpts,
    optionlist_universe: Option<&'a str>,
    machine_build_universe: Option<&'a str>,
    host_declared: bool,
) -> Option<&'a str> {
    if opts.universe.no_universe {
        return None;
    }
    opts.universe
        .universe
        .as_deref()
        .or(optionlist_universe)
        .or(machine_build_universe)
        .or(host_declared.then_some(crate::mdb::HOST_UNIVERSE))
}

pub struct BuildOutcome {
    pub meta: ConfigMeta,
    pub rebuilt: bool,
}

/// Run `config build` for `name` (§7). Returns the stored
/// metadata. The global DB is never touched here (§2.3); the caller updates
/// the active-config pointer afterwards.
pub fn build(
    installation: &Installation,
    machine: &Machine,
    name: &str,
    opts: &BuildOpts,
) -> Res<BuildOutcome> {
    let cactus_root = installation.cactus_root();
    if !cactus_root.is_dir() {
        bail!("no Cactus tree at {}", cactus_root.display());
    }
    let config_dir = cactus_root.join("configs").join(name);

    // Selection & inputs (§4.4, §7.8).
    let variant = machine.select_optionlist(opts.variant.as_deref())?;
    let optionlist = Optionlist::load(&machine.optionlist_path(&variant))?;
    let universe_name = resolve_build_universe(
        opts,
        optionlist.header.universe.as_deref(),
        machine.meta.build.universe.as_deref(),
        machine.meta.declared_host().is_some(),
    )
    .map(str::to_owned);
    // Unknown universe = hard error listing the known ones (§4.8).
    let universe = universe_name
        .as_deref()
        .map(|u| machine.meta.universe(u))
        .transpose()?;

    let thornlist_path = match &opts.thornlist {
        Some(path) => path.clone(),
        None => cactus_root.join("thornlists/einsteintoolkit.th"),
    };
    let thornlist_text = fs::read_to_string(&thornlist_path)
        .with_context(|| format!("Failed to read thornlist {}", thornlist_path.display()))?;
    // Machine-level thorn toggles (§7.5) plus this optionlist variant's own
    // (§7.8): the variant augments the machine, so one machine can carry build
    // flavors that disable different thorns (e.g. a CUDA variant dropping
    // thorns that won't compile with nvcc). apply_thorn_toggles checks
    // `disabled` before `enabled`, so a variant disable wins over a machine
    // enable of the same thorn.
    let enabled_thorns: Vec<String> = machine
        .meta
        .build
        .enabled_thorns
        .iter()
        .chain(&optionlist.header.enabled_thorns)
        .cloned()
        .collect();
    let disabled_thorns: Vec<String> = machine
        .meta
        .build
        .disabled_thorns
        .iter()
        .chain(&optionlist.header.disabled_thorns)
        .cloned()
        .collect();
    let thornlist_processed = apply_thorn_toggles(&thornlist_text, &enabled_thorns, &disabled_thorns);

    let stored_meta = ConfigMeta::load(&cactus_root, name)?;
    if let Some(stored) = &stored_meta
        && stored.variant != variant
        && opts.variant.is_none()
    {
        bail!(
            "config \"{name}\" was built with variant \"{}\"; pass --variant explicitly to change it",
            stored.variant
        );
    }
    let flags = effective_flags(opts, stored_meta.as_ref().map(|m| m.flags));

    // Rebuild decision (§7.8): diff the SOURCE TOML snapshot + the universe.
    let snapshot_path = config_dir.join("cactup-optionlist.toml");
    let stored_optionlist = fs::read_to_string(&snapshot_path).ok();
    let mut decision = rebuild_decision(
        stored_optionlist.as_deref(),
        &optionlist.source,
        stored_meta.as_ref().and_then(|m| m.universe.as_deref()),
        universe_name.as_deref(),
    );
    if opts.force || opts.reconfig {
        decision = RebuildDecision::Full("-f/--reconfig given");
    } else if decision == RebuildDecision::Incremental
        && is_complete(&cactus_root, name)
        && let Some(stored) = stored_meta.clone()
    {
        println!(
            "Config {} is up to date (same optionlist, same universe); pass -f to rebuild.",
            name
        );
        return Ok(BuildOutcome { meta: stored, rebuilt: false });
    }

    // Build-context variables (§6.3, build-time set).
    let mut vars = VarSet::new();
    vars.set("MAKEJOBS", make_jobs_var(opts.make_jobs, machine.meta.build.make_jobs));
    vars.set("USER", std::env::var("USER").unwrap_or_default());
    vars.set("SOURCEDIR", cactus_root.display().to_string());
    vars.set("CONFIGURATION", name);
    // Resolve @USER@/@ENV()@ in scratch-home (§4.2) — the raw template would
    // otherwise leak `@USER@` literally, since substitution is single-pass and
    // never re-scans a spliced value. Matches the sim path (sim/vars.rs).
    vars.set(
        "SCRATCH_HOME",
        machine.meta.resolved_paths()?.scratch_home.unwrap_or_default(),
    );
    // Several machines' make commands / build universes reference
    // @ALLOCATION@ (e.g. mike's and Deep Bayou's `srun … singularity exec`
    // build wrappers); bind it from the allocation knob the way the sim path
    // does, empty when unset.
    let allocation = crate::database::Db::open()
        .and_then(|db| db.read())
        .map(|db| db.knob("allocation").unwrap_or("").to_owned())
        .unwrap_or_default();
    vars.set("ALLOCATION", allocation);

    // Rendered native optionlist: render → inject flags → substitute (§7.8).
    let rendered = vars
        .substitute(&inject_build_flags(&optionlist.render(), flags))
        .context("substituting the rendered optionlist")?;

    fs::create_dir_all(&config_dir)
        .with_context(|| format!("Failed to create {}", config_dir.display()))?;
    // Staging cactup's files makes configs/<name> exist before Cactus's
    // setup_configuration.pl ever runs, so it takes its "Reconfiguring"
    // branch — which chdirs into the skeleton only the new-config branch
    // creates. Create that skeleton ourselves, or the first configure dies
    // with "Internal error - couldn't enter '…/config-data'".
    for sub in ["build", "lib", "scratch", "config-data"] {
        fs::create_dir_all(config_dir.join(sub))
            .with_context(|| format!("Failed to create {}", config_dir.join(sub).display()))?;
    }
    let rendered_path = config_dir.join("cactup-optionlist.cfg");
    fs::write(&rendered_path, &rendered)?;
    let thornlist_out = config_dir.join("cactup-thornlist.th");
    fs::write(&thornlist_out, &thornlist_processed)?;
    // Every build's combined output is teed here so a failure leaves something
    // to read once the terminal scrollback is gone (§7.2).
    let build_log = config_dir.join("cactup-build.log");

    // Per-config build lock, heartbeat-kept across the (long) make (§2.3 #4).
    let _build_lock = LinkLock::acquire(&config_dir.join(".cactup-build.lock"))?.with_heartbeat();

    if let Some(prebuilt) = &opts.virtual_executable {
        // §7.7: virtual/prebuilt executable — copy into place, skip make.
        let exe_dir = cactus_root.join("exe");
        fs::create_dir_all(&exe_dir)?;
        fs::copy(prebuilt, exe_dir.join(format!("cactus_{name}")))
            .with_context(|| format!("Failed to copy {}", prebuilt.display()))?;
    } else {
        // The default (`DEFAULT_MAKE`) templates @MAKEJOBS@ so
        // `[build].make-jobs` (§7.6: --make-jobs > machine make-jobs > 1) is
        // honored as the default -j even on machines that don't hand-write a
        // custom `make` key. A machine that sets its own `make` keeps full
        // control of parallelism.
        let make = vars
            .substitute(machine.meta.build.make.as_deref().unwrap_or(DEFAULT_MAKE))
            .context("substituting the machine make command")?;

        let mut steps: Vec<String> = Vec::new();
        if matches!(decision, RebuildDecision::Full(_)) && is_configured(&cactus_root, name) {
            steps.push(format!("{make} {name}-realclean"));
        }
        steps.push(format!(
            "echo yes | {make} {name}-config options={} THORNLIST={}",
            sh_quote(&rendered_path),
            sh_quote(&thornlist_out),
        ));
        if opts.clean {
            steps.push(format!("{make} {name}-clean"));
        }
        steps.push(format!("{make} {name}"));
        steps.push(format!("{make} {name}-utils"));

        // Build-phase env for the resolved universe (§6.1): universe env keys
        // override the machine [environment] key-by-key.
        let env = machine.meta.effective_env(universe_name.as_deref(), Phase::Build);
        let snippet = format!(
            "set -e\ncd {}\n{}{}",
            sh_quote(&cactus_root),
            if env.is_empty() { String::new() } else { format!("{env}\n") },
            steps.join("\n")
        );
        run_build_snippet(&snippet, universe, &vars, &build_log)?;
    }

    if !is_complete(&cactus_root, name) {
        let marker = completeness_marker(&cactus_root, name);
        let log_hint = if build_log.exists() {
            eprintln!(
                "\n{} the build finished but {} is missing — the config is incomplete\n{} {}",
                "✗".red().bold(),
                marker.display(),
                "→ build log:".red().bold(),
                build_log.display(),
            );
            format!("; see {}", build_log.display())
        } else {
            String::new()
        };
        bail!(
            "the build finished but {} is missing — the config is incomplete{log_hint}",
            marker.display(),
        );
    }

    // Metadata + rebuild snapshot (§7.4, §7.8).
    let now = Utc::now();
    let meta = ConfigMeta {
        schema: SCHEMA,
        name: name.to_owned(),
        variant: variant.clone(),
        gpu: optionlist.header.gpu,
        compatible_queues: optionlist.header.compatible_queues.clone(),
        thornlist: thornlist_path.display().to_string(),
        machine: machine.name.clone(),
        universe: universe_name,
        coerce_run_universe: optionlist.header.coerce_run_universe,
        // config-id is stable across rebuilds; build-id is per-build (§7.4).
        config_id: stored_meta
            .map(|m| m.config_id)
            .unwrap_or_else(|| generate_id("config", name, &machine.name, now)),
        build_id: generate_id("build", name, &machine.name, now),
        built: Some(now),
        flags,
    };
    meta.store(&cactus_root)?;
    fs::write(&snapshot_path, &optionlist.source)
        .with_context(|| format!("Failed to write {}", snapshot_path.display()))?;

    Ok(BuildOutcome { meta, rebuilt: true })
}

/// `config-data/cctk_Config.h` presence ⇒ configured; + executable ⇒ complete
/// (§7.2).
fn is_configured(cactus_root: &Path, name: &str) -> bool {
    completeness_marker(cactus_root, name).is_file()
}

fn completeness_marker(cactus_root: &Path, name: &str) -> PathBuf {
    cactus_root
        .join("configs")
        .join(name)
        .join("config-data/cctk_Config.h")
}

pub fn executable_path(cactus_root: &Path, name: &str) -> PathBuf {
    cactus_root.join("exe").join(format!("cactus_{name}"))
}

pub fn is_complete(cactus_root: &Path, name: &str) -> bool {
    is_configured(cactus_root, name) && executable_path(cactus_root, name).is_file()
}

fn generate_id(kind: &str, name: &str, machine: &str, now: DateTime<Utc>) -> String {
    // A per-process counter disambiguates ids minted within the same second
    // (e.g. back-to-back rebuilds): the executable cache is keyed by build-id
    // (§8.1), so two distinct builds must never share one.
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!(
        "{kind}-{name}-{machine}-{}-{}.{seq}",
        now.format("%Y.%m.%d-%H.%M.%S"),
        std::process::id()
    )
}

fn sh_quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
}

/// Run the env-setup'd make snippet, wrapped in the build universe when one
/// is resolved (§7.2, §4.8). Build output streams to the terminal *and* is
/// teed (stdout+stderr, combined) to `log_path`, so a failed build leaves a
/// persistent record to read after the terminal scrollback is gone. On
/// failure we point the user at that log loudly, on stderr, before bailing.
fn run_build_snippet(
    snippet: &str,
    universe: Option<&crate::mdb::Universe>,
    vars: &VarSet,
    log_path: &Path,
) -> Res<()> {
    let mut cmd = match universe {
        None => {
            let mut c = Command::new("/bin/sh");
            c.args(["-c", snippet]);
            c
        }
        Some(u) => match u.wrap(vars, snippet)? {
            crate::mdb::WrappedCommand::Shell(shell_cmd) => {
                let mut c = Command::new("/bin/sh");
                c.args(["-c", &shell_cmd]);
                c
            }
            crate::mdb::WrappedCommand::Argv(argv) => {
                let mut c = Command::new(&argv[0]);
                c.args(&argv[1..]);
                c
            }
        },
    };
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    crate::shell::trace_command(&cmd);

    // One combined log per build; File::create truncates any prior attempt.
    let log = Arc::new(Mutex::new(
        fs::File::create(log_path)
            .with_context(|| format!("Failed to create build log {}", log_path.display()))?,
    ));
    let mut child = cmd.spawn().context("Failed to spawn the build shell")?;

    // Mirror one child stream to a terminal fd and the shared log. stdout and
    // stderr keep their own destinations on-screen; both interleave into the
    // single log file (ordering approximate across the two streams, as in a
    // shell `2>&1`-style tee).
    fn tee(
        mut src: impl std::io::Read + Send + 'static,
        log: Arc<Mutex<fs::File>>,
        to_stderr: bool,
    ) -> std::thread::JoinHandle<()> {
        std::thread::spawn(move || {
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
                        if let Ok(mut f) = log.lock() {
                            let _ = f.write_all(chunk);
                        }
                    }
                }
            }
        })
    }

    let copiers = [
        tee(child.stdout.take().expect("stdout piped"), Arc::clone(&log), false),
        tee(child.stderr.take().expect("stderr piped"), Arc::clone(&log), true),
    ];
    let status = child.wait().context("Failed to wait on the build shell")?;
    for t in copiers {
        let _ = t.join();
    }

    if !status.success() {
        eprintln!(
            "\n{} the build failed ({status})\n{} {}",
            "✗".red().bold(),
            "→ build log:".red().bold(),
            log_path.display(),
        );
        bail!("the build failed ({status}); see {}", log_path.display());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mdb::Mdb;

    #[test]
    fn thorn_toggles() {
        let list = "# comment\nCactusBase/IOUtil\n#DISABLED McLachlan/ML_BSSN\nCarpetX/CarpetX\n";
        let out = apply_thorn_toggles(
            list,
            &["ML_BSSN".to_owned()],
            &["CarpetX/CarpetX".to_owned()],
        );
        assert_eq!(
            out,
            "# comment\nCactusBase/IOUtil\nMcLachlan/ML_BSSN\n#DISABLED CarpetX/CarpetX\n"
        );
        // Toggles are idempotent.
        assert_eq!(
            apply_thorn_toggles(&out, &["ML_BSSN".into()], &["CarpetX/CarpetX".into()]),
            out
        );
    }

    #[test]
    fn flag_injection_replaces_or_appends() {
        let rendered = "VERSION = 2020\nDEBUG = yes\nCC = gcc\n";
        let out = inject_build_flags(rendered, BuildFlags::default());
        assert!(out.contains("DEBUG = no"), "{out}");
        assert!(out.contains("OPTIMISE = yes"), "{out}");
        assert!(out.contains("UNSAFE = no") && out.contains("PROFILE = no"));
        assert!(out.starts_with("VERSION = 2020\n"), "order preserved: {out}");
        assert_eq!(out.matches("DEBUG").count(), 1, "replaced, not duplicated");
    }

    #[test]
    fn default_make_honors_make_jobs() {
        // When a machine omits `[build].make`, the default templates @MAKEJOBS@
        // so the resolved -j tracks `[build].make-jobs` (here, 6).
        let mut vars = VarSet::new();
        vars.set("MAKEJOBS", 6u64);
        assert_eq!(vars.substitute(DEFAULT_MAKE).unwrap(), "make -j6");
    }

    #[test]
    fn make_jobs_precedence_and_max() {
        // --make-jobs wins over the machine default.
        assert_eq!(make_jobs_var(Some(MakeJobs::Count(12)), Some(4)), VarValue::Int(12));
        // Falls back to the machine make-jobs, then to 1.
        assert_eq!(make_jobs_var(None, Some(4)), VarValue::Int(4));
        assert_eq!(make_jobs_var(None, None), VarValue::Int(1));
        // `-j max` resolves to a runtime nproc, so the build shell counts the
        // CPUs available in whatever context it runs in.
        assert_eq!(make_jobs_var(Some(MakeJobs::Max), Some(4)), VarValue::Str(MAX_MAKEJOBS.into()));

        // Substituted into the default make command it yields a live expansion.
        let mut vars = VarSet::new();
        vars.set("MAKEJOBS", make_jobs_var(Some(MakeJobs::Max), None));
        assert_eq!(
            vars.substitute(DEFAULT_MAKE).unwrap(),
            "make -j$(nproc 2>/dev/null || echo 1)"
        );
    }

    #[test]
    fn build_universe_precedence_and_host_fallback() {
        let mut opts = BuildOpts::default_for_tests();
        // §4.8: CLI → optionlist → [build].universe → declared host → None.
        assert_eq!(resolve_build_universe(&opts, None, None, false), None);
        assert_eq!(resolve_build_universe(&opts, None, None, true), Some("host"));
        assert_eq!(resolve_build_universe(&opts, None, Some("m"), true), Some("m"));
        assert_eq!(resolve_build_universe(&opts, Some("o"), Some("m"), true), Some("o"));
        opts.universe.universe = Some("cli".to_owned());
        assert_eq!(resolve_build_universe(&opts, Some("o"), Some("m"), true), Some("cli"));
        // --no-universe is the true bare escape hatch: it bypasses everything,
        // including a declared host.
        opts.universe.no_universe = true;
        assert_eq!(resolve_build_universe(&opts, Some("o"), Some("m"), true), None);
    }

    #[test]
    fn rebuild_decisions() {
        use RebuildDecision as R;
        assert_eq!(rebuild_decision(None, "x", None, None), R::Fresh);
        assert_eq!(rebuild_decision(Some("x"), "x", None, None), R::Incremental);
        assert!(matches!(rebuild_decision(Some("x"), "y", None, None), R::Full(_)));
        assert!(matches!(
            rebuild_decision(Some("x"), "x", Some("et-sif"), None),
            R::Full(_)
        ));
        assert_eq!(
            rebuild_decision(Some("x"), "x", Some("u"), Some("u")),
            R::Incremental
        );
    }

    /// End-to-end against a fake Cactus tree whose machine `make` is a shell
    /// function-free stub script that records its invocations and fabricates
    /// the completeness markers.
    #[test]
    fn build_drives_make_and_writes_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists/einsteintoolkit.th"), "A/B\nC/D\n").unwrap();

        // Fake make: log every call; on `<name>-config` / `<name>` fabricate
        // the marker / executable. The `-config` step emulates the crucial
        // setup_configuration.pl behavior: configs/sim already exists (cactup
        // staged files there), so its "Reconfiguring" branch runs, which
        // requires the config-data skeleton to exist already — it chdirs
        // instead of creating it.
        let fake_make = root.join("fakemake");
        fs::write(
            &fake_make,
            format!(
                "#!/bin/sh\necho \"$@\" >> {}/make.log\necho \"fake-make: $@\"\ncase \"$2\" in\n\
                 sim-config) cd {}/configs/sim/config-data || \
                 {{ echo \"Internal error - couldn't enter config-data\"; exit 1; }}; \
                 touch cctk_Config.h ;;\n\
                 sim) mkdir -p {}/exe && touch {}/exe/cactus_sim ;;\nesac\n",
                root.display(), cactus.display(), cactus.display(), cactus.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_make, fs::Permissions::from_mode(0o755)).unwrap();
        }

        // A machine whose make points at the stub and which disables C/D.
        let machine_dir = root.join("mdb/fake");
        fs::create_dir_all(machine_dir.join("optionlists")).unwrap();
        fs::create_dir_all(machine_dir.join("runscripts")).unwrap();
        fs::create_dir_all(machine_dir.join("submitscripts")).unwrap();
        fs::write(
            machine_dir.join("meta.toml"),
            format!(
                r#"
                [machine]
                nickname = "fake"
                [build]
                make = "{} -j@MAKEJOBS@"
                make-jobs = 4
                disabled-thorns = ["C/D"]
                [environment]
                env-setup = "CACTUP_BUILD_ENV=on"
                [queues.local]
                default = true
                [variants.submitscript]
                "default" = ["local"]
                [variants.runscript]
                "default" = ["local"]
                [variants.optionlist]
                variants = ["default"]
                "#,
                fake_make.display()
            ),
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n[options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        for s in ["runscripts/default.sh", "submitscripts/default.sh"] {
            fs::write(machine_dir.join(s), "#!/bin/sh\n").unwrap();
        }

        let mdb = Mdb::with_roots(root.join("mdb"), PathBuf::from("/nonexistent"));
        let machine = mdb.load("fake").unwrap();
        let inst = Installation::new("et", root.join("inst"));
        let opts = BuildOpts::default_for_tests();

        let outcome = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(outcome.rebuilt);
        let meta = &outcome.meta;
        assert_eq!(meta.variant, "default");
        assert_eq!(meta.compatible_queues, ["local"]);
        assert_eq!(meta.machine, "fake");
        assert!(meta.universe.is_none() && meta.coerce_run_universe);
        assert!(meta.flags.optimize && !meta.flags.debug);

        // make was driven with -j4 (machine make-jobs), config→build→utils.
        let log = fs::read_to_string(root.join("make.log")).unwrap();
        assert!(log.contains("-j4 sim-config options="), "{log}");
        assert!(log.contains("-j4 sim\n"), "{log}");
        assert!(log.contains("-j4 sim-utils"), "{log}");
        assert!(!log.contains("realclean"), "fresh build must not realclean: {log}");

        // Thorn toggle applied; flags injected into the rendered optionlist.
        let thornlist = fs::read_to_string(cactus.join("configs/sim/cactup-thornlist.th")).unwrap();
        assert!(thornlist.contains("#DISABLED C/D"), "{thornlist}");
        let rendered = fs::read_to_string(cactus.join("configs/sim/cactup-optionlist.cfg")).unwrap();
        assert!(rendered.starts_with("VERSION = 1\n") && rendered.contains("OPTIMISE = yes"));

        // Combined build output was teed to the per-config log.
        let build_log =
            fs::read_to_string(cactus.join("configs/sim/cactup-build.log")).unwrap();
        assert!(build_log.contains("fake-make: -j4 sim-config"), "{build_log}");
        assert!(build_log.contains("fake-make: -j4 sim\n"), "{build_log}");

        // Second build with nothing changed: up-to-date short-circuit.
        let again = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(!again.rebuilt);
        assert_eq!(again.meta.build_id, outcome.meta.build_id);

        // Changed optionlist ⇒ full rebuild (realclean) + new build-id,
        // stable config-id (§7.4, §7.8).
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n[options]\nVERSION = \"2\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        let machine = mdb.load("fake").unwrap();
        let rebuilt = build(&inst, &machine, "sim", &opts).unwrap();
        assert!(rebuilt.rebuilt);
        assert_ne!(rebuilt.meta.build_id, outcome.meta.build_id);
        assert_eq!(rebuilt.meta.config_id, outcome.meta.config_id);
        let log = fs::read_to_string(root.join("make.log")).unwrap();
        assert!(log.contains("sim-realclean"), "{log}");
    }

    /// A failing `make` must still leave a readable build log, and the error
    /// must point the user at it.
    #[test]
    fn failed_build_writes_log_and_points_at_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let cactus = root.join("inst/Cactus");
        fs::create_dir_all(cactus.join("thornlists")).unwrap();
        fs::write(cactus.join("thornlists/einsteintoolkit.th"), "A/B\n").unwrap();

        // Fake make: emit a diagnostic and fail on the compile step (`sim`),
        // after the config step fabricated the marker.
        let fake_make = root.join("fakemake");
        fs::write(
            &fake_make,
            format!(
                "#!/bin/sh\ncase \"$2\" in\n\
                 sim-config) cd {}/configs/sim/config-data && touch cctk_Config.h ;;\n\
                 sim) echo 'gcc: fatal error: no input files' 1>&2; exit 2 ;;\nesac\n",
                cactus.display()
            ),
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&fake_make, fs::Permissions::from_mode(0o755)).unwrap();
        }

        let machine_dir = root.join("mdb/fake");
        fs::create_dir_all(machine_dir.join("optionlists")).unwrap();
        fs::create_dir_all(machine_dir.join("runscripts")).unwrap();
        fs::create_dir_all(machine_dir.join("submitscripts")).unwrap();
        fs::write(
            machine_dir.join("meta.toml"),
            format!(
                r#"
                [machine]
                nickname = "fake"
                [build]
                make = "{} -j@MAKEJOBS@"
                [queues.local]
                default = true
                [variants.submitscript]
                "default" = ["local"]
                [variants.runscript]
                "default" = ["local"]
                [variants.optionlist]
                variants = ["default"]
                "#,
                fake_make.display()
            ),
        )
        .unwrap();
        fs::write(
            machine_dir.join("optionlists/default.toml"),
            "[cactup]\ngpu = false\ncompatible-queues = [\"local\"]\n[options]\nVERSION = \"1\"\nCC = \"gcc\"\n",
        )
        .unwrap();
        for s in ["runscripts/default.sh", "submitscripts/default.sh"] {
            fs::write(machine_dir.join(s), "#!/bin/sh\n").unwrap();
        }

        let mdb = Mdb::with_roots(root.join("mdb"), PathBuf::from("/nonexistent"));
        let machine = mdb.load("fake").unwrap();
        let inst = Installation::new("et", root.join("inst"));
        let opts = BuildOpts::default_for_tests();

        let err = match build(&inst, &machine, "sim", &opts) {
            Ok(_) => panic!("build should have failed"),
            Err(e) => e,
        };
        let build_log = cactus.join("configs/sim/cactup-build.log");
        // The error names the log path...
        assert!(
            err.to_string().contains(&build_log.display().to_string()),
            "error should point at the log: {err}"
        );
        // ...and the log captured make's stderr diagnostic.
        let captured = fs::read_to_string(&build_log).unwrap();
        assert!(captured.contains("no input files"), "{captured}");
    }
}
