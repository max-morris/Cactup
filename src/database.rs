//! The global database, `~/.cactup/database.json` (spec §2.1) — the ONLY
//! global mutable state: installations, the active installation, knobs (§5),
//! and the detected-machine cache with its verification stamp (§4.3). Config
//! metadata and simulation state live on disk next to what they describe
//! (D4, D6), never here.
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
use std::sync::{LazyLock, OnceLock};

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

/// A knob cactup recognizes (§5): its name plus how to check a value on
/// the way in and how to show the stored form on the way out. Most knobs
/// are free-form ([`KnobSpec::free_form`]); a knob with a closed value set
/// supplies its own `validate`/`render` (see `wisdom-frequency`, which
/// stores an ordinal but always renders the name).
pub struct KnobSpec {
    pub name: &'static str,
    /// Validate + normalize a user-supplied value into the stored form.
    pub validate: fn(&str) -> Res<String>,
    /// Render the stored form for display.
    pub render: fn(&str) -> String,
}

impl KnobSpec {
    const fn free_form(name: &'static str) -> Self {
        Self { name, validate: |v| Ok(v.to_owned()), render: str::to_owned }
    }
}

/// Knobs cactup recognizes (§5). `user`/`email` are normally derived
/// (`$USER`, `git config user.email`) but may be overridden as knobs.
pub const KNOWN_KNOBS: &[KnobSpec] = &[
    KnobSpec::free_form("allocation"),
    KnobSpec::free_form("mail"),
    KnobSpec::free_form("mail-type"),
    KnobSpec::free_form("queue"),
    KnobSpec::free_form("user"),
    KnobSpec::free_form("email"),
    KnobSpec {
        name: "wisdom-frequency",
        validate: crate::commands::wisdom::validate_frequency,
        render: crate::commands::wisdom::render_frequency,
    },
    KnobSpec {
        name: "wisdom-kind",
        validate: crate::commands::wisdom::validate_kind,
        render: str::to_owned,
    },
];

/// The spec for a knob name, if cactup recognizes it.
pub fn knob_spec(name: &str) -> Option<&'static KnobSpec> {
    KNOWN_KNOBS.iter().find(|s| s.name == name)
}

/// Check a knob identifier (§5): kebab-case — lowercase `a-z` only, digits
/// allowed after the first character, dashes allowed anywhere but first and
/// last. Every standard knob name satisfies this; custom knobs must.
pub fn validate_knob_name(name: &str) -> Res<()> {
    let bytes = name.as_bytes();
    let ok = !bytes.is_empty()
        && bytes[0].is_ascii_lowercase()
        && bytes[bytes.len() - 1] != b'-'
        && bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-');
    if !ok {
        bail!(
            "\"{name}\" is not a valid knob name: lowercase letters a-z, digits after the first \
             character, and dashes anywhere but first or last"
        );
    }
    // `cactup knob <subcommand>` shares the positional slot with knob names,
    // so a knob by that name could be created but never read or deleted.
    if RESERVED_KNOB_NAMES.contains(&name) {
        bail!("\"{name}\" is a `cactup knob` subcommand and cannot be used as a knob name");
    }
    Ok(())
}

/// Knob names taken by `cactup knob` subcommands (§5).
const RESERVED_KNOB_NAMES: &[&str] = &["delete"];

/// Turn a user-supplied `-K`/`cactup knob` value into the stored form: a
/// standard knob's `validate` (which rejects bad values and normalizes),
/// verbatim for a custom knob (after checking the name).
pub fn knob_stored_form(name: &str, value: &str) -> Res<String> {
    match knob_spec(name) {
        Some(spec) => (spec.validate)(value),
        None => {
            validate_knob_name(name)?;
            Ok(value.to_owned())
        }
    }
}

/// The stored form for display: a standard knob's `render`, verbatim for a
/// custom knob.
pub fn knob_display_form(name: &str, stored: &str) -> String {
    match knob_spec(name) {
        Some(spec) => (spec.render)(stored),
        None => stored.to_owned(),
    }
}

