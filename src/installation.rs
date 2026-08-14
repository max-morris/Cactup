//! Per-installation on-disk state (spec §7.4, §8.1, §11.8, D6):
//! `<installation home>/.cactup/installation.toml` (active config,
//! resolved sim-home/test-home), the name→dir registries
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
use colored::Colorize;
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
    pub config: String,
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

/// The pristine as-fetched thornlist, at the installation root (§3.2): the
/// exact list the installation was fetched (or last refetched) from, and the
/// baseline `installation refetch`'s hand-edit guard compares against. Never
/// hand-edited.
pub const SOURCE_THORNLIST: &str = "installation-source.th";

/// The live, editable thornlist, in `Cactus/thornlists/` — what a build reads
/// by default and what a user edits to add or drop a thorn.
pub const LIVE_THORNLIST: &str = "installation-default.th";

/// The name both copies used before the rename, inherited from GetComponents'
/// `COMPONENTLIST_TARGET`. It said nothing about which copy was which and read
/// as "stock Einstein Toolkit" even when it held a custom list, which is why
/// it is gone. Installations that predate the rename are upgraded in place by
/// [`Installation::migrate_thornlist_names`]; every read additionally falls
/// back to this name in case that migration could not run (a read-only tree,
/// say), so an un-migrated installation keeps working.
pub const LEGACY_THORNLIST: &str = "einsteintoolkit.th";

