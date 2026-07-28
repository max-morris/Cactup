//! The global database, `~/.cactup/database.json` (spec §2.1) — the ONLY
//! global mutable state: installations, the active installation, knobs (§5),
//! and the detected-machine cache (§4.3). Config metadata and simulation
//! state live on disk next to what they describe (D4, D6), never here.
//!
//! Locking follows §2.3 (D11): every mutation is a self-contained
//! lock → re-read → mutate → persist → unlock via [`Db::update`], so the lock
//! is never held across long-running work and a long command can never
//! clobber an unrelated change made while it worked. Reads take a snapshot
//! under the lock and release immediately. There is no whole-lifetime lock,
//! and consequently no Drop/signal persistence: in-memory state never
//! outlives the lock, so there is nothing to flush at exit.

use anyhow::{bail, Context};
use indexmap::IndexMap;
use serde_derive::{Deserialize, Serialize};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::lock::LinkLock;
use crate::Res;

/// On-disk schema version (§2.1), carried by the DB and every cactup TOML.
/// Bumped ONLY on a breaking on-disk change; a newer binary must read every
/// older schema it ever shipped, and refuses only a *newer* one.
pub const SCHEMA: u32 = 1;

/// Files written before the schema field existed are schema 1.
fn default_schema() -> u32 {
    SCHEMA
}

/// Knob names cactup recognizes (§5). `user`/`email` are normally derived
/// (`$USER`, `git config user.email`) but may be overridden as knobs.
pub const KNOWN_KNOBS: &[&str] = &["allocation", "mail", "mail-type", "queue", "user", "email"];

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct CactusInstallation {
    pub alias: String,
    pub release: Option<String>,
    pub path: String,
    /// For a **custom installation** (`install --thornlist`, where `release` is
    /// `None`): the thornlist file it was installed from. Without it "(custom
    /// installation)" is all we can say about where the tree came from. `None`
    /// for release installs, and for custom ones registered before this was
    /// recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thornlist: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct Database {
    #[serde(default = "default_schema")]
    pub schema: u32,
    pub cactup_version: String,
    /// alias → installation.
    #[serde(default)]
    pub installations: IndexMap<String, CactusInstallation>,
    /// Default installation to use for most commands.
    #[serde(default)]
    pub active_installation: Option<String>,
    /// Global default values (§5). A `~/.cactup` lives on exactly one
    /// machine, so knobs are a single flat map, not keyed by machine.
    #[serde(default)]
    pub knobs: IndexMap<String, String>,
    /// The resolved machine for *this* `~/.cactup` — a single string, not
    /// keyed by hostname (§4.3).
    #[serde(default)]
    pub detected_machine: Option<String>,
}

impl Database {
    pub fn new() -> Self {
        Self {
            schema: SCHEMA,
            cactup_version: crate::VERSION.to_owned(),
            installations: IndexMap::new(),
            active_installation: None,
            knobs: IndexMap::new(),
            detected_machine: None,
        }
    }

    /// The stored knob value, if set.
    pub fn knob(&self, name: &str) -> Option<&str> {
        self.knobs.get(name).map(String::as_str)
    }

    /// The effective knob value: stored → built-in/derived default (§5).
    pub fn knob_or_default(&self, name: &str) -> Option<String> {
        if let Some(v) = self.knob(name) {
            return Some(v.to_owned());
        }
        match name {
            "mail-type" => Some("all".to_owned()),
            "user" => std::env::var("USER").or_else(|_| std::env::var("LOGNAME")).ok(),
            "email" => git_config_email(),
            _ => None,
        }
    }

    pub fn set_knob(&mut self, name: &str, value: String) {
        self.knobs.insert(name.to_owned(), value);
    }

    /// Deserialize from `path`, or build a fresh one if the file doesn't
    /// exist. Enforces the §2.1 schema guard.
    fn read_from(path: &Path) -> Res<Self> {
        if !path.exists() {
            return Ok(Self::new());
        }

        let contents = fs::read_to_string(path)
            .with_context(|| format!("Failed to read database from {}", path.display()))?;
        let db: Database = serde_json::from_str(&contents)
            .with_context(|| format!("Failed to parse database at {}", path.display()))?;

        if db.schema > SCHEMA {
            bail!(
                "{} has schema {} but this cactup ({}) understands at most schema {SCHEMA}. \
                 It was written by a newer cactup; please upgrade.",
                path.display(),
                db.schema,
                crate::VERSION
            );
        }
        Ok(db)
    }

    /// Write to `path` as pretty JSON, atomically (temp file + rename), so a
    /// kill mid-write can never leave a torn database.
    fn persist(&self, path: &Path) -> Res<()> {
        let contents =
            serde_json::to_string_pretty(self).with_context(|| "Failed to serialize database")?;
        let dir = path.parent().expect("database path has a parent");
        let mut temp = tempfile::NamedTempFile::new_in(dir)
            .with_context(|| format!("Failed to create temp file in {}", dir.display()))?;
        temp.write_all(contents.as_bytes())
            .and_then(|()| temp.as_file().sync_all())
            .with_context(|| "Failed to write database temp file")?;
        temp.persist(path)
            .with_context(|| format!("Failed to move database into place at {}", path.display()))?;
        Ok(())
    }
}

/// Handle to the on-disk global DB. Cheap to construct; owns no lock. All
/// access goes through [`Db::read`] / [`Db::update`].
pub struct Db {
    path: PathBuf,
    lock_path: PathBuf,
}

