//! Simulation subsystem (spec §8 + §9): the per-simulation directory tree,
//! `.cactup/simulation.toml` metadata, registry lookup, and `sim create`.
//!
//! On-disk contract (§9): cactup never renames/removes anything simfactory
//! placed at or below `<SimName>`; its own bookkeeping is purely additive
//! `.cactup/` directories. A directory is a cactup simulation iff it contains
//! `.cactup/simulation.toml` (D10).

pub mod cache;
pub mod manage;
pub mod restart;
pub mod start;
pub mod vars;

use crate::build::{self, ConfigMeta};
use crate::commands::build as build_cmd;
use crate::commands::Ctx;
use crate::database::SCHEMA;
use crate::installation::{read_toml, write_toml, Installation, SimEntry};
use crate::lock::LinkLock;
use crate::mdb::Machine;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use chrono::Utc;
use colored::Colorize;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

fn default_schema() -> u32 {
    SCHEMA
}

/// Parfile basenames that would collide with the `.cactup/` metadata dir or
/// its entries (§8.2 step 2) — the only naming restriction.
const RESERVED_NAMES: &[&str] = &[".cactup", "exe", "cfg", "par"];

/// `<SimName>/.cactup/simulation.toml` (§9.3): the keys simfactory wrote at
/// create, plus cactup's `alias`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SimulationMeta {
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub machine: String,
    /// `simulation-<name>-<machine>-<hostname>-<user>-<timestamp>-<pid>` (§9.1).
    pub simulation_id: String,
    /// Cactus root at create time.
    pub sourcedir: PathBuf,
    /// The config this simulation was created from.
    pub configuration: String,
    pub config_id: String,
    pub build_id: String,
    /// The frozen binary: `.cactup/exe` (absolute; §8.1).
    pub executable: PathBuf,
    /// Master-copy file name under `.cactup/cfg/`.
    pub optionlist: String,
    /// Master-copy file name under `.cactup/cfg/`: the thorn set this
    /// simulation's frozen binary was built with. Empty for simulations created
    /// before this was recorded, and for configs built by an older cactup.
    #[serde(default)]
    pub thornlist: String,
    /// Master-copy file name under `.cactup/par/` (`<basename>.par` or `.py`).
    pub parfile: String,
    /// Installation alias (needed by the compute-node re-invocation, §8.3).
    pub alias: String,
}

impl Default for SimulationMeta {
    // Only exists so `read_toml` can be reused; a missing simulation.toml is
    // handled before this could ever be observed.
    fn default() -> Self {
        SimulationMeta {
            schema: SCHEMA,
            machine: String::new(),
            simulation_id: String::new(),
            sourcedir: PathBuf::new(),
            configuration: String::new(),
            config_id: String::new(),
            build_id: String::new(),
            executable: PathBuf::new(),
            optionlist: String::new(),
            thornlist: String::new(),
            parfile: String::new(),
            alias: String::new(),
        }
    }
}

/// A located simulation: name, directory, and its simulation-level metadata.
#[derive(Debug)]
pub struct Simulation {
    pub name: String,
    pub dir: PathBuf,
    pub meta: SimulationMeta,
}

impl Simulation {
    pub fn cactup_dir(&self) -> PathBuf {
        self.dir.join(".cactup")
    }

    /// The frozen executable (`@EXECUTABLE@`): `.cactup/exe`, never the live
    /// `<Cactus root>/exe/...` (§8.1).
    pub fn exe(&self) -> PathBuf {
        self.cactup_dir().join("exe")
    }

    /// The parfile master copy (§9.3).
    pub fn master_parfile(&self) -> PathBuf {
        self.cactup_dir().join("par").join(&self.meta.parfile)
    }

    /// The parfile basename with its one `.par`/`.py` extension stripped —
    /// the Cactus working-directory name and ready-to-run parfile stem (§8.2).
    pub fn parfile_stem(&self) -> &str {
        parfile_stem(&self.meta.parfile)
    }

    pub fn is_python_parfile(&self) -> bool {
        self.meta.parfile.ends_with(".py")
    }

    /// `log.txt` at the simulation root, preserved format (§12).
    pub fn log_path(&self) -> PathBuf {
        self.dir.join("log.txt")
    }