/// `preferred` if it exists, else `legacy` if *that* exists, else `preferred`
/// — so a caller that goes on to report a missing file names the new one.
fn prefer_existing(preferred: PathBuf, legacy: PathBuf) -> PathBuf {
    if preferred.exists() || !legacy.exists() {
        preferred
    } else {
        legacy
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
            .ok_or_else(|| anyhow!("no installation named \"{alias}\" (see `cactup list`)"))?;
        let inst = Installation::new(alias, &entry.path);
        // Upgrade a pre-rename installation on first use, whatever the command
        // is: every path that reads a thornlist then sees one set of names.
        inst.upgrade_thornlist_names();
        Ok(inst)
    }

    /// The Cactus source root of this installation.
    pub fn cactus_root(&self) -> PathBuf {
        self.root.join("Cactus")
    }

    /// The installation's live thornlist ([`LIVE_THORNLIST`]) — the canonical
    /// path, and the one to *write*. The filename is fixed regardless of what
    /// was installed, so for a custom installation this file holds the custom
    /// list's content; `config show` says as much rather than leaving the name
    /// to imply it.
    pub fn live_thornlist(&self) -> PathBuf {
        self.cactus_root().join("thornlists").join(LIVE_THORNLIST)
    }

    /// The live thornlist under its pre-rename name ([`LEGACY_THORNLIST`]).
    pub fn legacy_live_thornlist(&self) -> PathBuf {
        self.cactus_root().join("thornlists").join(LEGACY_THORNLIST)
    }

    /// The live thornlist to *read*: the current name, falling back to the
    /// pre-rename one on an installation the migration has not reached.
    pub fn live_thornlist_to_read(&self) -> PathBuf {
        prefer_existing(self.live_thornlist(), self.legacy_live_thornlist())
    }

    /// The installation's pristine as-fetched thornlist ([`SOURCE_THORNLIST`])
    /// — the canonical path, and the one to *write*.
    pub fn source_thornlist(&self) -> PathBuf {
        self.root.join(SOURCE_THORNLIST)
    }

    /// The pristine copy under its pre-rename name ([`LEGACY_THORNLIST`]).
    pub fn legacy_source_thornlist(&self) -> PathBuf {
        self.root.join(LEGACY_THORNLIST)
    }

    /// The pristine copy to *read*, with the same pre-rename fallback as
    /// [`Installation::live_thornlist_to_read`].
    pub fn source_thornlist_to_read(&self) -> PathBuf {
        prefer_existing(self.source_thornlist(), self.legacy_source_thornlist())
    }

    /// Whether a config's recorded thornlist path is this installation's live
    /// thornlist (as opposed to an explicit `--thornlist` file). Tolerates
    /// both forms `resolve_thornlist` records: a plain `display()` string for
    /// the default, and a canonicalized path when the same file was named via
    /// `--thornlist`. The pre-rename name counts too: a config recorded before
    /// the rename was still built from the live list, and must not start
    /// reading as `--thornlist`.
    pub fn is_live_thornlist(&self, recorded: &str) -> bool {
        let same = |candidate: PathBuf| {
            if recorded == candidate.display().to_string() {
                return true;
            }
            std::fs::canonicalize(&candidate).is_ok_and(|c| recorded == c.display().to_string())
        };
        same(self.live_thornlist()) || same(self.legacy_live_thornlist())
    }

    /// [`Installation::migrate_thornlist_names`], reported and best-effort: a
    /// failure (a read-only tree, say) must not fail the command the user
    /// actually ran, since reads fall back to the pre-rename names on their
    /// own. The notice goes to stderr — it is out-of-band with respect to
    /// whatever that command is printing.
    pub fn upgrade_thornlist_names(&self) {
        match self.migrate_thornlist_names() {
            Ok(true) => eprintln!(
                "{}",
                format!(
                    "Renamed this installation's thornlists: the pristine as-fetched copy is now \
                     {}, and the live, editable one is Cactus/thornlists/{LIVE_THORNLIST}.",
                    self.source_thornlist().display()
                )
                .dimmed()
            ),
            Ok(false) => {}
            Err(e) => eprintln!(
                "{}",
                format!("Warning: could not rename this installation's thornlists: {e:#}").yellow()
            ),
        }
    }

    /// Move an installation created before the thornlist rename onto the
    /// current names: `<root>/einsteintoolkit.th` becomes
    /// `<root>/installation-source.th` and
    /// `Cactus/thornlists/einsteintoolkit.th` becomes
    /// `Cactus/thornlists/installation-default.th`, and configs that recorded
    /// the old live path are retargeted so a rebuild still resolves the file
    /// it was built from (`build::resolve_thornlist` rule 2) instead of
    /// silently dropping to the config's snapshot.
    ///
    /// Idempotent, and cheap enough to run before every command that resolves
    /// an installation: an already-migrated tree costs two `exists` calls and
    /// touches nothing. A copy that already exists under the current name is
    /// never overwritten — reads prefer it, so a leftover file under the old
    /// name is inert. Returns whether anything moved.
    pub fn migrate_thornlist_names(&self) -> Res<bool> {
        let mut moved = false;
        for (legacy, current) in [
            (self.legacy_source_thornlist(), self.source_thornlist()),
            (self.legacy_live_thornlist(), self.live_thornlist()),
        ] {
            if !legacy.exists() || current.exists() {
                continue;
            }
            std::fs::rename(&legacy, &current).with_context(|| {
                format!("Failed to rename {} to {}", legacy.display(), current.display())
            })?;
            moved = true;
        }
        if !moved {
            return Ok(false);
        }
        crate::build::retarget_recorded_thornlist(
            &self.cactus_root(),
            &self.legacy_live_thornlist(),
            &self.live_thornlist(),
        )?;
        Ok(true)
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

    /// The pre-rename layout: both copies named `einsteintoolkit.th`, and a
    /// config recording the live one as the file it was built from.
    fn legacy_layout(inst: &Installation) {
        std::fs::create_dir_all(inst.cactus_root().join("thornlists")).unwrap();
        std::fs::write(inst.legacy_source_thornlist(), "A/B\n").unwrap();
        std::fs::write(inst.legacy_live_thornlist(), "A/B\nC/D\n").unwrap();
        let cfg = inst.cactus_root().join("configs/sim");
        std::fs::create_dir_all(&cfg).unwrap();
        std::fs::write(
            cfg.join("cactup-config.toml"),
            format!(
                "name = \"sim\"\nvariant = \"default\"\nthornlist = \"{}\"\nmachine = \"fake\"\n\
                 config-id = \"c1\"\nbuild-id = \"b1\"\n",
                inst.legacy_live_thornlist().display()
            ),
        )
        .unwrap();
    }

    /// Pre-rename installations must keep working *before* the migration runs:
    /// reads fall back to the old name, and a config recorded against it still
    /// reads as the live list rather than as an explicit `--thornlist`.
    #[test]
    fn pre_rename_thornlists_are_still_found() {
        let (_dir, inst) = inst();
        legacy_layout(&inst);

        assert_eq!(inst.live_thornlist_to_read(), inst.legacy_live_thornlist());
        assert_eq!(inst.source_thornlist_to_read(), inst.legacy_source_thornlist());
        assert!(inst.is_live_thornlist(&inst.legacy_live_thornlist().display().to_string()));

    }

    /// Nothing on disk at all: name the current file, not the old one, so the
    /// error a caller reports teaches the right name.
    #[test]
    fn missing_thornlists_resolve_to_the_current_name() {
        let (_dir, empty) = inst();
        assert_eq!(empty.live_thornlist_to_read(), empty.live_thornlist());
        assert_eq!(empty.source_thornlist_to_read(), empty.source_thornlist());
    }

    #[test]
    fn migration_renames_both_copies_and_retargets_configs() {
        let (_dir, inst) = inst();
        legacy_layout(&inst);

        assert!(inst.migrate_thornlist_names().unwrap());
        assert_eq!(std::fs::read_to_string(inst.source_thornlist()).unwrap(), "A/B\n");
        assert_eq!(std::fs::read_to_string(inst.live_thornlist()).unwrap(), "A/B\nC/D\n");
        assert!(!inst.legacy_source_thornlist().exists());
        assert!(!inst.legacy_live_thornlist().exists());

        // Retargeted, so a rebuild still resolves the live list (and still
        // picks up edits to it) instead of dropping to the config's snapshot.
        let meta = crate::build::ConfigMeta::load(&inst.cactus_root(), "sim").unwrap().unwrap();
        assert_eq!(meta.thornlist, inst.live_thornlist().display().to_string());
        assert!(inst.is_live_thornlist(&meta.thornlist));

        // Idempotent: the second run has nothing to move.
        assert!(!inst.migrate_thornlist_names().unwrap());
    }

    /// A file already under the current name wins; a leftover under the old one
    /// is never allowed to overwrite it.
    #[test]
    fn migration_never_clobbers_a_current_copy() {
        let (_dir, inst) = inst();
        legacy_layout(&inst);
        std::fs::write(inst.live_thornlist(), "live\n").unwrap();

        assert!(inst.migrate_thornlist_names().unwrap());
        assert_eq!(std::fs::read_to_string(inst.live_thornlist()).unwrap(), "live\n");
        assert_eq!(inst.live_thornlist_to_read(), inst.live_thornlist());
        // The pristine copy had no current-name file, so it still moved.
        assert!(inst.source_thornlist().exists());
    }

    /// A config built from an explicit `--thornlist` elsewhere is not ours to
    /// retarget, even when that file happens to carry the old name.
    #[test]
    fn migration_leaves_explicit_thornlists_alone() {
        let (dir, inst) = inst();
        legacy_layout(&inst);
        let elsewhere = dir.path().join("custom").join(LEGACY_THORNLIST);
        std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
        std::fs::write(&elsewhere, "X/Y\n").unwrap();
        let cfg = inst.cactus_root().join("configs/sim/cactup-config.toml");
        let text = std::fs::read_to_string(&cfg)
            .unwrap()
            .replace(&inst.legacy_live_thornlist().display().to_string(), &elsewhere.display().to_string());
        std::fs::write(&cfg, text).unwrap();

        assert!(inst.migrate_thornlist_names().unwrap());
        let meta = crate::build::ConfigMeta::load(&inst.cactus_root(), "sim").unwrap().unwrap();
        assert_eq!(meta.thornlist, elsewhere.display().to_string());
        assert!(!inst.is_live_thornlist(&meta.thornlist));
    }

    #[test]
    fn meta_roundtrip_and_null_config_discipline() {
        let (_dir, inst) = inst();

        // Fresh installation: defaults, null-config state fails fast (§7.1).
        let meta = inst.meta().unwrap();
        assert!(meta.active_config.is_none());
        let err = format!("{:#}", meta.active_config().unwrap_err());
        assert!(err.contains("cactup build"), "guidance expected: {err}");

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
                TestEntry { dir: "/scratch/t".into(), config: "tc".to_owned(), created: Utc::now() },
            );
            locked.set_tests(&tests).unwrap();
        }
        assert_eq!(inst.simulations().unwrap().simulations["bbh"].config, "sim");
        assert_eq!(inst.tests().unwrap().tests["et-tests"].config, "tc");
        // The lock is released with the guard.
        assert!(!inst.cactup_dir().join(".cactup-install.lock").exists());
        drop(inst.locked().unwrap());
    }

    #[test]
    fn is_live_thornlist_tolerates_both_recorded_forms() {
        let (_dir, inst) = inst();
        let live = inst.live_thornlist();

        // The default-rule form: a plain display() string, file need not exist.
        assert!(inst.is_live_thornlist(&live.display().to_string()));
        // Never a match: an explicit custom path, or an empty recorded string
        // (which the old unwrap_or_default() form could spuriously match when
        // canonicalize failed).
        assert!(!inst.is_live_thornlist("/somewhere/else/my-forks.th"));
        assert!(!inst.is_live_thornlist(""));

        // The --thornlist form: the same file, but recorded canonicalized.
        std::fs::create_dir_all(live.parent().unwrap()).unwrap();
        std::fs::write(&live, "!CRL_VERSION = 1.0\n").unwrap();
        let canonical = std::fs::canonicalize(&live).unwrap();
        assert!(inst.is_live_thornlist(&canonical.display().to_string()));
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