impl Db {
    /// The production handle under `~/.cactup`.
    pub fn open() -> Res<Db> {
        Ok(Self::in_dir(&crate::CACTUP_ROOT))
    }

    /// A handle rooted at an explicit directory (tests, tools).
    pub fn in_dir(dir: &Path) -> Db {
        Db {
            path: dir.join("database.json"),
            lock_path: dir.join("database.lock"),
        }
    }

    /// Take a consistent snapshot: lock, read, release. A snapshot is for
    /// reading only — never persist one (that would be the stale-clobber §2.3
    /// forbids); mutate through [`Db::update`] instead.
    pub fn read(&self) -> Res<Database> {
        self.ensure_dir()?;
        let _lock = LinkLock::acquire(&self.lock_path)?;
        Database::read_from(&self.path)
    }

    /// The §2.3 field-scoped read-modify-write: acquire the lock, re-read the
    /// on-disk DB, apply `mutate` (touching only the keys the caller owns),
    /// persist, release. Callers must keep the closure brief — long-running
    /// work happens strictly outside.
    pub fn update<T>(&self, mutate: impl FnOnce(&mut Database) -> Res<T>) -> Res<T> {
        self.ensure_dir()?;
        let _lock = LinkLock::acquire(&self.lock_path)?;
        let mut db = Database::read_from(&self.path)?;
        let result = mutate(&mut db)?;
        db.schema = SCHEMA;
        db.cactup_version = crate::VERSION.to_owned();
        db.persist(&self.path)?;
        Ok(result)
    }

    fn ensure_dir(&self) -> Res<()> {
        let dir = self.path.parent().expect("database path has a parent");
        fs::create_dir_all(dir)
            .with_context(|| format!("Failed to create cactup directory {}", dir.display()))
    }
}

fn git_config_email() -> Option<String> {
    let mut command = std::process::Command::new("git");
    command.args(["config", "user.email"]);
    crate::shell::trace_command(&command);
    let output = command.output().ok()?;
    if !output.status.success() {
        return None;
    }
    let email = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    (!email.is_empty()).then_some(email)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_is_a_scoped_rmw_and_read_sees_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::in_dir(dir.path());

        db.update(|d| {
            d.active_installation = Some("et".to_owned());
            Ok(())
        })
        .unwrap();
        // A second, field-scoped update must not clobber the first field.
        db.update(|d| {
            d.set_knob("queue", "local".to_owned());
            Ok(())
        })
        .unwrap();

        let snapshot = db.read().unwrap();
        assert_eq!(snapshot.active_installation.as_deref(), Some("et"));
        assert_eq!(snapshot.knob("queue"), Some("local"));
        assert_eq!(snapshot.schema, SCHEMA);
        // No lock is left behind by read/update.
        assert!(!dir.path().join("database.lock").exists());
    }

    #[test]
    fn schema_guard_refuses_newer_reads_older() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::in_dir(dir.path());

        let newer = format!(
            r#"{{ "schema": {}, "cactup-version": "99.0.0", "installations": {{}} }}"#,
            SCHEMA + 1
        );
        fs::write(dir.path().join("database.json"), newer).unwrap();
        let err = format!("{:#}", db.read().unwrap_err());
        assert!(err.contains("upgrade"), "unexpected error: {err}");

        // A pre-schema file (like today's) reads fine and defaults to SCHEMA.
        let legacy = r#"{
            "cactup-version": "0.1.0",
            "installations": { "et": { "alias": "et", "release": null, "path": "/x" } },
            "active-installation": "et"
        }"#;
        fs::write(dir.path().join("database.json"), legacy).unwrap();
        let snapshot = db.read().unwrap();
        assert_eq!(snapshot.schema, SCHEMA);
        assert_eq!(snapshot.installations["et"].path, "/x");
        assert!(snapshot.knobs.is_empty());
        // A custom installation registered before cactup recorded the thornlist
        // it came from still reads; `show`/`list` just cannot name the source.
        assert_eq!(snapshot.installations["et"].thornlist, None);
    }

    /// A custom installation's thornlist provenance survives a write/read
    /// round-trip — it is the only thing that identifies such an installation,
    /// since it has no release name.
    #[test]
    fn custom_installation_records_its_thornlist() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::in_dir(dir.path());
        db.update(|d| {
            d.installations.insert(
                "custom".to_owned(),
                CactusInstallation {
                    alias: "custom".to_owned(),
                    release: None,
                    path: "/x".to_owned(),
                    thornlist: Some("/home/u/lists/mine.th".to_owned()),
                },
            );
            Ok(())
        })
        .unwrap();

        let snapshot = db.read().unwrap();
        let entry = &snapshot.installations["custom"];
        assert_eq!(entry.release, None);
        assert_eq!(entry.thornlist.as_deref(), Some("/home/u/lists/mine.th"));
        // Release installs stay clean: the key is skipped when unset, so
        // existing database.json files gain nothing.
        let raw = fs::read_to_string(dir.path().join("database.json")).unwrap();
        assert!(raw.contains("mine.th"), "{raw}");
    }

    #[test]
    fn knob_defaults() {
        let db = Database::new();
        assert_eq!(db.knob_or_default("mail-type").as_deref(), Some("all"));
        assert_eq!(db.knob_or_default("allocation"), None);
        let mut db = db;
        db.set_knob("mail-type", "none".to_owned());
        assert_eq!(db.knob_or_default("mail-type").as_deref(), Some("none"));
    }
}
