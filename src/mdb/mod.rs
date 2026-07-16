//! The machine database (MDB) — spec §4 (layout, meta.toml model, two-layer
//! system/user resolution with name-shadowing, `discover.py` discovery,
//! variants/queues/universes and their validation), §7.8 (optionlist TOML +
//! render), §11.2 (test-partition marking & resolution).

pub mod autodetect;
pub mod discover;
pub mod meta;
pub mod optionlist;

// Convenience re-exports for the consuming subsystems.
pub use meta::{Meta, Phase, ScriptKind, Universe, WrappedCommand, HOST_UNIVERSE};
pub use optionlist::Optionlist;

use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use include_dir::{include_dir, Dir};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// The built-in `generic` machine, embedded in the binary so it is always
/// available — even in a release build before the system MDB (`~/.cactup/mdb`)
/// has been deployed. It is the zero-match fallback for unrecognized hosts and
/// the base template for `cactup machine create` (§4.3/§4.6/§4.7), so it must
/// never be able to go missing. Debug builds read the on-disk copy from the
/// repo `mdb/` and never touch this.
static GENERIC_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/mdb/generic");

/// Materialize the embedded `generic` to `~/.cactup/mdb-builtin/<version>/generic`
/// (once per process) and return its path, or `None` if extraction fails. It is
/// written to disk rather than served from memory because the rest of the MDB
/// machinery — running `discover.py`, `copy_dir` in `machine create` — expects a
/// real directory. The path is version-scoped so a binary upgrade re-extracts.
fn builtin_generic_dir() -> Option<PathBuf> {
    static CACHED: OnceLock<Option<PathBuf>> = OnceLock::new();
    CACHED
        .get_or_init(|| match extract_builtin_generic() {
            Ok(dir) => Some(dir),
            Err(e) => {
                eprintln!(
                    "{}",
                    format!("Warning: failed to materialize the built-in generic machine: {e:#}")
                        .yellow()
                );
                None
            }
        })
        .clone()
}

fn extract_builtin_generic() -> Res<PathBuf> {
    let dir = crate::CACTUP_ROOT
        .join("mdb-builtin")
        .join(crate::VERSION)
        .join("generic");
    // Extract only when absent: identical embedded contents, so a leftover copy
    // from an earlier run of this version is already correct.
    if !dir.join("meta.toml").is_file() {
        extract_embedded_dir(&GENERIC_DIR, &dir)?;
    }
    Ok(dir)
}

/// Recursively write an embedded `include_dir` tree to `dest`. Uses only the
/// stable `files`/`dirs`/`contents` API, reconstructing the tree from each
/// entry's file name (paths are relative to the embed root).
fn extract_embedded_dir(dir: &Dir, dest: &Path) -> Res<()> {
    std::fs::create_dir_all(dest)
        .with_context(|| format!("Failed to create {}", dest.display()))?;
    for file in dir.files() {
        let name = file.path().file_name().expect("embedded file has a name");
        let path = dest.join(name);
        std::fs::write(&path, file.contents())
            .with_context(|| format!("Failed to write {}", path.display()))?;
    }
    for sub in dir.dirs() {
        let name = sub.path().file_name().expect("embedded dir has a name");
        extract_embedded_dir(sub, &dest.join(name))?;
    }
    Ok(())
}

/// Which MDB layer a machine resolved from (§4.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layer {
    /// `~/.cactup/mdb` (prod, git-cloned) or the in-repo dev `mdb/`. Read-only.
    System,
    /// `~/.cactup/machines/` — user-created/customized; wins on name clashes.
    User,
}

/// The two-layer machine database (§4.1). No top-level index (D1): machines
/// are enumerated by scanning per-machine directories for a `meta.toml`.
pub struct Mdb {
    pub system_root: PathBuf,
    pub user_root: PathBuf,
}

impl Mdb {
    /// Resolve the MDB roots: `--mdb-path` override → the compile-time-selected
    /// system root (§2.2: dev = `<project root>/mdb`, prod = `~/.cactup/mdb`);
    /// the user overlay is always `~/.cactup/machines`.
    pub fn open(mdb_path_override: Option<&Path>) -> Mdb {
        let system_root = match mdb_path_override {
            Some(path) => path.to_owned(),
            None if cfg!(debug_assertions) => PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            None => crate::CACTUP_ROOT.join("mdb"),
        };
        Mdb { system_root, user_root: crate::CACTUP_ROOT.join("machines") }
    }

