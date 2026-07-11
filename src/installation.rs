//! Per-installation on-disk state (spec §7.4, §8.1, §11.8, D6):
//! `<installation home>/.cactup/installation.toml` (active config,
//! active test-config, resolved sim-home/test-home), the name→dir registries
//! `simulations.toml` and `tests.toml`, and the per-installation link-lock
//! (§2.3 item 5) guarding all of their mutations.
//!
//! Reads are lock-free (writes are atomic temp+rename); every mutation goes
//! through [`Installation::locked`] so two commands in one installation can
//! never race the registries or the active pointers. Different installations
//! never contend.

// Consumed by the Phase-3 CFG/SIM/TEST streams; unused until then.

use crate::commands::Ctx;
use crate::database::SCHEMA;
use crate::lock::LinkLock;
use crate::mdb::Machine;
use crate::Res;
use anyhow::{anyhow, bail, Context};
use chrono::{DateTime, Utc};
use indexmap::IndexMap;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

fn default_schema() -> u32 {
    SCHEMA
}

/// `installation.toml` (§8.1, §11.8).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct InstallationMeta {
    #[serde(default = "default_schema")]
    pub schema: u32,
    /// The active config; absent = the §7.1 null-config state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_config: Option<String>,
    /// The active test-config, independent of `active-config` (§11.8).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_test_config: Option<String>,
    /// Resolved at install time (§8.1); `sim` commands never re-derive it.
    #[serde(default)]
    pub sim_home: Option<PathBuf>,
    /// Resolved at install time like sim-home (§11.5, §11.8).
    #[serde(default)]
    pub test_home: Option<PathBuf>,
}

impl Default for InstallationMeta {
    fn default() -> Self {
        InstallationMeta {
            schema: SCHEMA,
            active_config: None,
            active_test_config: None,
            sim_home: None,
            test_home: None,
        }
    }
}

impl InstallationMeta {
    /// The active config, failing fast in the null-config state (§7.1).
    pub fn active_config(&self) -> Res<&str> {
        self.active_config.as_deref().ok_or_else(|| {
            anyhow!(
                "this installation has no active config (null-config state); \
                 build one with `cactup build <name>` or select one with `cactup config use <name>`"
            )
        })
    }

    /// The active test-config, failing fast when none exists (§11.3).
    pub fn active_test_config(&self) -> Res<&str> {
        self.active_test_config.as_deref().ok_or_else(|| {
            anyhow!(
                "this installation has no active test-config; \
                 build one with `cactup test build` or select one with `cactup test use <name>`"
            )
        })
    }

    pub fn sim_home(&self) -> Res<&Path> {
        self.sim_home
            .as_deref()
            .ok_or_else(|| anyhow!("installation.toml records no sim-home; `cactup use <alias>` backfills it"))
    }

    pub fn test_home(&self) -> Res<&Path> {
        self.test_home
            .as_deref()
            .ok_or_else(|| anyhow!("installation.toml records no test-home; `cactup use <alias>` backfills it"))
    }
}

/// One `simulations.toml` entry (§8.1): the location index, NOT per-sim state
/// (that lives in the sim's own `.cactup/`, D4).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SimEntry {
    pub dir: PathBuf,
    pub config: String,
    pub created: DateTime<Utc>,
}

/// One `tests.toml` entry (§11.8).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TestEntry {
    pub dir: PathBuf,
    pub test_config: String,
    pub created: DateTime<Utc>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct SimRegistry {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub simulations: IndexMap<String, SimEntry>,
}