/// The `-K NAME=VALUE` overlay for this process (§5.1): stored-form values,
/// applied to every snapshot [`Db::read`] hands out and never persisted.
/// Installed once by `main` before the first read.
static KNOB_OVERRIDES: OnceLock<IndexMap<String, String>> = OnceLock::new();

/// Install the `-K` overlay. A second call is a no-op (`main` calls it once).
pub fn set_knob_overrides(overrides: IndexMap<String, String>) {
    let _ = KNOB_OVERRIDES.set(overrides);
}

/// The `-K` overlay, empty when none was given.
pub fn knob_overrides() -> &'static IndexMap<String, String> {
    static EMPTY: LazyLock<IndexMap<String, String>> = LazyLock::new(IndexMap::new);
    KNOB_OVERRIDES.get().unwrap_or(&EMPTY)
}

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
    /// What the tree is on *now*, when an `installation refetch` with an
    /// explicit source moved it away from its install-time provenance
    /// (§2.1). `release`/`thornlist` above are never rewritten — they record
    /// how the installation was created; this pair records where a refetch
    /// took it. Absent until the first explicit-source refetch.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_release: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub current_thornlist: Option<String>,
    /// Repos the last refetch did not fetch — skipped as dirty, or failed —
    /// and why. Non-empty means the tree only *partially* conforms to the
    /// recorded thornlist above: those thorns on disk still hold what they
    /// held before. Recorded because the thornlist is adopted on disk even
    /// when some repos are skipped or a fetch errors, so the DB would
    /// otherwise assert a conformance the tree does not have. Cleared by any
    /// refetch that fetches every repo the list names (e.g. `refetch -f`).
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub unfetched_repos: IndexMap<String, UnfetchedRepo>,
}

/// A repo the last refetch did not fetch, and why: a deliberate skip (the
/// repo has local state) or an outright failure (the fetch was attempted and
/// errored). The two are not the same kind of fact — a skip is a supported
/// workflow the user may have intended, a failure is not — so callers must
/// keep them distinguishable rather than lumping both into one count.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub struct UnfetchedRepo {
    /// Why it did not get fetched — a deliberate skip or an outright error.
    pub reason: UnfetchedReason,
    /// The thorns this repo backs. For a failed download/external component,
    /// which backs no other thorn, this is the component itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub thorns: Vec<String>,
    /// For `Skipped`, the dirty-state description (`local commits`, …); for
    /// `Failed`, the error text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum UnfetchedReason {
    /// Left alone deliberately: the repo has local state (modifications,
    /// local commits, a switched branch, an in-progress rebase, …). A
    /// supported workflow, not an error — `--overwrite-modified`/`-f`
    /// fetches over it if that was not what the user wanted.
    Skipped,
    /// The fetch was attempted and errored. The user asked for this repo and
    /// did not get it, so this is an error state, reported more loudly.
    Failed,
}

impl CactusInstallation {
    /// Thorns whose on-disk contents do not conform to the recorded
    /// thornlist, i.e. the thorns backed by every unfetched repo.
    pub fn unfetched_thorn_count(&self) -> usize {
        self.unfetched_repos.values().map(|r| r.thorns.len()).sum()
    }

    /// Repos that errored out — an error state, not a choice, reported more
    /// loudly than a skip.
    pub fn failed_repo_count(&self) -> usize {
        self.unfetched_repos.values().filter(|r| r.reason == UnfetchedReason::Failed).count()
    }

    /// Repos deliberately left alone because they hold local state.
    pub fn skipped_repo_count(&self) -> usize {
        self.unfetched_repos.values().filter(|r| r.reason == UnfetchedReason::Skipped).count()
    }
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
    /// The discovered machine, stamped with where it was last verified
    /// (§4.3). Absent until discovery first succeeds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detected: Option<DetectedMachine>,
}