    /// Explicit roots, for tests and tools.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn with_roots(system_root: PathBuf, user_root: PathBuf) -> Mdb {
        Mdb { system_root, user_root }
    }

    /// All machine names with their winning layer, sorted by name.
    /// Name-shadowing is applied here: a user-MDB machine hides the
    /// system-MDB machine of the same name entirely (§4.1).
    pub fn machines(&self) -> Res<BTreeMap<String, Layer>> {
        let mut out = BTreeMap::new();
        for (root, layer) in [(&self.system_root, Layer::System), (&self.user_root, Layer::User)] {
            let entries = match std::fs::read_dir(root) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                other => other.with_context(|| format!("Failed to list MDB root {}", root.display()))?,
            };
            for entry in entries {
                let entry = entry?;
                // A machine directory is one holding a meta.toml; anything
                // else (e.g. __pycache__) is not a machine.
                if !entry.path().join("meta.toml").is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                out.insert(name, layer); // User is scanned second and wins
            }
        }
        // The embedded built-in `generic` is always enumerable, even when the
        // system MDB is not on disk. Guarded by `machine_dir` so a failed
        // extraction never lists a machine that cannot then be loaded.
        if !out.contains_key("generic") && self.machine_dir("generic").is_some() {
            out.insert("generic".to_owned(), Layer::System);
        }
        Ok(out)
    }

    /// The winning directory for `name`, if the machine exists in either layer.
    pub fn machine_dir(&self, name: &str) -> Option<(PathBuf, Layer)> {
        for (root, layer) in [(&self.user_root, Layer::User), (&self.system_root, Layer::System)] {
            let dir = root.join(name);
            if dir.join("meta.toml").is_file() {
                return Some((dir, layer));
            }
        }
        // Fall back to the embedded built-in `generic` when it is absent from
        // both real layers (a release build before the system MDB is deployed).
        // Debug builds find it in `system_root` (the repo `mdb/`) above.
        if name == "generic" {
            return builtin_generic_dir().map(|dir| (dir, Layer::System));
        }
        None
    }

    /// Load, validate, autodetect-fill, and path-substitute machine `name`.
    pub fn load(&self, name: &str) -> Res<Machine> {
        let (dir, layer) = self
            .machine_dir(name)
            .ok_or_else(|| anyhow!("no machine named \"{name}\" in the MDB (try `cactup machine list`)"))?;
        Machine::load(name, dir, layer)
    }

    /// Run every machine's `discover.py` against `hostname` and return the
    /// (sorted) names that claim it (§4.3). Shadowing is applied first, so an
    /// overridden machine never double-matches against its system original.
    /// A machine whose discover.py is missing or raises is treated as "did
    /// not match", with a warning under `verbose`.
    pub fn discover(&self, hostname: &str, verbose: bool) -> Res<Vec<String>> {
        let mut matches = Vec::new();
        for (name, _layer) in self.machines()? {
            let (dir, _) = self.machine_dir(&name).expect("machine enumerated but no dir");
            let discover_py = dir.join("discover.py");
            if !discover_py.is_file() {
                if verbose {
                    eprintln!("{}", format!("Warning: machine {name} has no discover.py; skipping it in discovery").yellow());
                }
                continue;
            }
            match discover::is_machine(&discover_py, hostname) {
                Ok(true) => matches.push(name),
                Ok(false) => {}
                Err(e) => {
                    if verbose {
                        eprintln!("{}", format!("Warning: treating machine {name} as non-matching: {e:#}").yellow());
                    }
                }
            }
        }
        Ok(matches)
    }
}

/// A loaded, validated machine.
#[derive(Debug)]
pub struct Machine {
    pub name: String,
    pub dir: PathBuf,
    pub layer: Layer,
    pub meta: Meta,
}