impl Default for SimRegistry {
    fn default() -> Self {
        SimRegistry { schema: SCHEMA, simulations: IndexMap::new() }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct TestRegistry {
    #[serde(default = "default_schema")]
    pub schema: u32,
    #[serde(default)]
    pub tests: IndexMap<String, TestEntry>,
}

impl Default for TestRegistry {
    fn default() -> Self {
        TestRegistry { schema: SCHEMA, tests: IndexMap::new() }
    }
}

/// One Einstein Toolkit installation on disk.
pub struct Installation {
    pub alias: String,
    /// The installation home (contains `Cactus/` and `.cactup/`).
    pub root: PathBuf,
}

impl Installation {
    pub fn new(alias: impl Into<String>, root: impl Into<PathBuf>) -> Installation {
        Installation { alias: alias.into(), root: root.into() }
    }

    /// The installation a command targets: `--installation <alias>` or the
    /// active installation; fail fast otherwise (§3).
    pub fn resolve(ctx: &Ctx) -> Res<Installation> {
        let db = ctx.db.read()?;
        let alias = match &ctx.globals.installation {
            Some(alias) => alias.clone(),
            None => db.active_installation.clone().ok_or_else(|| {
                anyhow!(
                    "no active installation; run `cactup install`, or `cactup use <alias>` \
                     to activate an existing one"
                )
            })?,
        };
        let entry = db
            .installations
            .get(&alias)
            .ok_or_else(|| anyhow!("no installation named \"{alias}\" (see `cactup show`)"))?;
        Ok(Installation::new(alias, &entry.path))
    }

    /// The Cactus source root of this installation.
    pub fn cactus_root(&self) -> PathBuf {
        self.root.join("Cactus")
    }

    pub fn cactup_dir(&self) -> PathBuf {
        self.root.join(".cactup")
    }

    fn meta_path(&self) -> PathBuf {
        self.cactup_dir().join("installation.toml")
    }

    fn simulations_path(&self) -> PathBuf {
        self.cactup_dir().join("simulations.toml")
    }

    fn tests_path(&self) -> PathBuf {
        self.cactup_dir().join("tests.toml")
    }

    /// Lock-free read (writes are atomic). Missing file = defaults.
    pub fn meta(&self) -> Res<InstallationMeta> {
        read_toml(&self.meta_path())
    }

    pub fn simulations(&self) -> Res<SimRegistry> {
        read_toml(&self.simulations_path())
    }

    pub fn tests(&self) -> Res<TestRegistry> {
        read_toml(&self.tests_path())
    }

    /// Acquire the per-installation lock (§2.3 item 5) for a mutation. Keep
    /// the guard for the whole compound operation; do NOT nest `locked()`
    /// calls (the lock is not reentrant).
    pub fn locked(&self) -> Res<LockedInstallation<'_>> {
        std::fs::create_dir_all(self.cactup_dir())
            .with_context(|| format!("Failed to create {}", self.cactup_dir().display()))?;
        let lock = LinkLock::acquire(&self.cactup_dir().join(".cactup-install.lock"))?;
        Ok(LockedInstallation { inst: self, _lock: lock })
    }

    /// First-time setup (install-time, §8.1) and `cactup use` backfill:
    /// resolve sim-home/test-home from the machine's `[paths]` (resolved
    /// here at use time — @USER@/@ENV()@, §4.2; an unset env var is a hard
    /// error) and write installation.toml. A home that is already recorded
    /// is never changed (homes are fixed at install time); only missing ones
    /// are filled.
    pub fn ensure_meta(&self, machine: &Machine) -> Res<InstallationMeta> {
        let locked = self.locked()?;
        let mut meta = locked.meta()?;
        if meta.sim_home.is_some() && meta.test_home.is_some() {
            return Ok(meta);
        }
        let paths = machine.meta.resolved_paths()?;
        if meta.sim_home.is_none() {
            meta.sim_home = Some(resolve_home(
                paths.simulation_home.as_deref(),
                "simulations",
                &self.alias,
            ));
        }
        if meta.test_home.is_none() {
            meta.test_home = Some(resolve_home(
                paths.test_home.as_deref(),
                "tests",
                &self.alias,
            ));
        }
        locked.set_meta(&meta)?;
        Ok(meta)
    }
}

/// `<machine home>/<alias>`, or the `~/.cactup/<fallback>/<alias>` fallback
/// when the machine omits the key (§8.1, §4.2).
fn resolve_home(machine_home: Option<&str>, fallback: &str, alias: &str) -> PathBuf {
    match machine_home {
        Some(home) => PathBuf::from(home).join(alias),
        None => crate::CACTUP_ROOT.join(fallback).join(alias),
    }
}

/// Mutation window for one installation: all writes to installation.toml and
/// the registries happen through this guard.
pub struct LockedInstallation<'i> {
    inst: &'i Installation,
    _lock: LinkLock,
}

impl LockedInstallation<'_> {
    pub fn meta(&self) -> Res<InstallationMeta> {
        self.inst.meta()
    }

    pub fn set_meta(&self, meta: &InstallationMeta) -> Res<()> {
        write_toml(&self.inst.meta_path(), meta)
    }

    pub fn simulations(&self) -> Res<SimRegistry> {
        self.inst.simulations()
    }

    pub fn set_simulations(&self, registry: &SimRegistry) -> Res<()> {
        write_toml(&self.inst.simulations_path(), registry)
    }

    pub fn tests(&self) -> Res<TestRegistry> {
        self.inst.tests()
    }

    pub fn set_tests(&self, registry: &TestRegistry) -> Res<()> {
        write_toml(&self.inst.tests_path(), registry)
    }
}

/// Read a schema-guarded cactup TOML, defaulting when the file is absent.
/// (Shared with the SIM/TEST metadata files — same §9.3 schema policy.)
pub(crate) fn read_toml<T: DeserializeOwned + Default>(path: &Path) -> Res<T> {
    #[derive(Deserialize)]
    struct SchemaOnly {
        #[serde(default = "default_schema")]
        schema: u32,
    }

    let text = match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(T::default()),
        other => other.with_context(|| format!("Failed to read {}", path.display()))?,
    };
    let probe: SchemaOnly =
        toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))?;
    if probe.schema > SCHEMA {
        bail!(
            "{} has schema {} but this cactup understands at most schema {SCHEMA}; please upgrade cactup",
            path.display(),
            probe.schema
        );
    }
    toml::from_str(&text).with_context(|| format!("Failed to parse {}", path.display()))
}