    /// Append a `[LOG:<ts>] <a>::<b>` line (§12). Best-effort: logging never
    /// fails a command.
    pub fn log(&self, command: &str, message: &str) {
        let ts = Utc::now().format("%Y-%m-%d %H:%M:%S");
        let line = format!("[LOG:{ts}] {command}::{message}\n");
        let _ = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.log_path())
            .and_then(|mut f| std::io::Write::write_all(&mut f, line.as_bytes()));
    }

    /// Open the simulation at `dir` by reading its metadata (schema-guarded).
    /// This is the D10 detection test: no `.cactup/simulation.toml`, no sim.
    pub fn open(name: &str, dir: &Path) -> Res<Simulation> {
        let path = dir.join(".cactup").join("simulation.toml");
        if !path.is_file() {
            bail!(
                "{} is not a cactup simulation ({} does not exist)",
                dir.display(),
                path.display()
            );
        }
        let meta: SimulationMeta = read_toml(&path)?;
        Ok(Simulation { name: name.to_owned(), dir: dir.to_owned(), meta })
    }

    /// Locate a simulation by name through the per-installation registry
    /// (§8.1) — the reason no path flag exists after create.
    pub fn locate(inst: &Installation, name: &str) -> Res<Simulation> {
        let registry = inst.simulations()?;
        let entry = registry.simulations.get(name).ok_or_else(|| {
            anyhow!(
                "no simulation named \"{name}\" in installation \"{}\" (see `cactup sim list`)",
                inst.alias
            )
        })?;
        if !entry.dir.is_dir() {
            // §8.1
            bail!(
                "simulation \"{name}\" is registered at {} but that directory is missing;\n\
                 run `cactup sim delete {name}` to drop the stale registry entry",
                entry.dir.display()
            );
        }
        Simulation::open(name, &entry.dir)
    }

    /// The per-simulation lock (§2.3 item 3): serializes two `submit`s (or a
    /// submit and a compute-node handoff) racing the same simulation.
    pub fn lock(&self) -> Res<LinkLock> {
        LinkLock::acquire(&self.cactup_dir().join("sim.lock"))
    }

    fn store_meta(&self) -> Res<()> {
        write_toml(&self.cactup_dir().join("simulation.toml"), &self.meta)
    }
}

/// Strip the single trailing `.par`/`.py` extension (§8.2: intra-name dots
/// survive — `q1.5.par` → `q1.5`).
pub fn parfile_stem(basename: &str) -> &str {
    basename
        .strip_suffix(".par")
        .or_else(|| basename.strip_suffix(".py"))
        .unwrap_or(basename)
}

/// §8.2 step 2: the parfile must end in `.par` or `.py`, and its stripped
/// basename must not be a reserved name. Returns (basename, stem).
fn validate_parfile(parfile: &Path) -> Res<(String, String)> {
    let basename = parfile
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow!("parfile path {} has no valid file name", parfile.display()))?
        .to_owned();
    if !basename.ends_with(".par") && !basename.ends_with(".py") {
        // §6.2
        bail!(
            "parfile {} must end in .par (literal @NAME@ substitution) or .py (computed)",
            parfile.display()
        );
    }
    let stem = parfile_stem(&basename).to_owned();
    if stem.is_empty() {
        bail!("parfile {} has an empty basename", parfile.display());
    }
    if RESERVED_NAMES.contains(&stem.as_str()) {
        // §8.2
        bail!(
            "parfile basename \"{stem}\" is reserved (one of: {}) — it would collide with \
             cactup's metadata entries",
            RESERVED_NAMES.join(", ")
        );
    }
    if !parfile.is_file() {
        bail!("parfile {} does not exist", parfile.display());
    }
    Ok((basename, stem))
}

/// `simulation-<name>-<machine>-<hostname>-<user>-<timestamp>-<pid>` (§9.1).
fn simulation_id(name: &str, machine: &str, hostname: &str) -> String {
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned());
    let ts = Utc::now().format("%Y.%m.%d-%H.%M.%S");
    let pid = std::process::id();
    format!("simulation-{name}-{machine}-{hostname}-{user}-{ts}-{pid}")
}

/// What a caller asked [`create`] for, apart from the already-resolved
/// context (ctx / machine / installation). §8.2
pub struct CreateRequest<'a> {
    /// Replace an existing simulation of the same name.
    pub force: bool,
    pub name: &'a str,
    pub parfile: &'a Path,
    /// Config to attach; `None` means the installation's active config.
    pub config: Option<&'a str>,
    /// Simulation directory; `None` means under sim-home. §8.1
    pub sim_dir: Option<&'a Path>,
}