impl Machine {
    fn load(name: &str, dir: PathBuf, layer: Layer) -> Res<Machine> {
        let meta_path = dir.join("meta.toml");
        let text = std::fs::read_to_string(&meta_path)
            .with_context(|| format!("Failed to read {}", meta_path.display()))?;
        let mut meta: Meta = toml::from_str(&text)
            .with_context(|| format!("Failed to parse {}", meta_path.display()))?;
        meta.validate(name)?;

        // §4.6: fill missing hardware from the OS when asked to (or when some
        // queue would otherwise resolve no value — hardware may live at the
        // top level, per-queue, or both, per-queue winning; §4.2). Explicit
        // values always win, and a machine whose queues fully cover a key is
        // left alone even when the top-level key is absent.
        let incomplete = meta.queues.values().any(|q| {
            q.max_cpus_per_node.or(meta.hardware.max_cpus_per_node).is_none() || q.memory.or(meta.hardware.memory).is_none()
        });
        if meta.hardware.autodetect || incomplete {
            let detected = autodetect::detect();
            let hw = &mut meta.hardware;
            hw.max_cpus_per_node = hw.max_cpus_per_node.or(Some(detected.cores));
            hw.memory = hw.memory.or(detected.memory_mb);
        }

        // [paths] values keep their @USER@/@ENV(NAME)@ tokens at load; they
        // are resolved by Meta::resolved_paths at use time, on the machine
        // itself, so entries for machines whose environment this host lacks
        // (e.g. TACC's $SCRATCH) still load and validate everywhere (§4.2).

        let machine = Machine { name: name.to_owned(), dir, layer, meta };
        machine.validate_files()?;
        Ok(machine)
    }

    /// §4.2's layout obligations: every declared variant must have its file.
    fn validate_files(&self) -> Res<()> {
        for kind in [ScriptKind::Submit, ScriptKind::Run] {
            for variant in self.meta.script_variants(kind).variants.keys() {
                self.script_path(kind, variant)?;
            }
        }
        for variant in &self.meta.variants.optionlist.variants {
            let path = self.optionlist_path(variant);
            if !path.is_file() {
                bail!(
                    "machine \"{}\" lists optionlist variant \"{variant}\" but {} does not exist",
                    self.name,
                    path.display()
                );
            }
        }
        Ok(())
    }

    pub fn optionlist_path(&self, variant: &str) -> PathBuf {
        self.dir.join("optionlists").join(format!("{variant}.toml"))
    }

    /// The script file for a variant: `<kind>/<variant>.sh` or `.py` (§4.4).
    /// Exactly one of the two must exist.
    pub fn script_path(&self, kind: ScriptKind, variant: &str) -> Res<ScriptFile> {
        let dir = self.dir.join(kind.dir_name());
        let sh = dir.join(format!("{variant}.sh"));
        let py = dir.join(format!("{variant}.py"));
        match (sh.is_file(), py.is_file()) {
            (true, false) => Ok(ScriptFile { path: sh, python: false }),
            (false, true) => Ok(ScriptFile { path: py, python: true }),
            (true, true) => bail!(
                "machine \"{}\" has both {} and {} — a variant may be .sh or .py, not both",
                self.name,
                sh.display(),
                py.display()
            ),
            (false, false) => bail!(
                "machine \"{}\" declares {} variant \"{variant}\" but neither {} nor {} exists",
                self.name,
                kind.dir_name(),
                sh.display(),
                py.display()
            ),
        }
    }

    /// Select the optionlist variant for a build (§4.4): explicit `--variant`
    /// if given; else the sole listed variant; else the sole `default = true`
    /// variant.
    pub fn select_optionlist(&self, explicit: Option<&str>) -> Res<String> {
        let listed = &self.meta.variants.optionlist.variants;

        if let Some(name) = explicit {
            return listed
                .iter()
                .find(|n| *n == name)
                .cloned()
                .ok_or_else(|| anyhow!("no optionlist variant named \"{name}\" (known: {})", listed.join(", ")));
        }

        if let [single] = listed.as_slice() {
            return Ok(single.clone());
        }
        let mut defaults = Vec::new();
        for variant in listed {
            if optionlist::load_header(&self.optionlist_path(variant))?.default {
                defaults.push(variant);
            }
        }
        match defaults.as_slice() {
            [single] => Ok((*single).clone()),
            [] if listed.is_empty() => bail!("machine \"{}\" has no optionlist variant", self.name),
            [] => bail!(
                "machine \"{}\" has several optionlist variants ({}) and none is marked \
                 default = true; pick one with --variant (§4.4)",
                self.name,
                listed.join(", ")
            ),
            several => bail!(
                "machine \"{}\" marks several optionlist variants default = true ({}); \
                 pick one with --variant (§4.4)",
                self.name,
                several.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ")
            ),
        }
    }
}