/// Atomic (temp + rename) TOML write, so readers never see a torn file.
pub(crate) fn write_toml<T: Serialize>(path: &Path, value: &T) -> Res<()> {
    let dir = path.parent().expect("cactup TOML paths have parents");
    std::fs::create_dir_all(dir).with_context(|| format!("Failed to create {}", dir.display()))?;
    let text = toml::to_string_pretty(value).context("Failed to serialize TOML")?;
    let mut temp = tempfile::NamedTempFile::new_in(dir)
        .with_context(|| format!("Failed to create temp file in {}", dir.display()))?;
    temp.write_all(text.as_bytes())
        .and_then(|()| temp.as_file().sync_all())
        .with_context(|| format!("Failed to write {}", path.display()))?;
    temp.persist(path)
        .with_context(|| format!("Failed to move {} into place", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inst() -> (tempfile::TempDir, Installation) {
        let dir = tempfile::tempdir().unwrap();
        let inst = Installation::new("et-dev", dir.path());
        (dir, inst)
    }

    #[test]
    fn meta_roundtrip_and_null_config_discipline() {
        let (_dir, inst) = inst();

        // Fresh installation: defaults, null-config state fails fast (§7.1).
        let meta = inst.meta().unwrap();
        assert!(meta.active_config.is_none());
        let err = format!("{:#}", meta.active_config().unwrap_err());
        assert!(err.contains("cactup build"), "guidance expected: {err}");
        assert!(meta.active_test_config().is_err());

        {
            let locked = inst.locked().unwrap();
            let mut meta = locked.meta().unwrap();
            meta.active_config = Some("sim".to_owned());
            meta.sim_home = Some("/work/et-dev".into());
            locked.set_meta(&meta).unwrap();
        }
        let meta = inst.meta().unwrap();
        assert_eq!(meta.active_config().unwrap(), "sim");
        assert_eq!(meta.sim_home().unwrap(), Path::new("/work/et-dev"));
        assert_eq!(meta.schema, SCHEMA);
        // Test-config pointer is independent of the config pointer (§11.8).
        assert!(meta.active_test_config().is_err());
    }

    #[test]
    fn registries_roundtrip_under_the_lock() {
        let (_dir, inst) = inst();
        {
            let locked = inst.locked().unwrap();
            let mut sims = locked.simulations().unwrap();
            sims.simulations.insert(
                "bbh".to_owned(),
                SimEntry { dir: "/scratch/bbh".into(), config: "sim".to_owned(), created: Utc::now() },
            );
            locked.set_simulations(&sims).unwrap();

            let mut tests = locked.tests().unwrap();
            tests.tests.insert(
                "et-tests".to_owned(),
                TestEntry { dir: "/scratch/t".into(), test_config: "tc".to_owned(), created: Utc::now() },
            );
            locked.set_tests(&tests).unwrap();
        }
        assert_eq!(inst.simulations().unwrap().simulations["bbh"].config, "sim");
        assert_eq!(inst.tests().unwrap().tests["et-tests"].test_config, "tc");
        // The lock is released with the guard.
        assert!(!inst.cactup_dir().join(".cactup-install.lock").exists());
        drop(inst.locked().unwrap());
    }

    #[test]
    fn schema_guard_refuses_newer_files() {
        let (_dir, inst) = inst();
        std::fs::create_dir_all(inst.cactup_dir()).unwrap();
        std::fs::write(inst.meta_path(), format!("schema = {}\n", SCHEMA + 1)).unwrap();
        let err = format!("{:#}", inst.meta().unwrap_err());
        assert!(err.contains("upgrade"), "{err}");
    }

    #[test]
    fn ensure_meta_resolves_homes_once() {
        let (_dir, inst) = inst();
        let mdb = crate::mdb::Mdb::with_roots(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("mdb"),
            PathBuf::from("/nonexistent"),
        );
        let mel5 = mdb.load("mel5").unwrap();

        let meta = inst.ensure_meta(&mel5).unwrap();
        // mel5 sets simulation-home/test-home; per-alias subdirs (§8.1, §11.5).
        assert!(meta.sim_home().unwrap().ends_with("simulations/et-dev"));
        assert!(meta.test_home().unwrap().ends_with("tests/et-dev"));

        // Fixed at install time: a second ensure with a different machine
        // changes nothing.
        let generic = mdb.load("generic").unwrap();
        let again = inst.ensure_meta(&generic).unwrap();
        assert_eq!(again.sim_home().unwrap(), meta.sim_home().unwrap());
    }
}