/// The discovery cache (§4.3): which machine this `~/.cactup` resolved to,
/// and the host and login session that last confirmed it. A command whose
/// hostname and session both match the stamp trusts the name outright;
/// anything else re-verifies it against the machine's own matcher first, so
/// a `~/.cactup` shared between clusters cannot carry one cluster's machine
/// onto another unnoticed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub struct DetectedMachine {
    pub name: String,
    /// The discovery hostname (`--hostname` → `~/.hostname` → system) the
    /// name was last verified for.
    pub hostname: String,
    /// The login session (`<session-id>:<leader-start-time>`, from /proc)
    /// that verification happened in; absent when /proc has no answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

impl DetectedMachine {
    /// Was this record stamped for exactly this host and login session? An
    /// unknown session on either side never counts as current: it costs one
    /// regex (or one python3) to be sure, and being wrong costs a job.
    pub fn is_current(&self, hostname: &str, session: Option<&str>) -> bool {
        self.hostname == hostname && session.is_some() && self.session.as_deref() == session
    }
}

impl Database {
    pub fn new() -> Self {
        Self {
            schema: SCHEMA,
            cactup_version: crate::VERSION.to_owned(),
            installations: IndexMap::new(),
            active_installation: None,
            knobs: IndexMap::new(),
            detected: None,
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
            // Stored form (§5): frequency is an ordinal, 2 = "normal".
            "wisdom-frequency" => Some("2".to_owned()),
            "wisdom-kind" => Some("all".to_owned()),
            _ => None,
        }
    }

    pub fn set_knob(&mut self, name: &str, value: String) {
        self.knobs.insert(name.to_owned(), value);
    }

    /// The stored custom knobs (§5) — every entry that is not a standard
    /// knob — in insertion order.
    pub fn custom_knobs(&self) -> impl Iterator<Item = (&str, &str)> {
        self.knobs
            .iter()
            .filter(|(name, _)| knob_spec(name).is_none())
            .map(|(name, value)| (name.as_str(), value.as_str()))
    }

    /// Lay the `-K` overlay (§5.1) over the stored knobs. Applied to read
    /// snapshots only — a `Db::update` closure sees the disk state, so an
    /// override is never written back.
    fn apply_knob_overrides(&mut self) {
        for (name, value) in knob_overrides() {
            self.knobs.insert(name.clone(), value.clone());
        }
    }