/// `sim create` (§8.2). `machine` must already be resolved; `req.config`
/// defaults to the installation's active config.
pub fn create(
    ctx: &Ctx,
    machine: &Machine,
    inst: &Installation,
    req: &CreateRequest,
) -> Res<Simulation> {
    let CreateRequest { force, name, parfile, config, sim_dir } = *req;
    let inst_meta = inst.meta()?;
    let sim_home = inst_meta.sim_home()?.to_owned();
    let cactus_root = inst.cactus_root();

    // 1. Resolve the config and locate its built executable (fatal if missing).
    let config = match config {
        Some(c) => c.to_owned(),
        None => inst_meta.active_config()?.to_owned(),
    };
    let config_dir = cactus_root.join("configs").join(&config);
    let cfg = match ConfigMeta::load(&cactus_root, &config)? {
        Some(cfg) => cfg,
        None => {
            if let Some(phrase) = build_cmd::in_flight_build(&config_dir, &config, Some(machine)) {
                bail!(
                    "{phrase} — wait for it to finish (`cactup build log {config}` / \
                     `cactup build show {config}`)"
                );
            }
            bail!("config \"{config}\" has never been built (see `cactup config list`)");
        }
    };
    let exe_src = build::executable_path(&cactus_root, &config);
    if !exe_src.is_file() {
        if let Some(phrase) = build_cmd::in_flight_build(&config_dir, &config, Some(machine)) {
            bail!(
                "{phrase} — wait for it to finish (`cactup build log {config}` / \
                 `cactup build show {config}`)"
            );
        }
        bail!(
            "config \"{config}\" has no executable at {} — build it first (`cactup build {config}`)",
            exe_src.display()
        );
    }
    // §7.9's build-submit feature makes this a real race, not a theoretical
    // one: `ensure_cached` below hard-links `exe_src` into the executable
    // cache, and a rebuild's `make` can be replacing those very bytes right
    // now. Unlike everywhere else `in_flight_build` is consulted, this one
    // is a hard error even though a machine is available to disambiguate —
    // freezing a half-written binary into a simulation is real corruption,
    // not just confusing advice, so only an explicit `-f` may proceed.
    if !force && let Some(phrase) = build_cmd::in_flight_build(&config_dir, &config, Some(machine)) {
        bail!(
            "{phrase} — hard-linking its executable while a build may still be replacing it \
             risks freezing a corrupted copy into this simulation; wait for it to finish, or \
             pass -f to proceed anyway"
        );
    }

    // 2. Parfile name validation (collision guard, §8.2).
    let (par_basename, _stem) = validate_parfile(parfile)?;

    // 3. The simulation directory — fixed now, forever (§8.1).
    let dir = match sim_dir {
        Some(p) => std::path::absolute(p)
            .with_context(|| format!("Failed to absolutize --sim-dir {}", p.display()))?,
        None => sim_home.join(&config).join(name),
    };

    // Steps 4–6 under the per-installation lock (§2.3 item 5): the registry
    // check/insert and the skeleton creation must not race another create.
    let locked = inst.locked()?;
    let mut registry = locked.simulations()?;

    let preexisting = registry.simulations.get(name).map(|e| e.dir.clone());
    if let Some(old_dir) = &preexisting {
        if !force {
            bail!(
                "simulation \"{name}\" already exists at {} (use -f to replace it)",
                old_dir.display()
            );
        }
        if old_dir.is_dir() {
            fs::remove_dir_all(old_dir)
                .with_context(|| format!("Failed to remove old simulation at {}", old_dir.display()))?;
        }
        registry.simulations.shift_remove(name);
    }
    if dir.exists() {
        if force {
            fs::remove_dir_all(&dir)
                .with_context(|| format!("Failed to remove old directory {}", dir.display()))?;
        } else {
            bail!("directory {} already exists (use -f to replace it)", dir.display());
        }
    }

    // 4. Skeleton (§9.3): .cactup/{cfg,par}.
    let cactup_dir = dir.join(".cactup");
    fs::create_dir_all(cactup_dir.join("cfg"))
        .and_then(|()| fs::create_dir_all(cactup_dir.join("par")))
        .with_context(|| format!("Failed to create {}", cactup_dir.display()))?;

    // 6. Master copies: the parfile, and the config's optionlist and thornlist
    //    snapshots. Each pair is ordered fed-to-Cactus copy first, source
    //    second; the name recorded in the metadata is the first that exists.
    fs::copy(parfile, cactup_dir.join("par").join(&par_basename))
        .with_context(|| format!("Failed to copy {} into the simulation", parfile.display()))?;
    let copy_cfg = |files: &[&str]| -> Res<String> {
        let mut primary = String::new();
        for cfg_file in files {
            let src = cactus_root.join("configs").join(&config).join(cfg_file);
            if src.is_file() {
                fs::copy(&src, cactup_dir.join("cfg").join(cfg_file))
                    .with_context(|| format!("Failed to copy {}", src.display()))?;
                if primary.is_empty() {
                    primary = (*cfg_file).to_owned();
                }
            }
        }
        Ok(primary)
    };
    let optionlist = copy_cfg(&["cactup-optionlist.cfg", "cactup-optionlist.toml"])?;
    // Which thorns the frozen binary actually contains. The processed list is
    // the authoritative answer — it is what Cactus was handed — and the source
    // snapshot rides along to record what it was derived from. Both may be
    // absent for a config built by an older cactup, hence no hard requirement.
    let thornlist = copy_cfg(&[build::THORNLIST_PROCESSED, build::THORNLIST_SNAPSHOT])?;

    // 5. Freeze the binary: populate CACHE/exe/<build-id> and hard-link it in
    //    (port of CopyFileWithCaching, §8.1), GC-ing orphans opportunistically.
    cache::gc(&sim_home, Some(&cfg.build_id))?;
    let cached = cache::ensure_cached(&sim_home, &cfg.build_id, &exe_src)?;
    cache::link_into(&cached, &cactup_dir.join("exe"))?;

    let hostname = crate::mdb::discover::resolve_hostname(ctx.globals.hostname.as_deref());
    let meta = SimulationMeta {
        schema: SCHEMA,
        machine: machine.name.clone(),
        simulation_id: simulation_id(name, &machine.name, &hostname),
        sourcedir: cactus_root.clone(),
        configuration: config.clone(),
        config_id: cfg.config_id.clone(),
        build_id: cfg.build_id.clone(),
        executable: cactup_dir.join("exe"),
        optionlist,
        thornlist,
        parfile: par_basename,
        alias: inst.alias.clone(),
    };
    let sim = Simulation { name: name.to_owned(), dir: dir.clone(), meta };
    sim.store_meta()?;

    // Register it (§8.1) and release the lock.
    registry.simulations.insert(
        name.to_owned(),
        SimEntry { dir: dir.clone(), config: config.clone(), created: Utc::now() },
    );
    locked.set_simulations(&registry)?;
    drop(locked);

    sim.log("create", &format!("created simulation (config {config}, build-id {})", sim.meta.build_id));
    println!(
        "Created simulation {} at {} (config {})",
        name.bold(),
        dir.display(),
        config.bold()
    );
    Ok(sim)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parfile_stem_strips_one_extension() {
        assert_eq!(parfile_stem("bbh.par"), "bbh");
        assert_eq!(parfile_stem("q1.5.par"), "q1.5");
        assert_eq!(parfile_stem("q1.5.py"), "q1.5");
        assert_eq!(parfile_stem("noext"), "noext");
    }

    #[test]
    fn parfile_validation() {
        let dir = tempfile::tempdir().unwrap();
        let good = dir.path().join("q1.5.par");
        fs::write(&good, "x").unwrap();
        let (base, stem) = validate_parfile(&good).unwrap();
        assert_eq!((base.as_str(), stem.as_str()), ("q1.5.par", "q1.5"));

        // Wrong extension.
        let txt = dir.path().join("a.txt");
        fs::write(&txt, "x").unwrap();
        assert!(validate_parfile(&txt).is_err());

        // Reserved names (§8.2).
        for reserved in ["exe.par", "cfg.py", "par.par", ".cactup.par"] {
            let p = dir.path().join(reserved);
            fs::write(&p, "x").unwrap();
            let err = validate_parfile(&p).unwrap_err().to_string();
            assert!(err.contains("reserved"), "{reserved}: {err}");
        }

        // Missing file.
        assert!(validate_parfile(&dir.path().join("ghost.par")).is_err());
    }

    #[test]
    fn simulation_id_format() {
        let id = simulation_id("bbh", "mel5", "mel5.cct.lsu.edu");
        assert!(id.starts_with("simulation-bbh-mel5-mel5.cct.lsu.edu-"), "{id}");
        assert!(id.split('-').count() >= 7, "{id}");
    }
}
