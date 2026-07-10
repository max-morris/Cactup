//! The machine database (MDB) — spec §4 (layout, meta.toml model, two-layer
//! system/user resolution with name-shadowing, `discover.py` discovery,
//! variants/queues/universes and their validation), §7.8 (optionlist TOML +
//! render), §11.2 (test-partition marking & resolution).

pub mod autodetect;
pub mod discover;
pub mod meta;
pub mod optionlist;

// Convenience re-exports for the consuming subsystems.
pub use meta::{Meta, Phase, ScriptKind, Universe, WrappedCommand};
pub use optionlist::Optionlist;

use crate::template::VarSet;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use colored::Colorize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

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
        None
    }

    /// Load, validate, autodetect-fill, and path-substitute machine `name`.
    pub fn load(&self, name: &str) -> Res<Machine> {
        let (dir, layer) = self
            .machine_dir(name)
            .ok_or_else(|| anyhow!("no machine named \"{name}\" in the MDB (try `cactup machine show`)"))?;
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

        // §4.6: fill missing hardware from the OS when asked to (or when the
        // core keys are simply absent). Explicit values always win.
        let hw = &mut meta.hardware;
        if hw.autodetect || hw.ppn.is_none() || hw.num_threads.is_none() || hw.memory.is_none() {
            let detected = autodetect::detect();
            hw.ppn = hw.ppn.or(Some(detected.cores));
            hw.num_threads = hw.num_threads.or(Some(detected.cores));
            hw.memory = hw.memory.or(detected.memory_mb);
        }

        // MDB load substitutes only the [paths] (@USER@) values; scheduler/
        // script templates keep their tokens for use-time substitution
        // (cross-stream contract; §4.2).
        let mut vars = VarSet::new();
        vars.set("USER", whoami());
        meta.substitute_paths(&vars)
            .with_context(|| format!("in {}", meta_path.display()))?;

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

    /// Select the optionlist variant for a build (§4.4, §11.2): explicit
    /// `--variant` if given; otherwise the partition (test set for
    /// `test build`, falling back to normal when empty; normal-only for
    /// `config build`) must contain exactly one candidate.
    pub fn select_optionlist(&self, explicit: Option<&str>, test_build: bool) -> Res<String> {
        let listed = &self.meta.variants.optionlist.variants;
        let mut flagged = Vec::new(); // (name, is_test)
        for variant in listed {
            let header = optionlist::load_header(&self.optionlist_path(variant))?;
            flagged.push((variant.clone(), header.test));
        }
        let test_set_nonempty = flagged.iter().any(|(_, t)| *t);

        if let Some(name) = explicit {
            let (name, is_test) = flagged
                .iter()
                .find(|(n, _)| n == name)
                .ok_or_else(|| anyhow!("no optionlist variant named \"{name}\" (known: {})", listed.join(", ")))?;
            if !test_build && *is_test {
                bail!("optionlist variant \"{name}\" is test-marked and never used for a normal build (§11.2)");
            }
            if test_build && test_set_nonempty && !*is_test {
                bail!("optionlist variant \"{name}\" is not test-marked, but this machine has test optionlists (§11.2)");
            }
            return Ok(name.clone());
        }

        let use_test = test_build && test_set_nonempty;
        let candidates: Vec<&String> = flagged.iter().filter(|(_, t)| *t == use_test).map(|(n, _)| n).collect();
        match candidates.as_slice() {
            [single] => Ok((*single).clone()),
            [] => bail!(
                "machine \"{}\" has no {}optionlist variant",
                self.name,
                if use_test { "test " } else { "non-test " }
            ),
            several => bail!(
                "machine \"{}\" has several {}optionlist variants ({}); pick one with --variant (§4.4)",
                self.name,
                if use_test { "test " } else { "" },
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
        assert_eq!(machines.keys().cloned().collect::<Vec<_>>(), ["generic", "mel5"]);
        assert!(machines.values().all(|l| *l == Layer::System));
    }

    #[test]
    fn loads_and_validates_the_real_machines() {
        let mdb = dev_mdb();

        let mel5 = mdb.load("mel5").unwrap();
        assert_eq!(mel5.meta.default_queue(), Some("local"));
        // [paths] @USER@ was substituted at load…
        let user = whoami();
        assert_eq!(mel5.meta.paths.simulation_home.as_deref(), Some(format!("/home/{user}/simulations").as_str()));
        assert_eq!(mel5.meta.paths.test_home.as_deref(), Some(format!("/home/{user}/tests").as_str()));
        // …but scheduler templates keep their tokens for use time.
        assert!(mel5.meta.scheduler.submit.as_deref().unwrap().contains("@SCRIPTFILE@"));
        // env-setup lives in [environment] and spans all three phases.
        assert!(mel5.meta.environment.effective(Phase::Build).contains("MPI_DIR"));
        // Optionlist partition: default for builds, test for test builds (§11.2).
        assert_eq!(mel5.select_optionlist(None, false).unwrap(), "default");
        assert_eq!(mel5.select_optionlist(None, true).unwrap(), "test");
        assert!(mel5.select_optionlist(Some("test"), false).is_err());
        // Script selection honors the test partition.
        let rs = mel5.meta.script_variants(ScriptKind::Run);
        assert_eq!(rs.select("local", false, None).unwrap().0, "default");
        assert_eq!(rs.select("local", true, None).unwrap().0, "test");
        assert!(!mel5.script_path(ScriptKind::Submit, "test").unwrap().python);

        let generic = mdb.load("generic").unwrap();
        // §4.6: autodetect filled the missing hardware.
        assert!(generic.meta.hardware.ppn.unwrap() >= 1);
        assert!(generic.meta.hardware.num_threads.unwrap() >= 1);
        assert!(generic.meta.hardware.memory.unwrap() > 0);
        // generic has no paths: consumers use the ~/.cactup fallbacks.
        assert!(generic.meta.paths.simulation_home.is_none());
        assert_eq!(generic.select_optionlist(None, true).unwrap(), "default"); // borrow normal set
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
        assert_eq!(machines.keys().cloned().collect::<Vec<_>>(), ["box", "solo"]);
        assert_eq!(machines["box"], Layer::User);
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