/// A resolved script variant file.
#[derive(Debug, Clone)]
pub struct ScriptFile {
    pub path: PathBuf,
    /// `.py` variants emit the script on stdout per the §6.1 convention.
    pub python: bool,
}

fn whoami() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_else(|_| "unknown".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn dev_mdb() -> Mdb {
        let system = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb");
        Mdb::with_roots(system, PathBuf::from("/nonexistent-user-mdb"))
    }

    #[test]
    fn enumerates_dev_machines_skipping_pycache() {
        let machines = dev_mdb().machines().unwrap();
        for name in ["generic", "mel5", "mike"] {
            assert!(machines.contains_key(name), "dev mdb should list {name}");
        }
        assert!(!machines.keys().any(|n| n.contains("pycache")));
        assert!(machines.values().all(|l| *l == Layer::System));
    }

    /// Sweep the whole dev MDB (the simfactory2 port): every machine must
    /// load+validate (meta.toml schema, queue coverage, per-variant files),
    /// every optionlist variant must parse under the §7.8 rules, and every
    /// declared script variant must resolve to exactly one .sh/.py file.
    #[test]
    fn loads_and_validates_every_dev_machine() {
        let mdb = dev_mdb();
        for (name, _layer) in mdb.machines().unwrap() {
            let machine = mdb
                .load(&name)
                .unwrap_or_else(|e| panic!("machine {name} failed to load: {e:#}"));
            for variant in &machine.meta.variants.optionlist.variants {
                optionlist::Optionlist::load(&machine.optionlist_path(variant))
                    .unwrap_or_else(|e| panic!("machine {name} optionlist {variant} failed: {e:#}"));
            }
        }
    }

    #[test]
    fn loads_and_validates_the_real_machines() {
        let mdb = dev_mdb();

        let mel5 = mdb.load("mel5").unwrap();
        assert_eq!(mel5.meta.default_queue(HOST_UNIVERSE), Some("local"));
        // [paths] keep their tokens at load (resolution is a use-time
        // concern — §4.2)…
        assert_eq!(mel5.meta.paths.simulation_home.as_deref(), Some("/home/@USER@/simulations"));
        // …and resolve on demand.
        let user = whoami();
        let paths = mel5.meta.resolved_paths().unwrap();
        assert_eq!(paths.simulation_home.as_deref(), Some(format!("/home/{user}/simulations").as_str()));
        assert_eq!(paths.test_home.as_deref(), Some(format!("/home/{user}/tests").as_str()));
        // Scheduler templates also keep their tokens for use time.
        assert!(mel5.meta.scheduler.submit.as_deref().unwrap().contains("@SCRIPTFILE@"));
        // env-setup lives in [environment] and spans all three phases.
        assert!(mel5.meta.environment.effective(Phase::Build).contains("MPI_DIR"));
        // Optionlist selection (§4.4): with two variants, the default-marked
        // one wins implicitly and the other stays reachable via --variant.
        assert_eq!(mel5.select_optionlist(None).unwrap(), "default");
        assert_eq!(mel5.select_optionlist(Some("debug")).unwrap(), "debug");
        // Script selection honors the test partition.
        let rs = mel5.meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("local", "host", false, None).unwrap().0, "default");
        assert_eq!(rs.select("local", "host", true, None).unwrap().0, "test");
        assert!(!mel5.script_path(ScriptKind::Submit, "test").unwrap().python);

        // db1.hpc.lsu.edu is ONE machine for the whole Deep Bayou cluster: the
        // upstream fragmentation into db1 (native) + db-sing-nv + db-sing-cpu +
        // etworkshop-db is unified here via ONE real queue plus per-variant
        // `build-universes` compatibility lists routing each build flavor's
        // configs to its scripts (§4.4; self-named "db").
        let db1 = mdb.load("db1.hpc.lsu.edu").unwrap();
        assert_eq!(db1.meta.machine.name.as_deref(), Some("db"));
        assert_eq!(db1.meta.default_queue(HOST_UNIVERSE), Some("gpu"));
        // The cluster's single real partition, GPU-flagged.
        assert_eq!(db1.meta.queues.len(), 1);
        assert!(db1.meta.queues["gpu"].gpu);
        assert_eq!(db1.meta.scheduler_queue_name("gpu").unwrap(), "gpu");
        // Availability vs. request (§8.5): a Deep Bayou node HAS 48 CPUs/cores,
        // and a no-`-c` job REQUESTS 24 CPUs/task — so the fill-the-node default
        // lands on floor(48/24) = 2 tasks/node, one per GPU.
        let hw = db1.meta.effective_hardware("gpu").unwrap();
        assert_eq!(hw.max_cpus_per_node, Some(48));
        assert_eq!(hw.default_cpus_per_task, Some(24));
        // Three optionlists, none default-marked: a build must pick one with
        // --variant (§4.4).
        assert!(db1.select_optionlist(None).is_err());
        for v in ["native", "sing-nv", "sing-cpu"] {
            assert_eq!(db1.select_optionlist(Some(v)).unwrap(), v);
        }
        // The native flavor builds in the DECLARED host universe — an identity
        // universe (§4.8) carrying only a module-loading env-build-setup
        // override (§6.1) — reached via the declared-host fallback, so
        // [build].universe is unset; the Singularity flavors name their own
        // build universe in the optionlist header, --nv vs not.
        assert!(db1.meta.build.universe.is_none());
        let host = db1.meta.universe("host").unwrap();
        assert!(host.wrapper.is_none() && host.wrapper_argv.is_none());
        assert!(host.environment.env_build_setup.as_deref().unwrap().contains("module load gcc/9.3.0"));
        assert_eq!(optionlist::load_header(&db1.optionlist_path("sing-nv")).unwrap().universe.as_deref(), Some("et-sing"));
        assert_eq!(optionlist::load_header(&db1.optionlist_path("sing-cpu")).unwrap().universe.as_deref(), Some("et-sing-cpu"));
        for v in ["sing-nv", "sing-cpu"] {
            assert_eq!(optionlist::load_header(&db1.optionlist_path(v)).unwrap().compatible_queues, ["gpu"]);
        }
        let argv = |u: &str| db1.meta.universe(u).unwrap().wrapper_argv.clone().unwrap();
        assert!(argv("et-sing").iter().any(|a| a == "--nv"));
        assert!(!argv("et-sing-cpu").iter().any(|a| a == "--nv"));
        // Native builds run with the "default" scripts; the Singularity
        // flavors share the "sing" scripts — same queue, routed by the
        // config's build universe (§4.4).
        let rs = db1.meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("gpu", "host", false, None).unwrap().0, "default");
        assert_eq!(rs.select("gpu", "et-sing", false, None).unwrap().0, "sing");
        assert_eq!(rs.select("gpu", "et-sing-cpu", true, None).unwrap().0, "sing-test");

        // qbd flipped SLURM->PBS with an empty upstream queue: its placeholder
        // queue carries name = "" so @QUEUE@ resolves to the empty string (the
        // remaining real-machine user of the per-queue `name` override).
        let qbd = mdb.load("qbd").unwrap();
        assert_eq!(qbd.meta.scheduler_queue_name("default").unwrap(), "");
        assert!(qbd.meta.scheduler.submit.as_deref().unwrap().starts_with("qsub"));

        // graham unifies one Compute Canada cluster's CPU (g++) and CUDA (nvcc)
        // build flavors into two optionlist variants. The CUDA variant carries
        // per-variant disabled-thorns (§7.8) — thorns nvcc can't compile — that
        // the CPU variant keeps; this is what lets the two flavors coexist on
        // one machine.
        let graham = mdb.load("graham").unwrap();
        assert!(!graham.meta.queues["cpu"].gpu && graham.meta.queues["gpu"].gpu);
        let gpu_ol = optionlist::load_header(&graham.optionlist_path("gpu")).unwrap();
        assert!(gpu_ol.disabled_thorns.iter().any(|t| t == "ExternalLibraries/LORENE"));
        assert!(optionlist::load_header(&graham.optionlist_path("default")).unwrap().disabled_thorns.is_empty());

        // frontera's [paths] read TACC's hashed storage roots via @ENV()@
        // (§4.2): the entry loads anywhere; resolution needs the machine's
        // own environment and hard-errors without it.
        let frontera = mdb.load("frontera").unwrap();
        assert_eq!(
            frontera.meta.paths.simulation_home.as_deref(),
            Some("@ENV(SCRATCH)@/simulations")
        );
        if std::env::var("SCRATCH").is_err() {
            let err = format!("{:#}", frontera.meta.resolved_paths().unwrap_err());
            assert!(err.contains("SCRATCH"), "error names the env var: {err}");
        }

        let generic = mdb.load("generic").unwrap();
        // §4.6: autodetect filled the missing hardware.
        assert!(generic.meta.hardware.max_cpus_per_node.unwrap() >= 1);
        assert!(generic.meta.hardware.memory.unwrap() > 0);
        // generic has no paths: consumers use the ~/.cactup fallbacks.
        assert!(generic.meta.paths.simulation_home.is_none());
        assert_eq!(generic.select_optionlist(None).unwrap(), "default"); // sole variant
    }

    #[test]
    fn user_layer_shadows_system_layer() {
        let system = tempfile::tempdir().unwrap();
        let user = tempfile::tempdir().unwrap();
        let minimal = r#"
            [machine]
            nickname = "m"
            [queues.local]
            default = true
            [variants.submitscript]
            "default" = ["local"]
            [variants.runscript]
            "default" = ["local"]
            [variants.optionlist]
            variants = ["default"]
        "#;
        for (root, tag) in [(system.path(), "system"), (user.path(), "user")] {
            let dir = root.join("box");
            fs::create_dir_all(dir.join("optionlists")).unwrap();
            fs::create_dir_all(dir.join("runscripts")).unwrap();
            fs::create_dir_all(dir.join("submitscripts")).unwrap();
            fs::write(dir.join("meta.toml"), minimal.replace("\"m\"", &format!("\"{tag}\""))).unwrap();
            fs::write(dir.join("optionlists/default.toml"), "[options]\nVERSION = \"1\"\n").unwrap();
            fs::write(dir.join("runscripts/default.sh"), "#!/bin/sh\n").unwrap();
            fs::write(dir.join("submitscripts/default.sh"), "#!/bin/sh\n").unwrap();
        }
        // Only-system machine to prove both layers are scanned.
        let solo = system.path().join("solo");
        fs::create_dir_all(&solo).unwrap();
        fs::write(solo.join("meta.toml"), minimal).unwrap();

        let mdb = Mdb::with_roots(system.path().to_owned(), user.path().to_owned());
        let machines = mdb.machines().unwrap();
        // `generic` is always present via the embedded built-in fallback, even
        // though neither temp root defines it.
        assert_eq!(machines.keys().cloned().collect::<Vec<_>>(), ["box", "generic", "solo"]);
        assert_eq!(machines["box"], Layer::User);
        assert_eq!(machines["generic"], Layer::System);
        assert_eq!(machines["solo"], Layer::System);

        let loaded = mdb.load("box").unwrap();
        assert_eq!(loaded.layer, Layer::User);
        assert_eq!(loaded.meta.machine.nickname.as_deref(), Some("user"));
    }

    #[test]
    fn load_reports_missing_variant_files() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("broken");
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            dir.join("meta.toml"),
            r#"
                [machine]
                nickname = "broken"
                [queues.local]
                default = true
                [variants.submitscript]
                "default" = ["local"]
                [variants.runscript]
                "default" = ["local"]
                [variants.optionlist]
                variants = ["default"]
            "#,
        )
        .unwrap();
        let mdb = Mdb::with_roots(root.path().to_owned(), PathBuf::from("/nonexistent"));
        let err = format!("{:#}", mdb.load("broken").unwrap_err());
        assert!(err.contains("default"), "unexpected error: {err}");
    }

    #[test]
    fn discovery_sweep_matches_mel5_only() {
        if std::process::Command::new("python3").arg("--version").output().is_err() {
            eprintln!("skipping: python3 not on PATH");
            return;
        }
        let mdb = dev_mdb();
        assert_eq!(mdb.discover("melete05.cct.lsu.edu", false).unwrap(), ["mel5"]);
        assert!(mdb.discover("someone-elses-laptop", false).unwrap().is_empty());
    }
}