    /// The effective knob values as `@KNOB(name)@` sees them (§6.1): every
    /// standard knob that has a stored or derived value, in display form,
    /// plus every custom knob. Frozen into restart/build/test metadata at
    /// submit time so the compute node never reads the DB (D11).
    pub fn knob_snapshot(&self) -> IndexMap<String, String> {
        let mut out = IndexMap::new();
        for spec in KNOWN_KNOBS {
            if let Some(stored) = self.knob_or_default(spec.name) {
                out.insert(spec.name.to_owned(), (spec.render)(&stored));
            }
        }
        for (name, value) in self.custom_knobs().filter(|(_, v)| !v.is_empty()) {
            out.insert(name.to_owned(), value.to_owned());
        }
        out
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
        let mut db = Database::read_from(&self.path)?;
        // The `-K` overlay (§5.1) lives on snapshots only: `update` below
        // re-reads the disk state, so an override can never be persisted.
        db.apply_knob_overrides();
        Ok(db)
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

    /// The stamped record round-trips, and a DB written before the stamp
    /// existed (a bare `detected-machine` string) reads as "not detected" —
    /// discovery simply runs again; no migration (§2.1).
    #[test]
    fn detected_machine_record_round_trips_and_old_key_reads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::in_dir(dir.path());
        fs::write(
            dir.path().join("database.json"),
            r#"{ "cactup-version": "0.1.0", "detected-machine": "mike" }"#,
        )
        .unwrap();
        assert_eq!(db.read().unwrap().detected, None);

        let record = DetectedMachine {
            name: "mike".into(),
            hostname: "mike1.hpc.lsu.edu".into(),
            session: Some("7:42".into()),
        };
        db.update(|d| {
            d.detected = Some(record.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(db.read().unwrap().detected, Some(record.clone()));
        let text = fs::read_to_string(dir.path().join("database.json")).unwrap();
        assert!(text.contains("\"detected\": {") && !text.contains("detected-machine"), "{text}");
        assert!(record.is_current("mike1.hpc.lsu.edu", Some("7:42")));
        assert!(!record.is_current("mike2.hpc.lsu.edu", Some("7:42")));
        assert!(!record.is_current("mike1.hpc.lsu.edu", Some("8:42")));
        assert!(!record.is_current("mike1.hpc.lsu.edu", None));
        let unknown = DetectedMachine { session: None, ..record };
        assert!(!unknown.is_current("mike1.hpc.lsu.edu", None));
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
                    current_release: None,
                    current_thornlist: None,
                    unfetched_repos: IndexMap::new(),
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

    /// `unfetched_repos` — the partial-conformance marker — survives a
    /// write/read round-trip with its order and contents intact (a `Skipped`
    /// and a `Failed` entry alike), and stays invisible in the JSON (no key
    /// emitted) when empty, so pre-existing `database.json` files gain
    /// nothing from this field.
    #[test]
    fn unfetched_repos_records_partial_conformance() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::in_dir(dir.path());
        let mut unfetched = IndexMap::new();
        unfetched.insert(
            "carpetx".to_owned(),
            UnfetchedRepo {
                reason: UnfetchedReason::Skipped,
                thorns: vec!["CarpetX/Algo".to_owned(), "CarpetX/BoxUtils".to_owned()],
                detail: Some("local commits".to_owned()),
            },
        );
        unfetched.insert(
            "openpmd-api".to_owned(),
            UnfetchedRepo {
                reason: UnfetchedReason::Failed,
                thorns: vec!["ExternalLibraries/openPMD".to_owned()],
                detail: Some("connection reset by peer".to_owned()),
            },
        );
        db.update(|d| {
            d.installations.insert(
                "et".to_owned(),
                CactusInstallation {
                    alias: "et".to_owned(),
                    release: Some("ET_2026_05".to_owned()),
                    path: "/x".to_owned(),
                    thornlist: None,
                    current_release: None,
                    current_thornlist: None,
                    unfetched_repos: unfetched,
                },
            );
            Ok(())
        })
        .unwrap();

        let snapshot = db.read().unwrap();
        let entry = &snapshot.installations["et"];
        let carpetx = &entry.unfetched_repos["carpetx"];
        assert_eq!(carpetx.reason, UnfetchedReason::Skipped);
        assert_eq!(carpetx.thorns, vec!["CarpetX/Algo".to_owned(), "CarpetX/BoxUtils".to_owned()]);
        assert_eq!(carpetx.detail.as_deref(), Some("local commits"));
        let openpmd = &entry.unfetched_repos["openpmd-api"];
        assert_eq!(openpmd.reason, UnfetchedReason::Failed);
        assert_eq!(openpmd.thorns, vec!["ExternalLibraries/openPMD".to_owned()]);
        assert_eq!(openpmd.detail.as_deref(), Some("connection reset by peer"));
        assert_eq!(entry.unfetched_thorn_count(), 3);
        assert_eq!(entry.failed_repo_count(), 1);
        assert_eq!(entry.skipped_repo_count(), 1);
        let raw = fs::read_to_string(dir.path().join("database.json")).unwrap();
        assert!(raw.contains("unfetched-repos"), "{raw}");

        // A separate DB with only an empty map: the key is skipped entirely
        // — existing database.json files gain nothing from this field.
        let dir2 = tempfile::tempdir().unwrap();
        let db2 = Db::in_dir(dir2.path());
        db2.update(|d| {
            d.installations.insert(
                "clean".to_owned(),
                CactusInstallation {
                    alias: "clean".to_owned(),
                    release: Some("ET_2026_05".to_owned()),
                    path: "/y".to_owned(),
                    thornlist: None,
                    current_release: None,
                    current_thornlist: None,
                    unfetched_repos: IndexMap::new(),
                },
            );
            Ok(())
        })
        .unwrap();
        let raw2 = fs::read_to_string(dir2.path().join("database.json")).unwrap();
        assert!(!raw2.contains("unfetched-repos"), "{raw2}");
    }

    #[test]
    fn knob_defaults() {
        let db = Database::new();
        assert_eq!(db.knob_or_default("mail-type").as_deref(), Some("all"));
        assert_eq!(db.knob_or_default("allocation"), None);
        // Wisdom defaults (§5): stored forms — the frequency ordinal 2 is
        // rendered as "normal" by the KnobSpec, and kind defaults to "all".
        assert_eq!(db.knob_or_default("wisdom-frequency").as_deref(), Some("2"));
        assert_eq!(db.knob_or_default("wisdom-kind").as_deref(), Some("all"));
        let mut db = db;
        db.set_knob("mail-type", "none".to_owned());
        assert_eq!(db.knob_or_default("mail-type").as_deref(), Some("none"));
    }

    #[test]
    fn knob_names_are_kebab_case() {
        for ok in ["a", "queue", "mail-type", "kadath-initial-data", "x1", "a-1-b", "wisdom-frequency"] {
            validate_knob_name(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        for bad in ["", "1a", "-a", "a-", "Queue", "mail_type", "a b", "ünï", "a.b", "a/b", "delete"] {
            assert!(validate_knob_name(bad).is_err(), "{bad:?} accepted");
        }
        // Every standard knob passes its own rule.
        for spec in KNOWN_KNOBS {
            validate_knob_name(spec.name).unwrap();
        }
    }

    #[test]
    fn stored_and_display_forms() {
        // Standard knobs go through their spec both ways.
        assert_eq!(knob_stored_form("wisdom-frequency", "chatty").unwrap(), "3");
        assert_eq!(knob_display_form("wisdom-frequency", "3"), "chatty");
        assert!(knob_stored_form("wisdom-frequency", "loud").is_err());
        // Custom knobs are verbatim, but the name must be valid.
        assert_eq!(knob_stored_form("kadath-initial-data", "/x y").unwrap(), "/x y");
        assert_eq!(knob_display_form("kadath-initial-data", "/x y"), "/x y");
        assert!(knob_stored_form("Bad", "v").is_err());
    }

    #[test]
    fn knob_snapshot_covers_standard_and_custom() {
        let mut db = Database::new();
        db.set_knob("allocation", "hpc_xxx".to_owned());
        db.set_knob("wisdom-frequency", "3".to_owned());
        db.set_knob("kadath-initial-data", "/scratch/id.info".to_owned());
        let custom: Vec<_> = db.custom_knobs().collect();
        assert_eq!(custom, [("kadath-initial-data", "/scratch/id.info")]);
        let snap = db.knob_snapshot();
        // Stored, derived-default and custom values, all in display form;
        // knobs with no value at all are absent.
        assert_eq!(snap["allocation"], "hpc_xxx");
        assert_eq!(snap["mail-type"], "all");
        assert_eq!(snap["wisdom-frequency"], "chatty");
        assert_eq!(snap["kadath-initial-data"], "/scratch/id.info");
        assert!(!snap.contains_key("mail"));
        assert!(!snap.contains_key("queue"));
        // An empty value (`cactup knob x ""`) is no value: it leaves the
        // snapshot, so @KNOB(…)@ sees it as unset rather than as "".
        db.set_knob("kadath-initial-data", String::new());
        assert!(!db.knob_snapshot().contains_key("kadath-initial-data"));
    }
}
